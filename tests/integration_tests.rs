use cockpit::assets::{HERDR_LAYOUT_JSON, HERDR_MOD_MAIN_JS};
use cockpit::config::{CockpitConfig, CockpitPaths};
use cockpit::correlate::{
    build_initial_agent_row, extract_slack_urls, extract_ticket_keys, resolve_git_workspace,
    should_include_terminal,
};
use cockpit::daemon::{collect_once, query_socket_once, DaemonLock};
use cockpit::doctor::run_doctor;
use cockpit::herdr::{
    parse_hc_event_line, FleetTracker, HcForeground, HcStatus, HcTerminal, HerdrClient,
};
use cockpit::logging::redact_secrets;
use cockpit::model::{
    sanitize_terminal_text, AgentKind, AttentionState, BlockedKind, CockpitSnapshot,
    CorrelationTier, FleetSummaryProjection, InspectResponse, IpcOp, IpcRequest, ProviderId,
    MAX_DOC_SNAPSHOT_BYTES,
};
use cockpit::providers::runbooks::discover_and_match_runbooks;
use cockpit::providers::{is_below_quota_reserve_floor, ProviderCoordinator, TokenBucket};
use cockpit::setup::{run_setup, run_uninstall, verify_installation, SetupOptions};
use cockpit::store::{CacheStore, CachedEntity, QuotaRecord};
use std::collections::BTreeMap;
use std::fs;
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
        config_dir: home.join(".config/cockpit"),
        data_dir: home.join(".local/share/cockpit"),
        state_dir: home.join(".local/state/cockpit"),
        runtime_dir: home.join(".local/state/cockpit/run"),
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
    assert!(home.join(".config/herdr-app/mods/cockpit/mod.json").exists());
    let shared_after_setup: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.join(".config/herdr-app/shared.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        shared_after_setup.get("layout").and_then(|v| v.as_str()),
        Some("cockpit")
    );
    assert!(home.join(".config/opencode/plugins/cockpit.js").exists());
    assert!(home.join(".pi/agent/extensions/cockpit.js").exists());
    assert!(home.join(".config/systemd/user/cockpit.service").exists());
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
    assert!(!home.join(".config/opencode/plugins/cockpit.js").exists());
    assert!(!home.join(".pi/agent/extensions/cockpit.js").exists());

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
    assert!(HERDR_LAYOUT_JSON.contains("\"cockpit\""));
    let _ = std::mem::size_of::<InspectResponse>();
}
