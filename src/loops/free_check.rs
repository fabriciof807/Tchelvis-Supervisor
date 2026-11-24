use crate::loops::config::{PreflightConfig, PreflightMode};
use crate::loops::evidence::{CliIdentity, EvidenceStore};
use crate::loops::preflight::{ProbeTarget, ProbeTempDir};
use crate::loops::process::{run_process, Clock, Limits, Outcome};
use crate::loops::sandbox::Sandbox;
use crate::loops::workflow::Provider;
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const FREE_CHECK_TIMEOUT_SECS: u64 = 15;
const MAX_FREE_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listed {
    Yes,
    No,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeCheck {
    pub account_ok: Result<(), String>,
    pub listed: Listed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    Fail(String),
    Evidence,
    Probe,
    FreeOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCheck {
    pub target: ProbeTarget,
    pub action: PlanAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeStatus {
    LoggedIn {
        email: Option<String>,
        org_id: Option<String>,
        org_name: Option<String>,
        /// The only field of these that tells accounts apart inside the sandbox, where the CLI
        /// sees its credential but not its profile.
        login_expires_at: Option<String>,
    },
    LoggedOut,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFreeOutcome {
    pub provider: Provider,
    pub account_ok: Result<(), String>,
    pub models: Vec<String>,
    pub list_known: bool,
    pub claude_status: Option<ClaudeStatus>,
}

fn non_empty_string_field(val: &Value, key: &str) -> Option<String> {
    val.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

pub fn inspect_claude_status(raw: &str) -> ClaudeStatus {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ClaudeStatus::Unknown("resposta vazia de 'claude auth status'".to_string());
    }
    let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
        return ClaudeStatus::Unknown("saída inválida de 'claude auth status'".to_string());
    };
    match val.get("loggedIn").and_then(|v| v.as_bool()) {
        Some(true) => ClaudeStatus::LoggedIn {
            email: non_empty_string_field(&val, "email"),
            org_id: non_empty_string_field(&val, "orgId"),
            org_name: non_empty_string_field(&val, "orgName"),
            login_expires_at: non_empty_string_field(&val, "loginExpiresAt"),
        },
        Some(false) => ClaudeStatus::LoggedOut,
        None => {
            ClaudeStatus::Unknown("campo 'loggedIn' ausente em 'claude auth status'".to_string())
        }
    }
}

pub fn parse_claude_status(raw: &str) -> Result<(), String> {
    match inspect_claude_status(raw) {
        ClaudeStatus::LoggedIn { .. } => Ok(()),
        ClaudeStatus::LoggedOut => Err(
            "conta do 'claude' desconectada; rode 'tchelvis loop auth login claude'".to_string(),
        ),
        ClaudeStatus::Unknown(reason) => Err(reason),
    }
}

fn looks_like_error_or_markup(token: &str) -> bool {
    token.starts_with('{')
        || token.starts_with('[')
        || token.starts_with('<')
        || token.ends_with(':')
}

pub fn parse_model_list(provider: Provider, raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    match provider {
        Provider::Claude => Vec::new(),
        Provider::Agy => {
            for line in raw.lines() {
                let line = line.trim_end_matches(&['\r', '\n'][..]);
                if line.trim().is_empty() {
                    continue;
                }
                let Some((id_raw, display_raw)) = line.split_once('\t') else {
                    return Vec::new();
                };
                let id = id_raw.trim();
                let display = display_raw.trim();
                if id.is_empty()
                    || id.chars().any(char::is_whitespace)
                    || looks_like_error_or_markup(id)
                    || display.is_empty()
                {
                    return Vec::new();
                }
                out.push(id.to_string());
                out.push(display.to_string());
            }
            out
        }
        Provider::Opencode => {
            for line in raw.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.chars().any(char::is_whitespace) || looks_like_error_or_markup(trimmed) {
                    return Vec::new();
                }
                out.push(trimmed.to_string());
            }
            out
        }
    }
}

pub fn is_model_listed(provider: Provider, model: Option<&str>, listed_items: &[String]) -> Listed {
    if listed_items.is_empty() {
        return Listed::Unknown;
    }
    let Some(m) = model else {
        return Listed::Yes;
    };
    if listed_items.iter().any(|item| item == m) {
        return Listed::Yes;
    }
    if provider == Provider::Agy
        && listed_items
            .iter()
            .filter(|item| item.chars().all(|c| !c.is_whitespace()))
            .any(|item| {
                item.strip_prefix(m)
                    .is_some_and(|rest| rest.starts_with('-'))
            })
    {
        return Listed::Yes;
    }
    Listed::No
}

pub fn plan_preflight(
    targets: &[ProbeTarget],
    checks: &[FreeCheck],
    evidence: &EvidenceStore,
    config: &PreflightConfig,
    identities: &BTreeMap<Provider, CliIdentity>,
    now_ms: i64,
) -> Vec<PlannedCheck> {
    targets
        .iter()
        .zip(checks.iter())
        .map(|(target, check)| {
            let action = if let Err(reason) = &check.account_ok {
                PlanAction::Fail(reason.clone())
            } else {
                // An unreadable check says nothing about the login behind the evidence.
                let has_valid_evidence = check.listed != Listed::Unknown
                    && identities
                    .get(&target.choice.provider)
                    .is_some_and(|id| {
                        evidence.is_valid(&target.choice, id, now_ms, config.valid_hours)
                    });
                match config.mode {
                    PreflightMode::Always => PlanAction::Probe,
                    PreflightMode::Auto => {
                        if has_valid_evidence {
                            PlanAction::Evidence
                        } else {
                            PlanAction::Probe
                        }
                    }
                    PreflightMode::Free => {
                        if has_valid_evidence {
                            PlanAction::Evidence
                        } else if check.listed == Listed::Yes {
                            PlanAction::FreeOnly
                        } else if check.listed == Listed::No {
                            PlanAction::Fail(format!(
                                "modelo '{}' não encontrado na lista de '{}' (preflight mode = \"free\")",
                                target.choice.display(),
                                target.choice.provider
                            ))
                        } else {
                            PlanAction::Fail(format!(
                                "não foi possível confirmar o modelo '{}' sem gastar tokens (lista de '{}' indisponível no modo preflight = \"free\")",
                                target.choice.display(),
                                target.choice.provider
                            ))
                        }
                    }
                }
            };
            PlannedCheck {
                target: target.clone(),
                action,
            }
        })
        .collect()
}

fn non_empty_regular_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && meta.len() > 0
}

fn run_one_free_check(
    provider: Provider,
    sandbox: &(dyn Sandbox + Sync),
    dir: &Path,
    home_dir: &Path,
    cancel: &AtomicBool,
    clock: &(dyn Clock + Sync),
    opencode_api_key: Option<&str>,
) -> ProviderFreeOutcome {
    let fallback =
        |account_ok: Result<(), String>, claude_status: Option<ClaudeStatus>| ProviderFreeOutcome {
            provider,
            account_ok,
            models: Vec::new(),
            list_known: false,
            claude_status,
        };

    let Ok(ws) = ProbeTempDir::new_in(dir, "tchelvis-free-ws-") else {
        return fallback(
            Ok(()),
            (provider == Provider::Claude)
                .then(|| ClaudeStatus::Unknown("falha ao criar diretório temporário".into())),
        );
    };
    let Ok(log_dir) = ProbeTempDir::new_in(dir, "tchelvis-free-log-") else {
        return fallback(
            Ok(()),
            (provider == Provider::Claude)
                .then(|| ClaudeStatus::Unknown("falha ao criar diretório de log".into())),
        );
    };
    let log_path = log_dir.path().join("free_check.log");

    let argv: Vec<OsString> = match provider {
        Provider::Claude => vec!["claude".into(), "auth".into(), "status".into()],
        Provider::Agy => vec!["agy".into(), "models".into()],
        Provider::Opencode => vec!["opencode".into(), "models".into(), "--standalone".into()],
    };

    let wrapped = match (provider, opencode_api_key) {
        (Provider::Opencode, Some(key)) => {
            sandbox.wrap_with_extra_env(&argv, ws.path(), &[("OPENCODE_API_KEY", key)])
        }
        _ => sandbox.wrap(&argv, ws.path()),
    };
    let Ok(mut sandboxed) = wrapped else {
        return fallback(
            Ok(()),
            (provider == Provider::Claude)
                .then(|| ClaudeStatus::Unknown("falha ao preparar sandbox".into())),
        );
    };
    if provider != Provider::Opencode {
        sandboxed.env.retain(|(k, _)| k != "OPENCODE_API_KEY");
    }

    let limits = Limits {
        timeout: Duration::from_secs(FREE_CHECK_TIMEOUT_SECS),
        stall: Duration::from_secs(FREE_CHECK_TIMEOUT_SECS),
    };
    let mut captured = String::new();
    let proc_res = run_process(
        sandboxed,
        b"",
        &log_path,
        limits,
        clock,
        cancel,
        &mut |line| {
            if captured.len() < MAX_FREE_OUTPUT_BYTES {
                if !captured.is_empty() {
                    captured.push('\n');
                }
                let remaining = MAX_FREE_OUTPUT_BYTES.saturating_sub(captured.len());
                if line.len() <= remaining {
                    captured.push_str(line);
                } else {
                    let take = line.floor_char_boundary(remaining);
                    captured.push_str(&line[..take]);
                }
            }
        },
        &mut || false,
    );

    match provider {
        Provider::Claude => {
            if matches!(
                proc_res,
                Ok(Outcome::TimedOut | Outcome::Stalled | Outcome::Cancelled)
            ) || proc_res.is_err()
            {
                return fallback(
                    Ok(()),
                    Some(ClaudeStatus::Unknown(
                        "tempo esgotado ou falha ao consultar 'claude auth status'".to_string(),
                    )),
                );
            }
            let status = inspect_claude_status(&captured);
            match &status {
                ClaudeStatus::LoggedIn { .. } => ProviderFreeOutcome {
                    provider,
                    account_ok: Ok(()),
                    models: Vec::new(),
                    list_known: true,
                    claude_status: Some(status),
                },
                ClaudeStatus::LoggedOut => ProviderFreeOutcome {
                    provider,
                    account_ok: Err(
                        "conta do 'claude' desconectada; rode 'tchelvis loop auth login claude'"
                            .to_string(),
                    ),
                    models: Vec::new(),
                    list_known: false,
                    claude_status: Some(status),
                },
                ClaudeStatus::Unknown(_) => fallback(Ok(()), Some(status)),
            }
        }
        Provider::Agy => {
            if matches!(proc_res, Ok(Outcome::Exited(0))) {
                let models = parse_model_list(Provider::Agy, &captured);
                if !models.is_empty() {
                    return ProviderFreeOutcome {
                        provider,
                        account_ok: Ok(()),
                        models,
                        list_known: true,
                        claude_status: None,
                    };
                }
                if captured.trim().is_empty()
                    && !non_empty_regular_file(
                        &home_dir.join(".gemini/antigravity-cli/antigravity-oauth-token"),
                    )
                {
                    return fallback(
                        Err(
                            "conta do 'agy' ausente (~/.gemini/antigravity-cli/antigravity-oauth-token); rode 'tchelvis loop auth login agy'"
                                .to_string(),
                        ),
                        None,
                    );
                }
            }
            fallback(Ok(()), None)
        }
        Provider::Opencode => {
            if matches!(proc_res, Ok(Outcome::Exited(0))) {
                let models = parse_model_list(Provider::Opencode, &captured);
                let list_known = !models.is_empty();
                return ProviderFreeOutcome {
                    provider,
                    account_ok: Ok(()),
                    models,
                    list_known,
                    claude_status: None,
                };
            }
            fallback(Ok(()), None)
        }
    }
}

pub fn run_free_checks_with_options(
    providers: &[Provider],
    sandbox: &(dyn Sandbox + Sync),
    dir: &Path,
    home_dir: &Path,
    cancel: &AtomicBool,
    clock: &(dyn Clock + Sync),
    opencode_api_key: Option<&str>,
) -> Result<BTreeMap<Provider, ProviderFreeOutcome>, String> {
    let mut unique: Vec<Provider> = Vec::new();
    for &p in providers {
        if !unique.contains(&p) {
            unique.push(p);
        }
    }
    if unique.is_empty() {
        return Ok(BTreeMap::new());
    }

    let results: Vec<ProviderFreeOutcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = unique
            .iter()
            .map(|&provider| {
                scope.spawn(move || {
                    run_one_free_check(
                        provider,
                        sandbox,
                        dir,
                        home_dir,
                        cancel,
                        clock,
                        opencode_api_key,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("free check thread panicked"))
            .collect()
    });

    if cancel.load(Ordering::Relaxed) {
        return Err("verificação gratuita cancelada por sinal".to_string());
    }

    let mut map = BTreeMap::new();
    for res in results {
        map.insert(res.provider, res);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::config::PreflightMode;
    use crate::loops::models::ModelChoice;
    use crate::loops::sandbox::SandboxedCommand;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;
    use tempfile::TempDir;

    const FIX_CLAUDE_OK: &str = include_str!("../../tests/fixtures/loop/free_claude_status.txt");
    const FIX_CLAUDE_SANDBOX: &str =
        include_str!("../../tests/fixtures/loop/free_claude_status_sandbox.txt");
    const FIX_CLAUDE_OUT: &str =
        include_str!("../../tests/fixtures/loop/free_claude_logged_out.txt");
    const FIX_AGY_MODELS: &str = include_str!("../../tests/fixtures/loop/free_agy_models.txt");
    const FIX_AGY_OUT: &str = include_str!("../../tests/fixtures/loop/free_agy_logged_out.txt");
    const FIX_OC_MODELS: &str = include_str!("../../tests/fixtures/loop/free_opencode_models.txt");
    const FIX_OC_NOKEY: &str = include_str!("../../tests/fixtures/loop/free_opencode_no_key.txt");

    type RecordedCall = (Vec<OsString>, Vec<(OsString, OsString)>);

    struct FakeSandbox {
        recorded_calls: Mutex<Vec<RecordedCall>>,
        extra_env: Vec<(OsString, OsString)>,
    }

    impl FakeSandbox {
        fn new() -> Self {
            Self {
                recorded_calls: Mutex::new(Vec::new()),
                extra_env: Vec::new(),
            }
        }

        fn with_env(mut self, k: &str, v: &str) -> Self {
            self.extra_env.push((OsString::from(k), OsString::from(v)));
            self
        }

        fn build_and_record(
            &self,
            argv: &[OsString],
            workspace: &Path,
            extra_env: &[(&str, &str)],
        ) -> SandboxedCommand {
            let mut env = vec![
                (
                    OsString::from("PATH"),
                    std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
                ),
                (
                    OsString::from("HOME"),
                    std::env::var_os("HOME").unwrap_or_else(|| OsString::from("/tmp")),
                ),
            ];
            env.extend(self.extra_env.clone());
            for &(k, v) in extra_env {
                if let Some(entry) = env.iter_mut().find(|(ek, _)| ek == std::ffi::OsStr::new(k)) {
                    entry.1 = OsString::from(v);
                } else {
                    env.push((OsString::from(k), OsString::from(v)));
                }
            }
            self.recorded_calls
                .lock()
                .unwrap()
                .push((argv.to_vec(), env.clone()));
            SandboxedCommand {
                argv: argv.to_vec(),
                env,
                cwd: workspace.to_path_buf(),
            }
        }
    }

    impl Sandbox for FakeSandbox {
        fn kind(&self) -> &'static str {
            "fake"
        }

        fn check(&self, _workspace: &Path) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }

        fn wrap(&self, argv: &[OsString], workspace: &Path) -> anyhow::Result<SandboxedCommand> {
            Ok(self.build_and_record(argv, workspace, &[]))
        }

        fn wrap_with_extra_env(
            &self,
            argv: &[OsString],
            workspace: &Path,
            extra_env: &[(&str, &str)],
        ) -> anyhow::Result<SandboxedCommand> {
            Ok(self.build_and_record(argv, workspace, extra_env))
        }
    }

    #[test]
    fn parses_real_capture_fixtures_and_rejects_garbage_safely() {
        assert!(parse_claude_status(FIX_CLAUDE_OK).is_ok());
        assert_eq!(
            inspect_claude_status(FIX_CLAUDE_OK),
            ClaudeStatus::LoggedIn {
                email: Some("user@example.com".to_string()),
                org_id: Some("00000000-0000-0000-0000-000000000000".to_string()),
                org_name: Some("Example".to_string()),
                login_expires_at: Some("2026-12-09T00:00:00Z".to_string()),
            }
        );
        assert_eq!(
            inspect_claude_status(FIX_CLAUDE_SANDBOX),
            ClaudeStatus::LoggedIn {
                email: None,
                org_id: None,
                org_name: None,
                login_expires_at: Some("2026-12-09T01:02:03.456Z".to_string()),
            }
        );

        assert!(parse_claude_status(FIX_CLAUDE_OUT).is_err());
        assert_eq!(
            inspect_claude_status(FIX_CLAUDE_OUT),
            ClaudeStatus::LoggedOut
        );
        assert!(matches!(
            inspect_claude_status("{not json"),
            ClaudeStatus::Unknown(_)
        ));
        assert!(matches!(
            inspect_claude_status(""),
            ClaudeStatus::Unknown(_)
        ));

        let agy_list = parse_model_list(Provider::Agy, FIX_AGY_MODELS);
        assert!(agy_list.contains(&"gemini-4-hydrogen-high".to_string()));
        assert!(agy_list.contains(&"Gemini 4 Hydrogen (High)".to_string()));
        assert!(agy_list.contains(&"gemini-pro-orchestrator".to_string()));
        assert!(parse_model_list(Provider::Agy, FIX_AGY_OUT).is_empty());
        assert!(parse_model_list(Provider::Agy, "Error: not logged in\n{bad json}").is_empty());

        let oc_list = parse_model_list(Provider::Opencode, FIX_OC_MODELS);
        assert!(oc_list.contains(&"opencode-go/glm-5.3-flash".to_string()));
        assert!(oc_list.contains(&"opencode/beam-free".to_string()));
        let oc_free = parse_model_list(Provider::Opencode, FIX_OC_NOKEY);
        assert!(oc_free.contains(&"opencode/beam-free".to_string()));
        assert!(!oc_free.iter().any(|m| m.starts_with("opencode-go/")));
        assert!(parse_model_list(Provider::Opencode, "Error: auth invalid\n{bad}").is_empty());
    }

    #[test]
    fn agy_prefix_rule_accepts_dash_variants_and_refuses_partial_stem() {
        let listed = parse_model_list(Provider::Agy, FIX_AGY_MODELS);
        assert_eq!(
            is_model_listed(Provider::Agy, Some("gemini-4-hydrogen"), &listed),
            Listed::Yes
        );
        assert_eq!(
            is_model_listed(Provider::Agy, Some("Gemini 4 Hydrogen (High)"), &listed),
            Listed::Yes
        );
        assert_eq!(
            is_model_listed(Provider::Agy, Some("nonexistent-model"), &listed),
            Listed::No
        );

        assert_eq!(
            is_model_listed(Provider::Agy, Some("GPT"), &listed),
            Listed::No,
            "a display name is not a prefix source"
        );

        let no_dash_list = vec![
            "gemini-40-high".to_string(),
            "claude-opus-5-5-low".to_string(),
        ];
        assert_eq!(
            is_model_listed(Provider::Agy, Some("gemini-4"), &no_dash_list),
            Listed::No,
            "gemini-4 without gemini-4-* must be refused"
        );

        let oc_listed = parse_model_list(Provider::Opencode, FIX_OC_NOKEY);
        assert_eq!(
            is_model_listed(Provider::Opencode, Some("opencode/beam"), &oc_listed),
            Listed::No,
            "opencode must require exact match"
        );
        assert_eq!(
            is_model_listed(Provider::Opencode, Some("opencode/beam-free"), &oc_listed),
            Listed::Yes
        );
    }

    #[test]
    fn plan_preflight_full_decision_matrix() {
        let tmp = TempDir::new().unwrap();
        let ev_path = tmp.path().join("model_evidence.json");
        let mut store = EvidenceStore::open(&ev_path);
        let choice = ModelChoice {
            provider: Provider::Agy,
            model: Some("gemini-4-hydrogen".into()),
        };
        let valid_id = CliIdentity {
            executable: "exe-1".into(),
            account: "present".into(),
            sandbox: "bwrap".into(),
        };
        let now_ms = 1_700_000_000_000i64;
        let ttl_ms = 12 * 3_600_000i64;
        store.record(&choice, &valid_id, now_ms - 10_000).unwrap();

        let target = ProbeTarget {
            choice: choice.clone(),
            stage_ids: vec!["review".into()],
        };
        let mut ids_valid = BTreeMap::new();
        ids_valid.insert(Provider::Agy, valid_id.clone());

        let mut ids_other_exe = BTreeMap::new();
        ids_other_exe.insert(
            Provider::Agy,
            CliIdentity {
                executable: "exe-2".into(),
                ..valid_id.clone()
            },
        );
        let mut ids_other_acct = BTreeMap::new();
        ids_other_acct.insert(
            Provider::Agy,
            CliIdentity {
                account: "absent".into(),
                ..valid_id.clone()
            },
        );
        let mut ids_other_sb = BTreeMap::new();
        ids_other_sb.insert(
            Provider::Agy,
            CliIdentity {
                sandbox: "inherited".into(),
                ..valid_id.clone()
            },
        );

        enum EvState<'a> {
            Valid,
            Expired,
            Future,
            OtherExe,
            OtherAcct,
            OtherSb,
            None(&'a BTreeMap<Provider, CliIdentity>),
        }

        let empty_store = EvidenceStore::open(&tmp.path().join("absent.json"));
        let modes = [
            PreflightMode::Auto,
            PreflightMode::Always,
            PreflightMode::Free,
        ];
        let list_states = [Listed::Yes, Listed::No, Listed::Unknown];
        let ev_states = [
            EvState::Valid,
            EvState::Expired,
            EvState::Future,
            EvState::OtherExe,
            EvState::OtherAcct,
            EvState::OtherSb,
            EvState::None(&ids_valid),
        ];

        for mode in modes {
            let cfg = PreflightConfig {
                mode,
                valid_hours: 12,
            };
            for listed in list_states {
                // Missing account must FAIL in every mode regardless of listed/evidence
                let check_missing = FreeCheck {
                    account_ok: Err("logged out".into()),
                    listed,
                };
                let res = plan_preflight(
                    std::slice::from_ref(&target),
                    &[check_missing],
                    &store,
                    &cfg,
                    &ids_valid,
                    now_ms,
                );
                assert!(
                    matches!(&res[0].action, PlanAction::Fail(r) if r == "logged out"),
                    "mode={mode:?} listed={listed:?}: got {:?}",
                    res[0].action
                );

                for ev in &ev_states {
                    let (used_store, used_ids, query_now, ev_is_valid) = match ev {
                        EvState::Valid => (&store, &ids_valid, now_ms, true),
                        EvState::Expired => (&store, &ids_valid, (now_ms - 10_000) + ttl_ms, false),
                        EvState::Future => (&store, &ids_valid, now_ms - 20_000, false),
                        EvState::OtherExe => (&store, &ids_other_exe, now_ms, false),
                        EvState::OtherAcct => (&store, &ids_other_acct, now_ms, false),
                        EvState::OtherSb => (&store, &ids_other_sb, now_ms, false),
                        EvState::None(id_map) => (&empty_store, *id_map, now_ms, false),
                    };
                    let chk = FreeCheck {
                        account_ok: Ok(()),
                        listed,
                    };
                    let planned = plan_preflight(
                        std::slice::from_ref(&target),
                        &[chk],
                        used_store,
                        &cfg,
                        used_ids,
                        query_now,
                    );
                    let action = &planned[0].action;
                    match mode {
                        PreflightMode::Always => {
                            assert_eq!(action, &PlanAction::Probe, "listed={listed:?}");
                        }
                        PreflightMode::Auto => {
                            if ev_is_valid && listed != Listed::Unknown {
                                assert_eq!(action, &PlanAction::Evidence);
                            } else {
                                assert_eq!(action, &PlanAction::Probe);
                            }
                        }
                        PreflightMode::Free => {
                            if ev_is_valid && listed != Listed::Unknown {
                                assert_eq!(action, &PlanAction::Evidence);
                            } else if listed == Listed::Yes {
                                assert_eq!(action, &PlanAction::FreeOnly);
                            } else {
                                assert!(
                                    matches!(action, PlanAction::Fail(_)),
                                    "expected Fail in free when listed={listed:?}, got {action:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    struct HangingFreeClock {
        ready_file: PathBuf,
        mono: AtomicU64,
    }

    impl Clock for HangingFreeClock {
        fn now_ms(&self) -> i64 {
            1_700_000_000_000 + self.mono.load(Ordering::Relaxed) as i64
        }

        fn monotonic_ms(&self) -> u64 {
            if self.ready_file.exists() {
                self.mono.fetch_add(16_000, Ordering::Relaxed)
            } else {
                self.mono.fetch_add(1, Ordering::Relaxed)
            }
        }
    }

    #[test]
    fn hanging_free_check_ends_at_15s_and_leads_to_probe_in_auto_and_fail_in_free() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let work_dir = tmp.path().join("work");
        let home_dir = tmp.path().join("home");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&work_dir).unwrap();
        fs::create_dir_all(&home_dir).unwrap();

        let grandchild_pid_file = tmp.path().join("free_grandchild.pid");
        crate::test_support::write_executable(
            bin_dir.join("claude"),
            format!(
                "#!/usr/bin/env sh\nsetsid sh -c 'sleep 30' </dev/null >/dev/null 2>&1 &\necho $! > \"{}\"\nsleep 30\n",
                grandchild_pid_file.display()
            ),
        );
        crate::test_support::write_executable(
            bin_dir.join("opencode"),
            "#!/usr/bin/env sh\nprintf 'opencode/beam-free\\n'\n",
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);
        let clock = HangingFreeClock {
            ready_file: grandchild_pid_file.clone(),
            mono: AtomicU64::new(1_000),
        };
        let cancel = AtomicBool::new(false);
        let secret = "sk-opencode-free-only-secret";

        let outcomes = run_free_checks_with_options(
            &[Provider::Claude, Provider::Opencode],
            &sb,
            &work_dir,
            &home_dir,
            &cancel,
            &clock,
            Some(secret),
        )
        .unwrap();

        let gc_pid: i32 = fs::read_to_string(&grandchild_pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..40 {
            if unsafe { libc::kill(gc_pid, 0) != 0 } {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if alive {
            unsafe { libc::kill(gc_pid, libc::SIGKILL) };
            panic!("hanging free check grandchild survived cleanup");
        }

        // Verify `opencode models --standalone` was used and only opencode got OPENCODE_API_KEY.
        let calls = sb.recorded_calls.lock().unwrap();
        assert!(calls.iter().any(|(a, _)| {
            a.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>()
                == ["opencode", "models", "--standalone"]
        }));
        for (argv, env) in calls.iter() {
            let bin = argv[0].to_string_lossy();
            let has_key = env.iter().any(|(k, _)| k == "OPENCODE_API_KEY");
            if bin == "opencode" {
                assert!(has_key);
            } else {
                assert!(!has_key, "unexpected OPENCODE_API_KEY passed to {bin}");
            }
        }

        // Timed-out claude free check -> account_ok is Ok(()), list_known=false -> Auto => Probe, Free => Fail
        let claude_out = &outcomes[&Provider::Claude];
        assert!(claude_out.account_ok.is_ok());
        assert!(!claude_out.list_known);

        let empty_store = EvidenceStore::open(&tmp.path().join("absent.json"));
        let target = ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Claude,
                model: Some("haiku".into()),
            },
            stage_ids: vec!["implement".into()],
        };
        let chk = FreeCheck {
            account_ok: claude_out.account_ok.clone(),
            listed: Listed::Unknown,
        };
        let ids = BTreeMap::new();
        let auto_plan = plan_preflight(
            std::slice::from_ref(&target),
            std::slice::from_ref(&chk),
            &empty_store,
            &PreflightConfig {
                mode: PreflightMode::Auto,
                valid_hours: 12,
            },
            &ids,
            1_700_000_000_000,
        );
        assert_eq!(auto_plan[0].action, PlanAction::Probe);

        let free_plan = plan_preflight(
            &[target],
            &[chk],
            &empty_store,
            &PreflightConfig {
                mode: PreflightMode::Free,
                valid_hours: 12,
            },
            &ids,
            1_700_000_000_000,
        );
        assert!(matches!(free_plan[0].action, PlanAction::Fail(_)));
    }
}
