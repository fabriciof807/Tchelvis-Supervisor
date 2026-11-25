use crate::loops::adapters::{command as adapter_command, EventParser};
use crate::loops::models::ModelChoice;
use crate::loops::process::{run_process, Clock, Limits, Outcome, SystemClock};
use crate::loops::sandbox::{wrap_for_provider, Sandbox};
use crate::loops::state::{sanitize_single_line, Usage};
use crate::loops::workflow::{Provider, Stage, Workflow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

static NEXT_PROBE_DIR_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct ProbeTempDir(PathBuf);

impl ProbeTempDir {
    pub(crate) fn new_in(parent: &Path, prefix: &str) -> std::io::Result<Self> {
        for _ in 0..64 {
            let seq = NEXT_PROBE_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!("{prefix}{}-{seq}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&candidate) {
                Ok(()) => return Ok(Self(candidate)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "não foi possível criar diretório temporário exclusivo",
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProbeTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub const PREFLIGHT_PROMPT: &str = "Reply with exactly: OK";
pub const PREFLIGHT_TIMEOUT_SECS: u64 = 60;
pub const MAX_PROBE_REASON_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    pub choice: ModelChoice,
    pub stage_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    pub choice: ModelChoice,
    pub stage_ids: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeMethod {
    #[default]
    Probe,
    Evidence,
    FreeOnly,
}

impl ProbeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Evidence => "evidência",
            Self::FreeOnly => "gratuito",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeRecord {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub method: ProbeMethod,
    pub usage: Usage,
    pub duration_ms: u64,
}

pub fn probe_targets(workflow: &Workflow) -> Vec<ProbeTarget> {
    let stage_by_id: BTreeMap<&str, &Stage> =
        workflow.stages.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut ordered: Vec<&Stage> = Vec::with_capacity(workflow.stages.len());
    if let Some(first) = workflow.stages.first() {
        let mut queue = VecDeque::new();
        if visited.insert(first.id.as_str()) {
            queue.push_back(first);
        }
        while let Some(st) = queue.pop_front() {
            ordered.push(st);
            for next_id in [
                st.next.as_deref(),
                st.on_pass.as_deref(),
                st.on_retry.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if next_id != "done" {
                    if let Some(&target_stage) = stage_by_id.get(next_id) {
                        if visited.insert(target_stage.id.as_str()) {
                            queue.push_back(target_stage);
                        }
                    }
                }
            }
        }
    }
    for st in &workflow.stages {
        if visited.insert(st.id.as_str()) {
            ordered.push(st);
        }
    }

    let mut targets: Vec<ProbeTarget> = Vec::new();
    let mut index_by_pair: BTreeMap<(&'static str, Option<&str>), usize> = BTreeMap::new();
    for st in ordered {
        let key = (st.provider.as_str(), st.model.as_deref());
        if let Some(&idx) = index_by_pair.get(&key) {
            targets[idx].stage_ids.push(st.id.clone());
        } else {
            let idx = targets.len();
            index_by_pair.insert(key, idx);
            targets.push(ProbeTarget {
                choice: ModelChoice {
                    provider: st.provider,
                    model: st.model.clone(),
                },
                stage_ids: vec![st.id.clone()],
            });
        }
    }
    targets
}

fn sanitize_probe_reason(raw: &str, extra_secrets: &[&str]) -> String {
    let single = sanitize_single_line(raw, None);
    let redacted = crate::loops::state::redact_loop_text(&single, extra_secrets);
    let truncated: String = redacted
        .chars()
        .take(MAX_PROBE_REASON_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if truncated.is_empty() {
        "falha desconhecida no teste do modelo".to_string()
    } else {
        truncated
    }
}

fn probe_one(
    target: &ProbeTarget,
    sandbox: &(dyn Sandbox + Sync),
    dir: &Path,
    cancel: &AtomicBool,
    clock: &(dyn Clock + Sync),
    opencode_api_key: Option<&str>,
) -> Result<ProbeRecord, ProbeFailure> {
    let extra_secrets: Vec<&str> = opencode_api_key
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let fail = |raw_reason: String| ProbeFailure {
        choice: target.choice.clone(),
        stage_ids: target.stage_ids.clone(),
        reason: sanitize_probe_reason(&raw_reason, &extra_secrets),
    };

    let probe_ws = ProbeTempDir::new_in(dir, "tchelvis-probe-ws-")
        .map_err(|e| fail(format!("falha ao criar diretório vazio de teste: {e}")))?;
    let probe_log_dir = ProbeTempDir::new_in(dir, "tchelvis-probe-log-")
        .map_err(|e| fail(format!("falha ao criar diretório temporário de log: {e}")))?;
    let log_path = probe_log_dir.path().join("probe.log");

    let mut cmd = adapter_command(
        target.choice.provider,
        target.choice.model.as_deref(),
        PREFLIGHT_PROMPT,
    )
    .map_err(|e| fail(format!("{e:#}")))?;

    match target.choice.provider {
        Provider::Claude => {
            cmd.argv.push(OsString::from("--tools"));
            cmd.argv.push(OsString::from(""));
            cmd.argv.push(OsString::from("--system-prompt"));
            cmd.argv.push(OsString::from("Reply briefly."));
        }
        Provider::Pi => {
            cmd.argv.push(OsString::from("--no-tools"));
            cmd.argv.push(OsString::from("--system-prompt"));
            cmd.argv.push(OsString::from("Reply briefly."));
        }
        _ => {}
    }

    let sandboxed = wrap_for_provider(
        sandbox,
        target.choice.provider,
        &cmd.argv,
        probe_ws.path(),
        probe_log_dir.path(),
        opencode_api_key,
    )
    .map_err(|e| fail(format!("{e:#}")))?;

    let mut parser = EventParser::new(target.choice.provider);
    let start_mono = clock.monotonic_ms();
    let limits = Limits {
        timeout: Duration::from_secs(PREFLIGHT_TIMEOUT_SECS),
        stall: Duration::from_secs(PREFLIGHT_TIMEOUT_SECS),
    };

    let proc_res = run_process(
        sandboxed,
        &cmd.stdin,
        &log_path,
        limits,
        clock,
        cancel,
        &mut |line| parser.line(line),
        &mut || false,
    );
    let duration_ms = clock.monotonic_ms().saturating_sub(start_mono);
    let parsed = parser.finish();

    match proc_res {
        Ok(Outcome::Exited(0)) if parsed.error.is_none() => Ok(ProbeRecord {
            provider: target.choice.provider,
            model: target.choice.model.clone(),
            method: ProbeMethod::Probe,
            usage: parsed.usage,
            duration_ms,
        }),
        Ok(Outcome::Exited(0)) => {
            Err(fail(parsed.error.unwrap_or_else(|| {
                "adapter retornou erro no teste do modelo".to_string()
            })))
        }
        Ok(Outcome::Exited(code)) => {
            Err(fail(parsed.error.unwrap_or_else(|| {
                format!("processo retornou código de saída {code}")
            })))
        }
        Ok(Outcome::Signaled(sig)) => {
            Err(fail(parsed.error.unwrap_or_else(|| {
                format!("processo encerrado pelo sinal {sig}")
            })))
        }
        Ok(Outcome::TimedOut) => Err(fail(format!(
            "tempo limite de {PREFLIGHT_TIMEOUT_SECS}s excedido no teste do modelo"
        ))),
        Ok(Outcome::Stalled) => Err(fail(format!(
            "modelo sem resposta por {PREFLIGHT_TIMEOUT_SECS}s no teste"
        ))),
        Ok(Outcome::Cancelled) => Err(fail("teste do modelo cancelado".to_string())),
        Err(e) => Err(fail(format!("{e:#}"))),
    }
}

pub fn run_preflight(
    targets: &[ProbeTarget],
    sandbox: &(dyn Sandbox + Sync),
    dir: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<ProbeRecord>, Vec<ProbeFailure>> {
    let clock = SystemClock::new();
    run_preflight_with_options(targets, sandbox, dir, cancel, &clock, None)
}

pub fn run_preflight_with_clock(
    targets: &[ProbeTarget],
    sandbox: &(dyn Sandbox + Sync),
    dir: &Path,
    cancel: &AtomicBool,
    clock: &(dyn Clock + Sync),
) -> Result<Vec<ProbeRecord>, Vec<ProbeFailure>> {
    run_preflight_with_options(targets, sandbox, dir, cancel, clock, None)
}

pub fn run_preflight_with_options(
    targets: &[ProbeTarget],
    sandbox: &(dyn Sandbox + Sync),
    dir: &Path,
    cancel: &AtomicBool,
    clock: &(dyn Clock + Sync),
    opencode_api_key: Option<&str>,
) -> Result<Vec<ProbeRecord>, Vec<ProbeFailure>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    let results: Vec<Result<ProbeRecord, ProbeFailure>> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| {
                scope
                    .spawn(move || probe_one(target, sandbox, dir, cancel, clock, opencode_api_key))
            })
            .collect();
        handles
            .into_iter()
            .zip(targets)
            .map(|(h, target)| {
                h.join().unwrap_or_else(|_| {
                    Err(ProbeFailure {
                        choice: target.choice.clone(),
                        stage_ids: target.stage_ids.clone(),
                        reason: "falha interna ao testar o modelo".to_string(),
                    })
                })
            })
            .collect()
    });

    let mut records = Vec::with_capacity(results.len());
    let mut failures = Vec::new();
    for res in results {
        match res {
            Ok(rec) => records.push(rec),
            Err(fail) => failures.push(fail),
        }
    }

    if failures.is_empty() {
        Ok(records)
    } else {
        Err(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::sandbox::SandboxedCommand;
    use crate::loops::workflow::Stage;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct FakeSandbox {
        recorded_argvs: Mutex<Vec<Vec<OsString>>>,
        observed_ws_entries: Mutex<Vec<usize>>,
        extra_env: Vec<(OsString, OsString)>,
    }

    impl FakeSandbox {
        fn new() -> Self {
            Self {
                recorded_argvs: Mutex::new(Vec::new()),
                observed_ws_entries: Mutex::new(Vec::new()),
                extra_env: Vec::new(),
            }
        }

        fn with_env(mut self, k: &str, v: &str) -> Self {
            self.extra_env.push((OsString::from(k), OsString::from(v)));
            self
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
            let entries = fs::read_dir(workspace)?.count();
            self.observed_ws_entries.lock().unwrap().push(entries);
            self.recorded_argvs.lock().unwrap().push(argv.to_vec());

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
            Ok(SandboxedCommand {
                argv: argv.to_vec(),
                env,
                cwd: workspace.to_path_buf(),
            })
        }
    }

    fn make_stage(id: &str, provider: Provider, model: Option<&str>) -> Stage {
        Stage {
            id: id.to_string(),
            role: "worker".to_string(),
            provider,
            model: model.map(str::to_string),
            reads: Vec::new(),
            writes: None,
            edits: true,
            next: Some("done".to_string()),
            verdict_gate: false,
            on_pass: None,
            on_retry: None,
        }
    }

    #[test]
    fn probe_targets_deduplicates_pairs_in_workflow_order_including_retry_only_stages() {
        let mut s1 = make_stage("plan", Provider::Claude, Some("sonnet"));
        s1.next = Some("work".to_string());
        let mut s2 = make_stage("work", Provider::Claude, Some("sonnet"));
        s2.next = Some("gate".to_string());
        let mut s3 = make_stage("gate", Provider::Agy, None);
        s3.verdict_gate = true;
        s3.writes = Some("critic.md".to_string());
        s3.next = None;
        s3.on_pass = Some("done".to_string());
        s3.on_retry = Some("repair".to_string());
        let mut s4 = make_stage("repair", Provider::Opencode, Some("zai/glm-5.3-flash"));
        s4.next = Some("gate".to_string());

        let wf = Workflow {
            name: "secret-repo-workflow".to_string(),
            description: "desc".to_string(),
            max_cycles: 3,
            stage_timeout_secs: 60,
            stall_secs: 60,
            tool_stall_secs: 1200,
            max_duration_secs: 300,
            stages: vec![s1, s2, s3, s4],
        };

        let targets = probe_targets(&wf);
        assert_eq!(targets.len(), 3);
        assert_eq!(
            targets[0],
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Claude,
                    model: Some("sonnet".to_string()),
                },
                stage_ids: vec!["plan".to_string(), "work".to_string()],
            }
        );
        assert_eq!(
            targets[1],
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Agy,
                    model: None,
                },
                stage_ids: vec!["gate".to_string()],
            }
        );
        assert_eq!(
            targets[2],
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Opencode,
                    model: Some("zai/glm-5.3-flash".to_string()),
                },
                stage_ids: vec!["repair".to_string()],
            }
        );
    }

    #[test]
    fn run_preflight_passes_with_claude_tools_empty_clean_prompt_and_empty_temp_dirs() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        let capture_dir = tmp.path().join("captures");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();
        fs::create_dir_all(&capture_dir).unwrap();

        let claude_stdin = capture_dir.join("claude_stdin.txt");
        let claude_script = bin_dir.join("claude");
        crate::test_support::write_executable(
            &claude_script,
            format!(
                r#"#!/usr/bin/env sh
cat > "{}"
printf '%s\n' '{{"type":"result","is_error":false,"result":"Hello, not OK literally","num_turns":1,"usage":{{"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":4}},"total_cost_usd":0.0005}}'
"#,
                claude_stdin.display()
            ),
        );

        let agy_script = bin_dir.join("agy");
        crate::test_support::write_executable(
            &agy_script,
            r#"#!/usr/bin/env sh
printf '%s\n' '{"event":"step_update","step_update":{"state":"DONE","step_type":"agent_response"}}'
printf '%s\n' '{"event":"result","result":{"status":"SUCCESS","usage":{"input_tokens":20,"output_tokens":5,"cache_read_tokens":2}}}'
"#,
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);

        let targets = vec![
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Claude,
                    model: Some("haiku".to_string()),
                },
                stage_ids: vec!["plan".to_string(), "audit".to_string()],
            },
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Agy,
                    model: None,
                },
                stage_ids: vec!["review".to_string()],
            },
        ];

        let cancel = AtomicBool::new(false);
        let records = run_preflight(&targets, &sb, &probe_root, &cancel).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].provider, Provider::Claude);
        assert_eq!(records[0].model.as_deref(), Some("haiku"));
        assert_eq!(records[0].usage.input_tokens, Some(12));
        assert_eq!(records[1].provider, Provider::Agy);
        assert_eq!(records[1].usage.turns, Some(1));

        // Prompt contains only the fixed string and nothing from any repo.
        assert_eq!(fs::read_to_string(&claude_stdin).unwrap(), PREFLIGHT_PROMPT);

        // Claude probe included --tools "".
        let argvs = sb.recorded_argvs.lock().unwrap();
        let claude_argv = argvs
            .iter()
            .find(|a| a.first().is_some_and(|p| p == "claude"))
            .unwrap();
        let tools_idx = claude_argv.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(claude_argv.get(tools_idx + 1).unwrap(), "");

        // Every probe got an empty workspace directory, and all temp dirs were removed afterwards.
        assert!(sb
            .observed_ws_entries
            .lock()
            .unwrap()
            .iter()
            .all(|&n| n == 0));
        assert_eq!(fs::read_dir(&probe_root).unwrap().count(), 0);
    }

    #[test]
    fn run_preflight_lists_all_failures_with_redacted_single_line_200_char_reasons() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();

        let long_tail = "x".repeat(300);
        crate::test_support::write_executable(
            bin_dir.join("claude"),
            format!(
                r#"#!/usr/bin/env sh
cat >/dev/null
printf '%s\n' '{{"type":"result","is_error":true,"result":"falha\nAuthorization: Bearer super_secret_token_999 {long_tail}"}}'
exit 1
"#
            ),
        );
        crate::test_support::write_executable(
            bin_dir.join("opencode"),
            r#"#!/usr/bin/env sh
cat >/dev/null
printf '%s\n' '{"type":"error","error":{"message":"modelo inválido ghp_1234567890abcdef1234"}}'
exit 1
"#,
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);

        let targets = vec![
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Claude,
                    model: Some("bad-claude".into()),
                },
                stage_ids: vec!["audit".into()],
            },
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Opencode,
                    model: Some("bad-op".into()),
                },
                stage_ids: vec!["fix".into()],
            },
        ];

        let cancel = AtomicBool::new(false);
        let failures = run_preflight(&targets, &sb, &probe_root, &cancel).unwrap_err();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].stage_ids, vec!["audit"]);
        assert!(failures[0].reason.contains("falha"));
        assert!(failures[0].reason.contains("[REDACTED]"));
        assert_eq!(failures[1].stage_ids, vec!["fix"]);
        assert!(failures[1].reason.contains("modelo inválido"));
        assert!(failures[1].reason.contains("[REDACTED]"));

        for f in &failures {
            assert!(!f.reason.contains('\n'));
            assert!(!f.reason.contains('\r'));
            assert!(!f.reason.contains("super_secret_token_999"));
            assert!(!f.reason.contains("ghp_1234567890abcdef1234"));
            assert!(
                f.reason.chars().count() <= MAX_PROBE_REASON_CHARS,
                "reason len {} > 200: {}",
                f.reason.chars().count(),
                f.reason
            );
        }
        assert_eq!(fs::read_dir(&probe_root).unwrap().count(), 0);
    }

    struct DelayedTimeoutClock {
        gate_file: PathBuf,
        fast_done_file: PathBuf,
        mono: AtomicU64,
    }

    impl Clock for DelayedTimeoutClock {
        fn now_ms(&self) -> i64 {
            1_700_000_000_000 + self.mono.load(Ordering::Relaxed) as i64
        }

        fn monotonic_ms(&self) -> u64 {
            // Both probes share this clock: it only jumps once the fast one ended and was reaped.
            let fast_reaped = fs::read_to_string(&self.fast_done_file)
                .ok()
                .and_then(|raw| raw.strip_suffix('\n')?.parse::<u32>().ok())
                .is_some_and(|pid| !Path::new(&format!("/proc/{pid}")).exists());
            if self.gate_file.exists() && fast_reaped {
                self.mono.fetch_add(65_000, Ordering::Relaxed)
            } else {
                self.mono.fetch_add(1, Ordering::Relaxed)
            }
        }
    }

    #[test]
    fn hanging_probe_ends_at_60s_kills_setsid_tree_and_does_not_disturb_others() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();

        let grandchild_pid_file = tmp.path().join("slow_grandchild.pid");
        let fast_done_file = tmp.path().join("fast_done");

        crate::test_support::write_executable(
            bin_dir.join("claude"),
            format!(
                r#"#!/usr/bin/env sh
cat >/dev/null
setsid sh -c 'sleep 30' </dev/null >/dev/null 2>&1 &
echo $! > "{}"
sleep 30
"#,
                grandchild_pid_file.display()
            ),
        );

        crate::test_support::write_executable(
            bin_dir.join("agy"),
            format!(
                r#"#!/usr/bin/env sh
printf '%s\n' '{{"event":"step_update","step_update":{{"state":"DONE","step_type":"agent_response"}}}}'
printf '%s\n' '{{"event":"result","result":{{"status":"SUCCESS","usage":{{"input_tokens":5,"output_tokens":2,"cache_read_tokens":0}}}}}}'
echo $$ > "{}"
"#,
                fast_done_file.display()
            ),
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);
        let clock = DelayedTimeoutClock {
            gate_file: grandchild_pid_file.clone(),
            fast_done_file: fast_done_file.clone(),
            mono: AtomicU64::new(1_000),
        };

        let targets = vec![
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Claude,
                    model: None,
                },
                stage_ids: vec!["audit".into()],
            },
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Agy,
                    model: None,
                },
                stage_ids: vec!["review".into()],
            },
        ];

        let cancel = AtomicBool::new(false);
        let failures =
            run_preflight_with_clock(&targets, &sb, &probe_root, &cancel, &clock).unwrap_err();
        assert!(
            fast_done_file.exists(),
            "fast probe must finish cleanly: {failures:?}"
        );
        assert_eq!(
            failures.len(),
            1,
            "only hanging probe should fail: {failures:?}"
        );
        assert_eq!(failures[0].choice.provider, Provider::Claude);
        assert!(
            failures[0].reason.contains("60") || failures[0].reason.contains("tempo"),
            "unexpected timeout reason: {}",
            failures[0].reason
        );

        let gc_pid: i32 = fs::read_to_string(&grandchild_pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..40 {
            let dead = unsafe { libc::kill(gc_pid, 0) != 0 };
            if dead {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if alive {
            unsafe {
                libc::kill(gc_pid, libc::SIGKILL);
            }
            panic!("hanging probe grandchild pid {gc_pid} survived cleanup");
        }
        assert_eq!(fs::read_dir(&probe_root).unwrap().count(), 0);
    }

    #[test]
    fn cancel_flag_aborts_probe_and_cleans_up() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();

        crate::test_support::write_executable(
            bin_dir.join("claude"),
            "#!/usr/bin/env sh\nsleep 30\n",
        );
        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);

        let cancel = AtomicBool::new(true);
        let targets = vec![ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Claude,
                model: None,
            },
            stage_ids: vec!["audit".into()],
        }];
        let failures = run_preflight(&targets, &sb, &probe_root, &cancel).unwrap_err();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].reason.contains("cancelado"));
        assert_eq!(fs::read_dir(&probe_root).unwrap().count(), 0);
    }

    struct PanickingSandbox;

    impl Sandbox for PanickingSandbox {
        fn kind(&self) -> &'static str {
            "panic"
        }

        fn check(&self, _workspace: &Path) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }

        fn wrap(
            &self,
            _argv: &[OsString],
            _workspace: &Path,
        ) -> anyhow::Result<crate::loops::sandbox::SandboxedCommand> {
            panic!("boom")
        }
    }

    #[test]
    fn a_probe_that_panics_is_reported_as_its_own_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Agy,
                model: Some("m".to_string()),
            },
            stage_ids: vec!["review".to_string()],
        };
        let failures = run_preflight(
            std::slice::from_ref(&target),
            &PanickingSandbox,
            tmp.path(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(failures[0].choice, target.choice);
        assert_eq!(failures[0].stage_ids, target.stage_ids);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn pi_probe_adds_no_tools_and_short_system_prompt() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();
        let saw_argv = tmp.path().join("saw_argv");
        let ok = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/loop/pi_ok.jsonl"
        );
        crate::test_support::write_executable(
            bin_dir.join("pi"),
            format!(
                "#!/usr/bin/env sh\n\
                 printf '%s' \"$*\" > \"{}\"\n\
                 cat \"{}\"\n\
                 exit 0\n",
                saw_argv.display(),
                ok
            ),
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);
        let targets = vec![ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Pi,
                model: Some("opencode-go/glm-5.3".into()),
            },
            stage_ids: vec!["implement".into()],
        }];

        let records = run_preflight_with_options(
            &targets,
            &sb,
            &probe_root,
            &AtomicBool::new(false),
            &SystemClock::new(),
            None,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let argv = fs::read_to_string(&saw_argv).unwrap();
        assert!(
            argv.ends_with("--model opencode-go/glm-5.3 --no-tools --system-prompt Reply briefly."),
            "got: {argv}"
        );
        assert_eq!(records[0].usage.output_tokens, Some(39));
    }

    #[test]
    fn pi_bad_model_with_exit_zero_fails_probe() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();
        let bad = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/loop/pi_bad_model.jsonl"
        );
        crate::test_support::write_executable(
            bin_dir.join("pi"),
            format!("#!/usr/bin/env sh\ncat \"{bad}\"\nexit 0\n"),
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);
        let targets = vec![ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Pi,
                model: Some("opencode-go/glm-5.3".into()),
            },
            stage_ids: vec!["implement".into()],
        }];

        let failures = run_preflight_with_options(
            &targets,
            &sb,
            &probe_root,
            &AtomicBool::new(false),
            &SystemClock::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].reason.contains("isn't included"),
            "got: {}",
            failures[0].reason
        );
    }

    #[test]
    fn opencode_api_key_reaches_only_opencode_and_is_redacted_from_failure_reasons() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();
        let claude_saw = tmp.path().join("claude_saw_key");
        let opencode_saw = tmp.path().join("opencode_saw_key");

        crate::test_support::write_executable(
            bin_dir.join("claude"),
            format!(
                "#!/usr/bin/env sh\n\
                 printf '%s' \"${{OPENCODE_API_KEY:-ABSENT}}\" > \"{}\"\n\
                 printf '%s\\n' '{{\"type\":\"result\",\"is_error\":false,\"num_turns\":1,\"result\":\"OK\"}}'\n",
                claude_saw.display()
            ),
        );
        crate::test_support::write_executable(
            bin_dir.join("opencode"),
            format!(
                "#!/usr/bin/env sh\n\
                 printf '%s' \"${{OPENCODE_API_KEY:-ABSENT}}\" > \"{}\"\n\
                 printf '%s\\n' '{{\"type\":\"error\",\"error\":{{\"message\":\"rejected key secret-oc-key-42\"}}}}'\n\
                 exit 1\n",
                opencode_saw.display()
            ),
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        // Intentionally also pass OPENCODE_API_KEY via FakeSandbox env to ensure claude strips it.
        let sb = FakeSandbox::new()
            .with_env("PATH", &test_path)
            .with_env("OPENCODE_API_KEY", "secret-oc-key-42");

        let targets = vec![
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Claude,
                    model: None,
                },
                stage_ids: vec!["review".into()],
            },
            ProbeTarget {
                choice: ModelChoice {
                    provider: Provider::Opencode,
                    model: Some("zai/glm".into()),
                },
                stage_ids: vec!["implement".into()],
            },
        ];

        let failures = run_preflight_with_options(
            &targets,
            &sb,
            &probe_root,
            &AtomicBool::new(false),
            &SystemClock::new(),
            Some("secret-oc-key-42"),
        )
        .unwrap_err();

        assert_eq!(fs::read_to_string(&claude_saw).unwrap(), "ABSENT");
        assert_eq!(
            fs::read_to_string(&opencode_saw).unwrap(),
            "secret-oc-key-42"
        );
        assert_eq!(failures.len(), 1);
        assert!(
            !failures[0].reason.contains("secret-oc-key-42"),
            "secret leaked in probe failure: {}",
            failures[0].reason
        );
        assert!(failures[0].reason.contains("[REDACTED]"));
    }

    #[test]
    fn claude_probe_argv_has_lean_flags_while_stage_argv_does_not() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let probe_root = tmp.path().join("probes");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&probe_root).unwrap();

        crate::test_support::write_executable(
            bin_dir.join("claude"),
            "#!/usr/bin/env sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"result\",\"is_error\":false,\"num_turns\":1,\"result\":\"OK\"}'\n",
        );

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", bin_dir.display());
        let sb = FakeSandbox::new().with_env("PATH", &test_path);

        let target = ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Claude,
                model: Some("haiku".into()),
            },
            stage_ids: vec!["implement".into()],
        };
        run_preflight(&[target], &sb, &probe_root, &AtomicBool::new(false)).unwrap();

        let argvs = sb.recorded_argvs.lock().unwrap();
        let probe_argv: Vec<String> = argvs[0]
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(probe_argv.windows(2).any(|w| w == ["--tools", ""]));
        assert!(
            probe_argv
                .windows(2)
                .any(|w| w == ["--system-prompt", "Reply briefly."]),
            "missing lean --system-prompt flag in probe argv: {probe_argv:?}"
        );

        let stage_argv: Vec<String> =
            adapter_command(Provider::Claude, Some("haiku"), "stage prompt")
                .unwrap()
                .argv
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
        assert!(!stage_argv.iter().any(|a| a == "--tools"));
        assert!(!stage_argv.iter().any(|a| a == "--system-prompt"));
    }
}
