use std::collections::BTreeMap;
use std::fs;
use tchelvis::assets::{HERDR_LAYOUT_JSON, HERDR_MOD_MAIN_JS};
use tchelvis::config::{CockpitConfig, CockpitPaths};
use tchelvis::correlate::{
    build_initial_agent_row, extract_slack_urls, extract_ticket_keys, resolve_git_workspace,
    should_include_terminal,
};
use tchelvis::daemon::{collect_once, query_socket_once, DaemonLock};
use tchelvis::doctor::run_doctor;
use tchelvis::herdr::{
    parse_hc_event_line, FleetTracker, HcForeground, HcStatus, HcTerminal, HerdrClient,
};
use tchelvis::logging::redact_secrets;
use tchelvis::model::{
    sanitize_terminal_text, AgentKind, AgentLifecycle, AgentRow, AttentionState, BlockedKind,
    CiCheckItem, CiSummary, CockpitSnapshot, CorrelatedContext, CorrelationTier, DeployReleaseRef,
    FleetSummary, FleetSummaryProjection, InspectResponse, IpcOp, IpcRequest, IssueRef,
    ProviderHealth, ProviderId, ProviderStatus, PullRequestRef, RemoteRepo, ReviewSummary,
    RunbookRef, SlackThreadRef, WorkspaceInfo, MAX_DOC_SNAPSHOT_BYTES, SNAPSHOT_SCHEMA_VERSION,
};
use tchelvis::providers::runbooks::discover_and_match_runbooks;
use tchelvis::providers::{is_below_quota_reserve_floor, ProviderCoordinator, TokenBucket};
use tchelvis::setup::{run_setup, run_uninstall, verify_installation, SetupOptions};
use tchelvis::store::{CacheStore, CachedEntity, QuotaRecord};
use tchelvis::supervisor::state::{
    AutoReplyAuditEntry, LifecycleState, PendingOperatorAction, SupervisorMode, Urgency,
};
use tchelvis::supervisor::tui::{AgentCardView, DesktopModSnapshot};
use tchelvis::supervisor::watcher::SupervisorRuntimeStatus;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

#[test]
fn test_integration_hc_events_and_hysteresis_rollup() {
    let mut tracker = FleetTracker::new();
    let snap_line = r#"{"channel":null,"seq":10,"terminals":[{"address":"t1","name":"main-agent","state":"live","reported":true,"status":{"state":"working"},"turn":3,"cwd":"/workspace","foreground":{"argv":["claude"],"cwd":"/workspace","name":"claude","pid":42}},{"address":"t2","name":"sub-agent","parent":"t1","state":"live","reported":true,"status":{"state":"blocked","kind":"auth","msg":"Login needed"},"status_ms":500,"turn":1,"cwd":"/workspace"}]}"#;
    let item = parse_hc_event_line(snap_line).unwrap();
    assert!(tracker.apply_stream_item(item, 1_000));

    let blocked_map = tracker.blocked_children_map(1_000);
    assert_eq!(blocked_map.get("t1"), Some(&vec!["t2".to_string()]));
    assert_eq!(
        tracker.effective_attention("t2", 1_000),
        AttentionState::Blocked {
            kind: BlockedKind::Auth,
            message: Some("Login needed".into()),
            since_ms: 500,
        }
    );

    // Clear t2 to working at 1200ms -> held until 2700ms unless turn increments
    let clear_ev = parse_hc_event_line(
        r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"working"},"seq":11,"terminal":"t2","type":"status"}}"#,
    )
    .unwrap();
    tracker.apply_stream_item(clear_ev, 1_200);
    assert!(matches!(
        tracker.effective_attention("t2", 2_000),
        AttentionState::Blocked { .. }
    ));

    // Turn increment immediately clears held Blocked state
    let turn_ev = parse_hc_event_line(
        r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"turn":2},"seq":12,"terminal":"t2","type":"turn"}}"#,
    )
    .unwrap();
    tracker.apply_stream_item(turn_ev, 2_100);
    assert!(matches!(
        tracker.effective_attention("t2", 2_100),
        AttentionState::Working { .. }
    ));
}

#[test]
fn test_integration_4_tier_correlation_and_plain_shell_filter() {
    // Plain shell with reported=false and no cockpit.* labels is excluded by default
    let plain_shell = HcTerminal {
        address: "t9".into(),
        name: Some("bash".into()),
        state: "live".into(),
        reported: false,
        argv: vec!["/bin/bash".into()],
        ..Default::default()
    };
    assert!(!should_include_terminal(&plain_shell, false));
    assert!(should_include_terminal(&plain_shell, true));

    // Agent on feature branch with ticket key and linked worktree
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let git_dir = repo_dir.join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    fs::write(
        git_dir.join("HEAD"),
        "ref: refs/heads/feat/ENG-777-oauth-UTF-8\n",
    )
    .unwrap();
    fs::write(
        git_dir.join("config"),
        "[remote \"origin\"]\n\turl = https://github.com/acme/api.git\n",
    )
    .unwrap();

    let mut labels = BTreeMap::new();
    labels.insert(
        "cockpit.slack".into(),
        "https://acme.slack.com/archives/C999/p1700000111222333".into(),
    );

    let term = HcTerminal {
        address: "t7".into(),
        name: Some("auth-work".into()),
        state: "live".into(),
        reported: true,
        cwd: Some(repo_dir.to_string_lossy().to_string()),
        foreground: Some(HcForeground {
            argv: vec!["claude".into()],
            cwd: Some(repo_dir.to_string_lossy().to_string()),
            name: Some("claude".into()),
            pid: Some(900),
        }),
        status: Some(HcStatus {
            state: "blocked".into(),
            kind: Some("question".into()),
            msg: Some("Which callback URI?".into()),
            app: Some("claude".into()),
        }),
        labels,
        ..Default::default()
    };

    assert!(should_include_terminal(&term, false));
    let row = build_initial_agent_row(&term, term.raw_attention_state(1_000), Vec::new(), 1);
    assert_eq!(row.agent_kind, AgentKind::ClaudeCode);
    assert_eq!(row.correlation_tier, CorrelationTier::Exact);
    assert_eq!(row.context.candidate_issues, vec!["ENG-777".to_string()]);
    assert_eq!(
        extract_ticket_keys("UTF-8 SHA-256 ISO-8601 RFC-9110 CVE-2026 HTTP-2"),
        Vec::<String>::new()
    );
    let slack_links = extract_slack_urls(term.labels.get("cockpit.slack").unwrap());
    assert_eq!(slack_links.len(), 1);
    assert_eq!(slack_links[0].0, "acme");
    assert_eq!(slack_links[0].1, "C999");
    assert_eq!(slack_links[0].2, "1700000111.222333");

    let ws = resolve_git_workspace(repo_dir.to_str().unwrap());
    assert_eq!(ws.git_branch.as_deref(), Some("feat/ENG-777-oauth-UTF-8"));
}

#[test]
fn test_integration_store_quota_and_runbook_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("state.db");
    let store = CacheStore::open(&db_path).unwrap();

    store
        .put_cached(&CachedEntity {
            provider: ProviderId::Jira.as_str().into(),
            resource_key: "jira:NOPE-1".into(),
            payload_json: "null".into(),
            etag: None,
            fetched_at: 1_000,
            expires_at: 1_900,
            is_negative: true,
        })
        .unwrap();
    let neg = store
        .get_cached(ProviderId::Jira, "jira:NOPE-1")
        .unwrap()
        .unwrap();
    assert!(neg.is_negative);

    store
        .update_quota(&QuotaRecord {
            provider: ProviderId::Github.as_str().into(),
            remaining: Some(500),
            limit_total: Some(5000),
            reset_at: Some(9_999_999),
            cooldown_until: None,
            last_error_kind: None,
            last_error_msg: None,
            last_success_at: Some(1_000),
        })
        .unwrap();
    let q = store.get_quota(ProviderId::Github).unwrap().unwrap();
    assert!(is_below_quota_reserve_floor(q.remaining, q.limit_total));

    let mut tb = TokenBucket::new(1.0, 0.5);
    assert!(tb.try_acquire(1.0));
    assert!(!tb.try_acquire(1.0));

    // Runbooks with .gitignore exclusion
    fs::write(tmp.path().join(".gitignore"), "ignored_runbooks\n").unwrap();
    let ignored_dir = tmp.path().join("ignored_runbooks");
    fs::create_dir_all(&ignored_dir).unwrap();
    fs::write(ignored_dir.join("RUNBOOK.md"), "# Ignored\n").unwrap();
    fs::write(
        tmp.path().join("RUNBOOK.md"),
        "---\ntitle: Root Service Runbook\ntags: [ENG-777]\n---\nCheck logs.\n",
    )
    .unwrap();

    let rbs = discover_and_match_runbooks(
        Some(tmp.path().to_str().unwrap()),
        tmp.path().to_str().unwrap(),
        Some("feat/ENG-777"),
        &["ENG-777".into()],
        &[],
        None,
        tmp.path(),
    );
    assert_eq!(rbs.len(), 1);
    assert_eq!(rbs[0].title, "Root Service Runbook");
}

#[tokio::test]
async fn test_integration_uds_ipc_and_setup_doctor_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let paths = CockpitPaths {
        home_dir: home.to_path_buf(),
        config_dir: home.join(".config/tchelvis"),
        data_dir: home.join(".local/share/tchelvis"),
        state_dir: home.join(".local/state/tchelvis"),
        runtime_dir: home.join(".local/state/tchelvis/run"),
    };
    paths.ensure_dirs().unwrap();

    // Seed user config files
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".config/opencode")).unwrap();
    fs::create_dir_all(home.join(".pi/agent")).unwrap();
    fs::create_dir_all(home.join(".config/herdr-app")).unwrap();

    let orig_claude = "{\n  \"model\": \"sonnet\"\n}\n";
    let orig_herdr = "{\n  \"default_layout\": \"default\"\n}\n";
    let orig_shared = "{\n  \"layout\": \"main\"\n}\n";
    fs::write(home.join(".claude/settings.json"), orig_claude).unwrap();
    fs::write(home.join(".config/herdr-app/settings.json"), orig_herdr).unwrap();
    fs::write(home.join(".config/herdr-app/shared.json"), orig_shared).unwrap();

    // Setup with --set-default-layout and --systemd
    let opts = SetupOptions {
        set_default_layout: true,
        systemd: true,
        ..Default::default()
    };
    let setup_rep = run_setup(&paths, &opts).unwrap();
    assert!(setup_rep.verified_ok);
    assert!(home
        .join(".config/herdr-app/mods/tchelvis/mod.json")
        .exists());
    let shared_after_setup: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.join(".config/herdr-app/shared.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        shared_after_setup.get("layout").and_then(|v| v.as_str()),
        Some("tchelvis")
    );
    assert!(home.join(".config/opencode/plugins/tchelvis.js").exists());
    assert!(home.join(".pi/agent/extensions/tchelvis.js").exists());
    assert!(home.join(".config/systemd/user/tchelvis.service").exists());
    assert!(verify_installation(&paths).unwrap().verified_ok);

    // Run doctor and generate redacted bundle
    let cfg = CockpitConfig::default();
    let bundle_path = home.join("diag-bundle.json");
    let doc_rep = run_doctor(&cfg, &paths, Some(&bundle_path)).await.unwrap();
    assert!(bundle_path.exists());
    assert!(doc_rep
        .checks
        .iter()
        .any(|c| c.name == "hooks" && c.status == "ok"));
    assert!(doc_rep
        .checks
        .iter()
        .any(|c| c.name == "herdr_mod" && c.status == "ok"));

    // Uninstall and verify exact rollback
    run_uninstall(&paths, false, false, None).unwrap();
    assert_eq!(
        fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
        orig_claude
    );
    assert_eq!(
        fs::read_to_string(home.join(".config/herdr-app/settings.json")).unwrap(),
        orig_herdr
    );
    assert_eq!(
        fs::read_to_string(home.join(".config/herdr-app/shared.json")).unwrap(),
        orig_shared
    );
    assert!(!home.join(".config/opencode/plugins/tchelvis.js").exists());
    assert!(!home.join(".pi/agent/extensions/tchelvis.js").exists());

    // Test Unix socket IPC server/client and DaemonLock
    let lock = DaemonLock::try_acquire(&paths.lock_file()).unwrap();
    assert!(lock.is_some());

    let sock_path = paths.socket_file();
    let listener = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (rh, mut wh) = stream.into_split();
            let mut reader = BufReader::new(rh);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
            let req: IpcRequest = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(req.op, IpcOp::Status);
            let snap = CockpitSnapshot::default();
            let out = serde_json::to_string(&snap).unwrap() + "\n";
            let _ = wh.write_all(out.as_bytes()).await;
        }
    });

    let snap_resp: CockpitSnapshot = query_socket_once(
        &sock_path,
        &IpcRequest {
            op: IpcOp::Status,
            terminal: None,
            include_all: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(snap_resp.schema_version, 1);

    let proj = FleetSummaryProjection::from_snapshot(&snap_resp);
    assert!(serde_json::to_vec(&proj).unwrap().len() <= MAX_DOC_SNAPSHOT_BYTES);

    // Verify collect_once works against current environment or fallback
    let store = CacheStore::open(&paths.db_file()).unwrap();
    let herdr = HerdrClient::default();
    let coord = ProviderCoordinator::new();
    let live_snap = collect_once(&cfg, &paths, &store, &herdr, &coord, false, false)
        .await
        .unwrap();
    assert_eq!(live_snap.schema_version, 1);

    // Verify redaction and sanitization
    assert!(!redact_secrets("Bearer ghp_1234567890abcdef123456").contains("ghp_"));
    assert_eq!(
        sanitize_terminal_text("Hello \x1b]7501;state=blocked\x07World"),
        "Hello World"
    );
    assert!(HERDR_MOD_MAIN_JS.contains("export function mount"));
    assert!(HERDR_LAYOUT_JSON.contains("\"tchelvis\""));
    let _ = std::mem::size_of::<InspectResponse>();
}

fn write_fixture_if_changed(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == content {
            return;
        }
    }
    fs::write(path, content).unwrap();
}

#[test]
fn test_contract_fixtures_and_schema_version_match() {
    let expected_decl = format!("export const schemaVersion = {SNAPSHOT_SCHEMA_VERSION};");
    assert!(
        HERDR_MOD_MAIN_JS.contains(&expected_decl),
        "assets/herdr-app/mods/tchelvis/main.js must export `{expected_decl}`"
    );

    let mut providers = BTreeMap::new();
    providers.insert(
        ProviderId::Github,
        ProviderHealth {
            provider: ProviderId::Github,
            status: ProviderStatus::Ready,
            detail: Some("Authenticated via gh".into()),
            quota_remaining: Some(4800),
            quota_limit: Some(5000),
            quota_reset_at: Some(1_700_003_600),
            cooldown_until: None,
            last_success_at: Some(1_700_000_000),
        },
    );
    providers.insert(
        ProviderId::Linear,
        ProviderHealth {
            provider: ProviderId::Linear,
            status: ProviderStatus::Ready,
            detail: Some("API key configured".into()),
            ..Default::default()
        },
    );
    providers.insert(
        ProviderId::Slack,
        ProviderHealth {
            provider: ProviderId::Slack,
            status: ProviderStatus::Ready,
            detail: Some("Token configured".into()),
            ..Default::default()
        },
    );
    providers.insert(
        ProviderId::Runbooks,
        ProviderHealth {
            provider: ProviderId::Runbooks,
            status: ProviderStatus::Ready,
            detail: Some("Local runbooks".into()),
            ..Default::default()
        },
    );

    let t1 = AgentRow {
        address: "t1".into(),
        name: "auth-agent".into(),
        parent: None,
        agent_kind: AgentKind::ClaudeCode,
        lifecycle: AgentLifecycle::Live,
        attention_state: AttentionState::Blocked {
            kind: BlockedKind::Permission,
            message: Some("Approve bash command?".into()),
            since_ms: 1_700_000_000_000,
        },
        prompt_destructive: false,
        blocked_children: Vec::new(),
        active_children: vec!["t2".into()],
        turn: 4,
        reported: true,
        labels: BTreeMap::from([("cockpit.issue".into(), "ENG-42".into())]),
        workspace: WorkspaceInfo {
            cwd: "/workspace/acme-api".into(),
            repo_root: Some("/workspace/acme-api".into()),
            worktree_path: None,
            git_branch: Some("feat/ENG-42-oauth".into()),
            git_head_sha: Some("abcdef1234567890".into()),
            commit_subject: Some("Add OAuth callback".into()),
            remote_repo: Some(RemoteRepo {
                host: "github.com".into(),
                owner: "acme".into(),
                repo: "api".into(),
            }),
        },
        correlation_tier: CorrelationTier::Exact,
        provenance: Vec::new(),
        context: CorrelatedContext {
            pull_request: Some(PullRequestRef {
                provider: ProviderId::Github,
                number: 42,
                title: "Add OAuth callback".into(),
                state: "open".into(),
                is_draft: false,
                url: "https://github.com/acme/api/pull/42".into(),
                head_branch: "feat/ENG-42-oauth".into(),
                head_sha: Some("abcdef1234567890".into()),
                local_ahead: false,
                body_snippet: Some("Implements ENG-42 OAuth callback".into()),
            }),
            candidate_prs: Vec::new(),
            ci: Some(CiSummary {
                overall_state: "failing".into(),
                total: 3,
                passed: 1,
                failed: 1,
                pending: 1,
                failing_checks: vec!["cargo-test".into()],
                checks: vec![
                    CiCheckItem {
                        name: "cargo-test".into(),
                        status: "completed".into(),
                        conclusion: Some("failure".into()),
                        details_url: Some("https://github.com/acme/api/actions/runs/1".into()),
                    },
                    CiCheckItem {
                        name: "clippy".into(),
                        status: "completed".into(),
                        conclusion: Some("success".into()),
                        details_url: None,
                    },
                    CiCheckItem {
                        name: "e2e".into(),
                        status: "in_progress".into(),
                        conclusion: None,
                        details_url: None,
                    },
                ],
            }),
            review: Some(ReviewSummary {
                decision: "changes_requested".into(),
                approved_count: 1,
                changes_requested_count: 1,
                commented_count: 0,
                reviewers: vec!["alice".into()],
            }),
            deploy_release: Some(DeployReleaseRef {
                environment: Some("staging".into()),
                deploy_state: Some("success".into()),
                latest_release_tag: Some("v0.1.0".into()),
                release_url: Some("https://github.com/acme/api/releases/tag/v0.1.0".into()),
            }),
            issue: Some(IssueRef {
                provider: ProviderId::Linear,
                key: "ENG-42".into(),
                title: "Implement OAuth callback".into(),
                status: "In Progress".into(),
                assignee: Some("alice".into()),
                priority: Some("High".into()),
                url: "https://linear.app/acme/issue/ENG-42".into(),
            }),
            candidate_issues: vec!["ENG-42".into()],
            slack_threads: vec![SlackThreadRef {
                workspace: "acme".into(),
                channel_id: "C123".into(),
                thread_ts: "1700000111.222333".into(),
                url: "https://acme.slack.com/archives/C123/p1700000111222333".into(),
                title_snippet: Some("OAuth rollout".into()),
                reply_count: Some(4),
                latest_reply_at: Some("1700000222".into()),
            }],
            runbooks: vec![RunbookRef {
                path: "docs/runbooks/auth.md".into(),
                title: "Auth Troubleshooting".into(),
                summary: "Verify callback URL".into(),
                matched_reason: "matched branch keyword auth".into(),
                tags: vec!["auth".into()],
                service: Some("api".into()),
                ci_job: Some("cargo-test".into()),
            }],
        },
        updated_at: 1_700_000_100,
    };

    let t2 = AgentRow {
        address: "t2".into(),
        name: "opencode-worker".into(),
        parent: Some("t1".into()),
        agent_kind: AgentKind::OpenCode,
        lifecycle: AgentLifecycle::Live,
        attention_state: AttentionState::Working {
            message: Some("Running test suite".into()),
        },
        workspace: WorkspaceInfo {
            cwd: "/workspace/acme-api".into(),
            git_branch: Some("feat/ENG-99-tests".into()),
            ..Default::default()
        },
        correlation_tier: CorrelationTier::Inferred,
        context: CorrelatedContext {
            candidate_issues: vec!["ENG-99".into()],
            ..Default::default()
        },
        updated_at: 1_700_000_050,
        ..Default::default()
    };

    let agents = vec![t1.clone(), t2];
    let snapshot = CockpitSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        cockpit_version: env!("CARGO_PKG_VERSION").to_string(),
        generated_at: 1_700_000_100,
        published_at: 1_700_000_100,
        daemon_pid: Some(4242),
        herdr_connected: true,
        summary: FleetSummary::from_agents(&agents),
        agents,
        providers: providers.clone(),
    };

    let inspect_resp = InspectResponse {
        agent: t1,
        providers,
    };

    let sup_snap = DesktopModSnapshot {
        pill_text: "Supervisor: Evaluating".into(),
        runtime_status: SupervisorRuntimeStatus::Evaluating("t1".into()),
        mode_label: SupervisorMode::SemiAuto.label().to_string(),
        mode: SupervisorMode::SemiAuto,
        model_loaded: true,
        llm_backend: "ollama".into(),
        llm_host: "http://127.0.0.1:11434".into(),
        llm_model: "qwen3:1.7b".into(),
        num_gpu: 99,
        allow_gpu: true,
        llm_ram_bytes: Some(256_000_000),
        llm_vram_bytes: Some(1_200_000_000),
        llm_connected: true,
        model_installed: true,
        llm_status_detail: "Ready".into(),
        llm_managed: false,
        llm_runtime: String::new(),
        llm_container_state: String::new(),
        gpu_backend: "cuda".into(),
        gpu_detail: "RTX".into(),
        fallback_configured: false,
        fallback_key_present: false,
        fallback_active: false,
        fallback_model: String::new(),
        fallback_host: String::new(),
        fallback_reason: None,
        fallback_detail: None,
        fallback_since: None,
        agents: vec![AgentCardView {
            agent_id: "t1".into(),
            session_name: "auth-agent".into(),
            lifecycle_state: LifecycleState::WaitingConfirmation,
            rolling_summary: "Waiting for bash confirmation".into(),
            audit_badge: Some("Auto-assisted (1/3)".into()),
            auto_reply_log: vec![AutoReplyAuditEntry {
                timestamp: 1_700_000_000,
                payload: "ls".into(),
                summary: "Listed workspace".into(),
            }],
            action_card: Some(PendingOperatorAction {
                urgency: Urgency::Critical,
                detected_state: LifecycleState::WaitingConfirmation,
                reason_for_operator: "Needs confirmation".into(),
                updated_summary: "Run cargo test?".into(),
                hc_command_payload: "y".into(),
                triggered_at: 1_700_000_010,
            }),
            last_output_preview: "$ cargo test".into(),
            eval_error: None,
        }],
    };

    let fixtures_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    write_fixture_if_changed(
        &fixtures_dir.join("cockpit_snapshot.json"),
        &(serde_json::to_string_pretty(&snapshot).unwrap() + "\n"),
    );
    write_fixture_if_changed(
        &fixtures_dir.join("inspect_response.json"),
        &(serde_json::to_string_pretty(&inspect_resp).unwrap() + "\n"),
    );
    write_fixture_if_changed(
        &fixtures_dir.join("desktop_mod_snapshot.json"),
        &(serde_json::to_string_pretty(&sup_snap).unwrap() + "\n"),
    );
}
