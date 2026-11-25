use crate::loops::adapters::{
    build_prompt, command as adapter_command, AdapterResult, EventParser,
};
use crate::loops::artifacts::{collect_output, prepare_exchange};
use crate::loops::models::ModelSource;
use crate::loops::preflight::ProbeRecord;
use crate::loops::process::{
    run_process_diagnosed, Clock, Limits, Outcome, StopSnapshot, ToolWatch,
};
use crate::loops::report::render_report_with_secrets;
use crate::loops::sandbox::{wrap_for_provider, Sandbox};
use crate::loops::state::{
    redact_loop_text, sanitize_single_line, save_state, CallerInfo, Execution, RunState, RunStatus,
    StopDiagnostic, StopLimit, StopProcess, ToolSignal, ToolSilence, Usage,
    LOOP_STATE_SCHEMA_VERSION,
};
use crate::loops::verdict::{parse_verdict, summary_line, MAX_SUMMARY_CHARS};
use crate::loops::workflow::{Provider, Stage, Workflow};
use crate::loops::workspace::Workspace;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, UNIX_EPOCH};

pub const HEARTBEAT_INTERVAL_MS: u64 = 10_000;
pub const PROBE_SAMPLE_INTERVAL_MS: u64 = 10_000;
const MAX_PROBE_ENTRIES: usize = 100_000;
const MAX_REASON_CHARS: usize = 500;
/// Upper bounds on what a single stop diagnostic keeps, so a wedged stage cannot bloat the state.
const MAX_STOP_TOOLS: usize = 10;
const MAX_STOP_PROCESSES: usize = 20;
const MAX_STOP_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    Ok,
    NonZeroExit(i32),
    Signaled(i32),
    TimedOut,
    Stalled,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageResult {
    pub outcome: StageOutcome,
    pub usage: Usage,
    pub error: Option<String>,
    /// Filled only when the engine cut the stage short; see [`StopDiagnostic`].
    pub stop: Option<StopDiagnostic>,
    /// The longest silence past `stall_secs` that an open tool made acceptable.
    pub tool_silence: Option<ToolSilence>,
}

impl Default for StageResult {
    fn default() -> Self {
        Self {
            outcome: StageOutcome::Ok,
            usage: Usage::default(),
            error: None,
            stop: None,
            tool_silence: None,
        }
    }
}

pub struct StageRequest<'a> {
    pub run_id: &'a str,
    pub cycle: u32,
    pub exec_seq: usize,
    pub stage: &'a Stage,
    pub workspace: &'a Path,
    pub prompt: &'a str,
    pub log_path: &'a Path,
    pub limits: Limits,
    pub tool_stall: Duration,
    pub cancel: &'a AtomicBool,
    pub pulse: &'a dyn Fn(bool),
}

pub trait StageRunner {
    fn run(&mut self, req: StageRequest<'_>) -> StageResult;
}

pub trait EvidenceRecorder {
    fn record(&mut self, provider: Provider, model: Option<&str>, now_ms: i64);
    fn discard(&mut self, provider: Provider, model: Option<&str>);
}

pub struct ProcessStageRunner<'a> {
    pub sandbox: &'a dyn Sandbox,
    pub clock: &'a dyn Clock,
    pub opencode_api_key: Option<String>,
    pub extra_secrets: Vec<String>,
}

impl<'a> ProcessStageRunner<'a> {
    pub fn new(sandbox: &'a dyn Sandbox, clock: &'a dyn Clock) -> Self {
        Self {
            sandbox,
            clock,
            opencode_api_key: None,
            extra_secrets: Vec::new(),
        }
    }

    pub fn with_opencode_api_key(mut self, key: Option<String>) -> Self {
        self.opencode_api_key = key.filter(|k| !k.is_empty());
        self
    }

    pub fn with_extra_secrets(mut self, extra_secrets: Vec<String>) -> Self {
        self.extra_secrets = extra_secrets;
        self
    }
}

impl<'a> StageRunner for ProcessStageRunner<'a> {
    fn run(&mut self, req: StageRequest<'_>) -> StageResult {
        let extra_refs: Vec<&str> = self.extra_secrets.iter().map(|s| s.as_str()).collect();
        let cmd = match adapter_command(req.stage.provider, req.stage.model.as_deref(), req.prompt)
        {
            Ok(c) => c,
            Err(e) => {
                return StageResult {
                    outcome: StageOutcome::Error,
                    usage: Usage::default(),
                    error: Some(sanitize_reason(&format!("{e:#}"), &extra_refs)),
                    stop: None,
                    tool_silence: None,
                };
            }
        };

        let sandboxed = match wrap_for_provider(
            self.sandbox,
            req.stage.provider,
            &cmd.argv,
            req.workspace,
            // One scratch directory per stage execution, so a stage never inherits another's files.
            &req.log_path.with_extension("scratch"),
            self.opencode_api_key.as_deref(),
        ) {
            Ok(s) => s,
            Err(e) => {
                return StageResult {
                    outcome: StageOutcome::Error,
                    usage: Usage::default(),
                    error: Some(sanitize_reason(&format!("{e:#}"), &extra_refs)),
                    stop: None,
                    tool_silence: None,
                };
            }
        };

        // The monitor asks the parser what is open while `on_line` feeds it, so the parser is
        // shared through a `RefCell` rather than moved into one closure.
        let parser = RefCell::new(EventParser::new(req.stage.provider));
        let mut sampler = WorkspaceSampler::new(req.workspace, self.clock);
        let pulse = req.pulse;
        let mut tool_watch = ToolWatch {
            limit: req.tool_stall,
            view: &mut || parser.borrow().tool_view(),
            silence: None,
        };

        let mut stop_snapshot: Option<StopSnapshot> = None;
        let proc_res = run_process_diagnosed(
            sandboxed,
            &cmd.stdin,
            req.log_path,
            req.limits,
            self.clock,
            req.cancel,
            &mut |line| {
                parser.borrow_mut().line(line);
                pulse(true);
            },
            &mut || {
                pulse(false);
                if sampler.sample_throttled() {
                    pulse(true);
                    true
                } else {
                    false
                }
            },
            Some(&mut tool_watch),
            &mut stop_snapshot,
        );
        let tool_silence = tool_watch.silence;
        let parser = parser.into_inner();

        // A stage the engine ended never reaches its own settled event, so the parser must not
        // report the resulting silence as a provider failure.
        let interrupted = matches!(
            &proc_res,
            Ok(Outcome::Stalled) | Ok(Outcome::TimedOut) | Ok(Outcome::Cancelled)
        );
        let (parsed, stop) = if interrupted {
            let stream = parser.finish_interrupted();
            let stop = stop_snapshot.map(|snapshot| StopDiagnostic {
                last_event: stream.last_event.clone(),
                last_output_at_ms: snapshot.last_output_at_ms,
                silent_ms: snapshot.silent_ms,
                cli_alive: snapshot.cli_alive,
                running_tools: stream.running_tools.clone(),
                processes: snapshot.processes,
                processes_total: snapshot.processes_total,
                limit: snapshot.limit,
                limit_secs: (snapshot.limit == Some(StopLimit::ToolStall))
                    .then_some(req.tool_stall.as_secs()),
                tool_signal: snapshot.tool_signal,
            });
            (
                AdapterResult {
                    usage: stream.usage,
                    error: stream.error,
                },
                stop,
            )
        } else {
            (parser.finish(), None)
        };

        let parsed_error = parsed.error.map(|e| sanitize_reason(&e, &extra_refs));
        match proc_res {
            Err(e) => StageResult {
                outcome: StageOutcome::Error,
                usage: parsed.usage,
                error: Some(sanitize_reason(&format!("{e:#}"), &extra_refs)),
                stop: None,
                tool_silence,
            },
            Ok(Outcome::Exited(0)) => {
                if parsed_error.is_some() {
                    StageResult {
                        outcome: StageOutcome::Error,
                        usage: parsed.usage,
                        error: parsed_error,
                        stop: None,
                        tool_silence,
                    }
                } else {
                    StageResult {
                        outcome: StageOutcome::Ok,
                        usage: parsed.usage,
                        error: None,
                        stop: None,
                        tool_silence,
                    }
                }
            }
            Ok(Outcome::Exited(code)) => StageResult {
                outcome: StageOutcome::NonZeroExit(code),
                usage: parsed.usage,
                error: parsed_error
                    .or_else(|| Some(format!("processo retornou código de saída {code}"))),
                stop: None,
                tool_silence,
            },
            Ok(Outcome::Signaled(sig)) => StageResult {
                outcome: StageOutcome::Signaled(sig),
                usage: parsed.usage,
                error: parsed_error
                    .or_else(|| Some(format!("processo encerrado pelo sinal {sig}"))),
                stop: None,
                tool_silence,
            },
            Ok(Outcome::TimedOut) => StageResult {
                outcome: StageOutcome::TimedOut,
                usage: parsed.usage,
                error: parsed_error,
                stop,
                tool_silence,
            },
            Ok(Outcome::Stalled) => StageResult {
                outcome: StageOutcome::Stalled,
                usage: parsed.usage,
                error: parsed_error,
                stop,
                tool_silence,
            },
            Ok(Outcome::Cancelled) => StageResult {
                outcome: StageOutcome::Cancelled,
                usage: parsed.usage,
                error: parsed_error,
                stop: None,
                tool_silence,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceFingerprint {
    entries: usize,
    total_bytes: u64,
    hash: u64,
}

fn fnv1a_mix(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(1_099_511_628_211);
    }
    h
}

/// Samples the workspace without following symlinks and without running `git`.
fn compute_workspace_fingerprint(root: &Path) -> WorkspaceFingerprint {
    let mut stack = vec![root.to_path_buf()];
    let mut entries = 0usize;
    let mut total_bytes = 0u64;
    let mut hash = 14_695_981_039_346_656_037u64;

    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = read_dir
            .flatten()
            .filter_map(|ent| {
                let name = ent.file_name();
                if name == std::ffi::OsStr::new(".git") {
                    None
                } else {
                    Some(ent.path())
                }
            })
            .collect();
        children.sort();

        for path in children {
            if entries >= MAX_PROBE_ENTRIES {
                return WorkspaceFingerprint {
                    entries,
                    total_bytes,
                    hash,
                };
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            entries += 1;
            let rel = path.strip_prefix(root).unwrap_or(&path);
            hash = fnv1a_mix(hash, rel.as_os_str().as_encoded_bytes());
            hash = fnv1a_mix(hash, b"|");

            let ft = meta.file_type();
            let kind_tag: u8 = if ft.is_symlink() {
                b'L'
            } else if ft.is_dir() {
                b'D'
            } else {
                b'F'
            };
            hash = fnv1a_mix(hash, &[kind_tag]);

            if !ft.is_symlink() && ft.is_dir() {
                stack.push(path);
            } else {
                let len = meta.len();
                total_bytes = total_bytes.saturating_add(len);
                hash = fnv1a_mix(hash, &len.to_le_bytes());
                if let Ok(mtime) = meta.modified() {
                    if let Ok(dur) = mtime.duration_since(UNIX_EPOCH) {
                        hash = fnv1a_mix(hash, &dur.as_secs().to_le_bytes());
                        hash = fnv1a_mix(hash, &dur.subsec_nanos().to_le_bytes());
                    }
                }
            }
        }
    }

    WorkspaceFingerprint {
        entries,
        total_bytes,
        hash,
    }
}

pub struct WorkspaceSampler<'a> {
    workspace: PathBuf,
    clock: &'a dyn Clock,
    last_sample_mono_ms: u64,
    last_fingerprint: WorkspaceFingerprint,
}

impl<'a> WorkspaceSampler<'a> {
    pub fn new(workspace: &Path, clock: &'a dyn Clock) -> Self {
        let now = clock.monotonic_ms();
        let fp = compute_workspace_fingerprint(workspace);
        Self {
            workspace: workspace.to_path_buf(),
            clock,
            last_sample_mono_ms: now,
            last_fingerprint: fp,
        }
    }

    pub fn sample_throttled(&mut self) -> bool {
        let now = self.clock.monotonic_ms();
        if now.saturating_sub(self.last_sample_mono_ms) < PROBE_SAMPLE_INTERVAL_MS {
            return false;
        }
        self.last_sample_mono_ms = now;
        let fp = compute_workspace_fingerprint(&self.workspace);
        if fp != self.last_fingerprint {
            self.last_fingerprint = fp;
            true
        } else {
            false
        }
    }
}

pub struct EngineConfig {
    pub run_id: String,
    pub workflow: Workflow,
    pub roles: BTreeMap<String, String>,
    pub run_dir: PathBuf,
    pub input_prompt: String,
    pub sandbox_kind: String,
    pub sandbox_exposures: Vec<String>,
    pub branch: String,
    pub base_commit: Option<String>,
    pub caller: Option<CallerInfo>,
    pub cancel: Arc<AtomicBool>,
}

pub type FinalHook = Box<dyn FnOnce(&RunState) + Send>;

pub struct Engine<W: Workspace, R: StageRunner, C: Clock> {
    config: EngineConfig,
    workspace: W,
    runner: R,
    clock: C,
    model_sources: BTreeMap<String, ModelSource>,
    preflight: Vec<ProbeRecord>,
    extra_secrets: Vec<String>,
    on_final: Option<FinalHook>,
    evidence: Option<Box<dyn EvidenceRecorder + Send>>,
}

fn sanitize_text(raw: &str, max_chars: Option<usize>, extra_secrets: &[&str]) -> String {
    let single = sanitize_single_line(raw, max_chars);
    let redacted = redact_loop_text(&single, extra_secrets);
    match max_chars {
        Some(limit) => redacted
            .chars()
            .take(limit)
            .collect::<String>()
            .trim()
            .to_string(),
        None => redacted,
    }
}

fn sanitize_reason(raw: &str, extra_secrets: &[&str]) -> String {
    let s = sanitize_text(raw, Some(MAX_REASON_CHARS), extra_secrets);
    if s.is_empty() {
        "erro desconhecido".to_string()
    } else {
        s
    }
}

/// Redacts and caps a diagnostic before it is persisted; process names come from the kernel.
fn sanitize_stop(stop: StopDiagnostic, extra_secrets: &[&str]) -> StopDiagnostic {
    // Redact the whole text before cutting it: a cut secret would no longer be recognised.
    let text = |raw: &str| {
        let full = sanitize_text(raw, None, extra_secrets);
        sanitize_text(&full, Some(MAX_STOP_NAME_CHARS), extra_secrets)
    };
    StopDiagnostic {
        last_event: stop
            .last_event
            .map(|event| text(&event))
            .filter(|event| !event.is_empty()),
        last_output_at_ms: stop.last_output_at_ms,
        silent_ms: stop.silent_ms,
        cli_alive: stop.cli_alive,
        running_tools: stop.running_tools.map(|tools| {
            tools
                .iter()
                .take(MAX_STOP_TOOLS)
                .map(|tool| text(tool))
                .filter(|tool| !tool.is_empty())
                .collect()
        }),
        processes: stop
            .processes
            .iter()
            .take(MAX_STOP_PROCESSES)
            .map(|p| StopProcess {
                name: text(&p.name),
                age_ms: p.age_ms,
                cpu_ms: p.cpu_ms,
                after_last_output: p.after_last_output,
            })
            .collect(),
        processes_total: stop.processes_total,
        limit: stop.limit,
        limit_secs: stop.limit_secs,
        tool_signal: stop.tool_signal.map(|signal| ToolSignal {
            source: signal.source,
            names: signal
                .names
                .iter()
                .take(MAX_STOP_TOOLS)
                .map(|name| text(name))
                .filter(|name| !name.is_empty())
                .collect(),
        }),
    }
}

fn read_self_start_ticks() -> Option<u64> {
    let raw = fs::read_to_string("/proc/self/stat").ok()?;
    let rparen = raw.rfind(')')?;
    let rest = raw.get(rparen + 1..)?.trim_start();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(19)?.parse::<u64>().ok()
}

fn write_text_file(path: &Path, content: &str) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    Ok(())
}

struct SharedState {
    run_dir: PathBuf,
    last_heartbeat_mono_ms: u64,
    state: RunState,
    write_error: Option<String>,
}

struct HeartbeatSignal {
    stopped: Mutex<bool>,
    cv: Condvar,
}

struct HeartbeatGuard<'a>(&'a HeartbeatSignal);

impl Drop for HeartbeatGuard<'_> {
    fn drop(&mut self) {
        let mut stopped = self.0.stopped.lock().unwrap_or_else(|e| e.into_inner());
        *stopped = true;
        self.0.cv.notify_all();
    }
}

impl<W: Workspace, R: StageRunner, C: Clock> Engine<W, R, C> {
    pub fn new(config: EngineConfig, workspace: W, runner: R, clock: C) -> Self {
        Self {
            config,
            workspace,
            runner,
            clock,
            model_sources: BTreeMap::new(),
            preflight: Vec::new(),
            extra_secrets: Vec::new(),
            on_final: None,
            evidence: None,
        }
    }

    pub fn with_model_sources(mut self, model_sources: BTreeMap<String, ModelSource>) -> Self {
        self.model_sources = model_sources;
        self
    }

    pub fn with_preflight(mut self, preflight: Vec<ProbeRecord>) -> Self {
        self.preflight = preflight;
        self
    }

    pub fn with_extra_secrets(mut self, extra_secrets: Vec<String>) -> Self {
        self.extra_secrets = extra_secrets;
        self
    }

    pub fn with_evidence_recorder<E>(mut self, recorder: E) -> Self
    where
        E: EvidenceRecorder + Send + 'static,
    {
        self.evidence = Some(Box::new(recorder));
        self
    }

    pub fn with_on_final<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(&RunState) + Send + 'static,
    {
        self.on_final = Some(Box::new(hook));
        self
    }

    pub fn run(mut self) -> RunState {
        let extra_refs: Vec<&str> = self.extra_secrets.iter().map(|s| s.as_str()).collect();
        let extra_secrets = extra_refs.as_slice();
        let start_now_ms = self.clock.now_ms();
        let start_mono_ms = self.clock.monotonic_ms();
        let logs_dir = self.config.run_dir.join("logs");

        let initial_state = RunState {
            schema_version: LOOP_STATE_SCHEMA_VERSION,
            run_id: self.config.run_id.clone(),
            workflow: self.config.workflow.name.clone(),
            status: RunStatus::Running,
            reason: None,
            sandbox: self.config.sandbox_kind.clone(),
            sandbox_exposures: self.config.sandbox_exposures.clone(),
            preflight: self.preflight.clone(),
            current_cycle: 1,
            max_cycles: self.config.workflow.max_cycles,
            branch: None,
            base_commit: self.config.base_commit.clone(),
            engine_pid: Some(std::process::id()),
            engine_start_ticks: read_self_start_ticks(),
            caller: self.config.caller.clone(),
            heartbeat_at_ms: start_now_ms,
            last_activity_at_ms: start_now_ms,
            started_at_ms: start_now_ms,
            finished_at_ms: None,
            executions: Vec::new(),
            files: Vec::new(),
            summary: String::new(),
        };

        let shared = Mutex::new(SharedState {
            run_dir: self.config.run_dir.clone(),
            last_heartbeat_mono_ms: start_mono_ms,
            state: initial_state,
            write_error: None,
        });

        let pulse = |had_activity: bool| {
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            if had_activity {
                guard.state.last_activity_at_ms = self.clock.now_ms();
            }
            let now_mono = self.clock.monotonic_ms();
            if now_mono.saturating_sub(guard.last_heartbeat_mono_ms) >= HEARTBEAT_INTERVAL_MS {
                guard.last_heartbeat_mono_ms = now_mono;
                guard.state.heartbeat_at_ms = self.clock.now_ms();
                if guard.state.status == RunStatus::Running && guard.write_error.is_none() {
                    if let Err(e) = save_state(&guard.run_dir, &guard.state) {
                        guard.write_error = Some(sanitize_reason(
                            &format!("falha ao gravar state.json: {e:#}"),
                            extra_secrets,
                        ));
                    }
                }
            }
        };

        let persist_now = || -> Result<(), String> {
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(err) = guard.write_error.clone() {
                return Err(err);
            }
            let now_mono = self.clock.monotonic_ms();
            guard.last_heartbeat_mono_ms = now_mono;
            guard.state.heartbeat_at_ms = self.clock.now_ms();
            save_state(&guard.run_dir, &guard.state).map_err(|e| {
                sanitize_reason(&format!("falha ao gravar state.json: {e:#}"), extra_secrets)
            })
        };

        // A function, so the lock is released before the caller acts on the error.
        let write_error = || -> Option<String> {
            let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.write_error.clone()
        };

        let mut base_tree: Option<String> = None;
        let mut current_tree: Option<String> = None;

        let hb_signal = HeartbeatSignal {
            stopped: Mutex::new(false),
            cv: Condvar::new(),
        };

        std::thread::scope(|scope| {
            let hb_ref = &hb_signal;
            let pulse_ref = &pulse;
            scope.spawn(move || {
                let mut stopped = hb_ref.stopped.lock().unwrap_or_else(|e| e.into_inner());
                while !*stopped {
                    let (guard, _) = hb_ref
                        .cv
                        .wait_timeout(stopped, Duration::from_secs(10))
                        .unwrap_or_else(|e| e.into_inner());
                    stopped = guard;
                    if *stopped {
                        break;
                    }
                    drop(stopped);
                    pulse_ref(false);
                    stopped = hb_ref.stopped.lock().unwrap_or_else(|e| e.into_inner());
                }
            });
            let _hb_guard = HeartbeatGuard(&hb_signal);

            let end_run = |status: RunStatus, reason: Option<String>, summary: Option<String>| {
                let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                guard.state.status = status;
                if let Some(r) = reason {
                    guard.state.reason = Some(sanitize_reason(&r, extra_secrets));
                }
                if let Some(s) = summary {
                    guard.state.summary = sanitize_text(&s, Some(MAX_SUMMARY_CHARS), extra_secrets);
                }
            };

            if let Err(e) = fs::create_dir_all(&logs_dir) {
                end_run(
                    RunStatus::Error,
                    Some(format!("falha ao criar diretório da run: {e:#}")),
                    None,
                );
                return;
            }
            if let Err(e) = write_text_file(
                &self.config.run_dir.join("input.md"),
                &self.config.input_prompt,
            ) {
                end_run(
                    RunStatus::Error,
                    Some(format!("falha ao gravar input.md: {e:#}")),
                    None,
                );
                return;
            }
            if let Err(err) = persist_now() {
                end_run(RunStatus::Error, Some(err), None);
                return;
            }

            match self.workspace.snapshot() {
                Ok(tree) => {
                    base_tree = Some(tree.clone());
                    current_tree = Some(tree);
                }
                Err(e) => {
                    end_run(
                        RunStatus::Error,
                        Some(format!("falha ao capturar snapshot inicial: {e:#}")),
                        None,
                    );
                    return;
                }
            }

            if self.config.workflow.stages.is_empty() {
                end_run(
                    RunStatus::Error,
                    Some("workflow sem etapas".to_string()),
                    None,
                );
                return;
            }

            let stages_by_id: BTreeMap<&str, &Stage> = self
                .config
                .workflow
                .stages
                .iter()
                .map(|s| (s.id.as_str(), s))
                .collect();

            let mut current_stage_id = self.config.workflow.stages[0].id.clone();
            let mut current_cycle: u32 = 1;
            let mut exec_seq: usize = 0;
            let mut malformed_retried = false;
            let max_dur_ms = self.config.workflow.max_duration_secs.saturating_mul(1000);
            let stage_timeout_ms = self.config.workflow.stage_timeout_secs.saturating_mul(1000);
            let stall_ms = self.config.workflow.stall_secs.saturating_mul(1000);

            loop {
                if let Some(err) = write_error() {
                    end_run(RunStatus::Error, Some(err), None);
                    break;
                }

                if self.config.cancel.load(Ordering::Relaxed) {
                    end_run(
                        RunStatus::Cancelled,
                        Some("execução cancelada".to_string()),
                        None,
                    );
                    break;
                }

                let elapsed_ms = self.clock.monotonic_ms().saturating_sub(start_mono_ms);
                if elapsed_ms >= max_dur_ms {
                    end_run(
                        RunStatus::Escalate,
                        Some("tempo máximo do loop atingido (MAX_DURATION)".to_string()),
                        None,
                    );
                    break;
                }

                let Some(&stage) = stages_by_id.get(current_stage_id.as_str()) else {
                    end_run(
                        RunStatus::Error,
                        Some(format!("etapa desconhecida '{current_stage_id}'")),
                        None,
                    );
                    break;
                };

                let Some(role_text) = self.config.roles.get(&stage.role) else {
                    end_run(
                        RunStatus::Error,
                        Some(format!("role '{}' não encontrado", stage.role)),
                        None,
                    );
                    break;
                };

                if stage.reads.iter().any(|r| r == "current.diff") {
                    let Some(tree) = current_tree.as_deref() else {
                        end_run(
                            RunStatus::Error,
                            Some("árvore atual ausente ao gerar current.diff".to_string()),
                            None,
                        );
                        break;
                    };
                    match self.workspace.diff(tree) {
                        Ok(diff_text) => {
                            if let Err(e) = write_text_file(
                                &self.config.run_dir.join("current.diff"),
                                &diff_text,
                            ) {
                                end_run(
                                    RunStatus::Error,
                                    Some(format!("falha ao gravar current.diff: {e:#}")),
                                    None,
                                );
                                break;
                            }
                        }
                        Err(e) => {
                            end_run(
                                RunStatus::Error,
                                Some(format!("falha ao gerar current.diff: {e:#}")),
                                None,
                            );
                            break;
                        }
                    }
                }

                let mut available_reads = Vec::new();
                let mut reads_to_copy = Vec::new();
                for name in &stage.reads {
                    let path = self.config.run_dir.join(name);
                    if path.is_file() {
                        available_reads.push(name.clone());
                        reads_to_copy.push((name.clone(), path));
                    }
                }

                if let Err(e) = prepare_exchange(self.workspace.path(), &reads_to_copy) {
                    end_run(
                        RunStatus::Error,
                        Some(format!(
                            "falha ao preparar artefatos de entrada para '{}': {e:#}",
                            stage.id
                        )),
                        None,
                    );
                    break;
                }

                let prompt = build_prompt(role_text, stage, &available_reads);
                exec_seq += 1;
                let log_rel = format!("logs/c{current_cycle}-{exec_seq:02}-{}.log", stage.id);
                let log_path = self.config.run_dir.join(&log_rel);

                {
                    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                    guard.state.current_cycle = current_cycle;
                }
                if let Err(err) = persist_now() {
                    end_run(RunStatus::Error, Some(err), None);
                    break;
                }

                let remaining_ms = max_dur_ms.saturating_sub(elapsed_ms);
                let effective_timeout_ms = stage_timeout_ms.min(remaining_ms);
                let stage_started_now = self.clock.now_ms();
                let stage_started_mono = self.clock.monotonic_ms();

                let result = self.runner.run(StageRequest {
                    run_id: &self.config.run_id,
                    cycle: current_cycle,
                    exec_seq,
                    stage,
                    workspace: self.workspace.path(),
                    prompt: &prompt,
                    log_path: &log_path,
                    limits: Limits {
                        timeout: Duration::from_millis(effective_timeout_ms),
                        stall: Duration::from_millis(stall_ms),
                    },
                    tool_stall: Duration::from_millis(
                        self.config.workflow.tool_stall_secs.saturating_mul(1000),
                    ),
                    cancel: &self.config.cancel,
                    pulse: &pulse,
                });

                let stage_finished_now = self.clock.now_ms();
                let stage_duration_ms =
                    self.clock.monotonic_ms().saturating_sub(stage_started_mono);

                if let Some(rec) = self.evidence.as_mut() {
                    match result.outcome {
                        StageOutcome::Ok if result.error.is_none() => {
                            rec.record(stage.provider, stage.model.as_deref(), stage_finished_now);
                        }
                        StageOutcome::Cancelled => {}
                        _ => {
                            rec.discard(stage.provider, stage.model.as_deref());
                        }
                    }
                }

                // A stage that did not finish well: an editor's work is kept for the branch, and a
                // reader's edits are undone. Best effort, the run is ending anyway.
                let finished = result.outcome == StageOutcome::Ok && result.error.is_none();
                if !finished {
                    if let Ok(tree) = self.workspace.snapshot() {
                        if stage.edits {
                            current_tree = Some(tree);
                        } else if let Some(prev) = current_tree.as_deref() {
                            if tree != prev {
                                let _ = self.workspace.restore(prev);
                            }
                        }
                    }
                }

                let model_source = self.model_sources.get(&stage.id).copied();

                if let Some(err) = write_error() {
                    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                    guard.state.executions.push(Execution {
                        cycle: current_cycle,
                        stage_id: stage.id.clone(),
                        role: stage.role.clone(),
                        provider: stage.provider,
                        model: stage.model.clone(),
                        model_source,
                        started_at_ms: stage_started_now,
                        finished_at_ms: Some(stage_finished_now),
                        duration_ms: stage_duration_ms,
                        usage: result.usage,
                        verdict: None,
                        outcome: "ERROR".to_string(),
                        error: Some(err.clone()),
                        log_file: Some(log_rel),
                        stop: result
                            .stop
                            .clone()
                            .map(|stop| sanitize_stop(stop, extra_secrets)),
                        tool_silence: result.tool_silence,
                    });
                    drop(guard);
                    end_run(RunStatus::Error, Some(err), None);
                    break;
                }

                let push_exec = |outcome: &str, verdict: Option<_>, err: Option<String>| {
                    let clean_err = err.map(|e| sanitize_reason(&e, extra_secrets));
                    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                    guard.state.executions.push(Execution {
                        cycle: current_cycle,
                        stage_id: stage.id.clone(),
                        role: stage.role.clone(),
                        provider: stage.provider,
                        model: stage.model.clone(),
                        model_source,
                        started_at_ms: stage_started_now,
                        finished_at_ms: Some(stage_finished_now),
                        duration_ms: stage_duration_ms,
                        usage: result.usage.clone(),
                        verdict,
                        outcome: outcome.to_string(),
                        error: clean_err,
                        stop: result
                            .stop
                            .clone()
                            .map(|stop| sanitize_stop(stop, extra_secrets)),
                        log_file: Some(log_rel.clone()),
                        tool_silence: result.tool_silence,
                    });
                };

                // The hint names what was likely still holding the stage, so the reason explains
                // a wedge without opening state.json.
                let with_hint = |base: &str| match result.stop.as_ref() {
                    Some(stop) => format!("{base}: {}", stop.hint()),
                    None => base.to_string(),
                };

                match result.outcome {
                    StageOutcome::Cancelled => {
                        push_exec("CANCELLED", None, result.error);
                        end_run(
                            RunStatus::Cancelled,
                            Some("execução cancelada".to_string()),
                            None,
                        );
                        break;
                    }
                    StageOutcome::Stalled => {
                        push_exec("STALLED", None, result.error);
                        let base = if result.stop.as_ref().and_then(|s| s.limit)
                            == Some(StopLimit::ToolStall)
                        {
                            "etapa travada sem atividade com ferramenta aberta (STALLED, tool_stall_secs)"
                        } else {
                            "etapa travada sem atividade (STALLED)"
                        };
                        end_run(RunStatus::Escalate, Some(with_hint(base)), None);
                        break;
                    }
                    StageOutcome::TimedOut => {
                        let total_elapsed = self.clock.monotonic_ms().saturating_sub(start_mono_ms);
                        if total_elapsed >= max_dur_ms {
                            push_exec("MAX_DURATION", None, result.error);
                            end_run(
                                RunStatus::Escalate,
                                Some(with_hint("tempo máximo do loop atingido (MAX_DURATION)")),
                                None,
                            );
                        } else {
                            push_exec("TIMEOUT", None, result.error);
                            end_run(
                                RunStatus::Escalate,
                                Some(with_hint("tempo limite da etapa atingido (STAGE_TIMEOUT)")),
                                None,
                            );
                        }
                        break;
                    }
                    StageOutcome::NonZeroExit(code) => {
                        let msg = result.error.unwrap_or_else(|| {
                            format!("etapa '{}' retornou código {code}", stage.id)
                        });
                        push_exec("ERROR", None, Some(msg.clone()));
                        end_run(RunStatus::Error, Some(msg), None);
                        break;
                    }
                    StageOutcome::Signaled(sig) => {
                        let msg = result.error.unwrap_or_else(|| {
                            format!("etapa '{}' encerrada pelo sinal {sig}", stage.id)
                        });
                        push_exec("ERROR", None, Some(msg.clone()));
                        end_run(RunStatus::Error, Some(msg), None);
                        break;
                    }
                    StageOutcome::Error => {
                        let msg = result
                            .error
                            .unwrap_or_else(|| format!("falha na etapa '{}'", stage.id));
                        push_exec("ERROR", None, Some(msg.clone()));
                        end_run(RunStatus::Error, Some(msg), None);
                        break;
                    }
                    StageOutcome::Ok if result.error.is_some() => {
                        let msg = result.error.clone().unwrap_or_default();
                        push_exec("ERROR", None, Some(msg.clone()));
                        end_run(RunStatus::Error, Some(msg), None);
                        break;
                    }
                    StageOutcome::Ok => {}
                }

                // Collect declared output artifact.
                let mut collected_output: Option<String> = None;
                if let Some(out_name) = &stage.writes {
                    match collect_output(self.workspace.path(), out_name) {
                        Ok(content) => {
                            if let Err(e) =
                                write_text_file(&self.config.run_dir.join(out_name), &content)
                            {
                                let msg = format!("falha ao salvar artefato '{out_name}': {e:#}");
                                push_exec("ERROR", None, Some(msg.clone()));
                                end_run(RunStatus::Error, Some(msg), None);
                                break;
                            }
                            let archive = format!("c{current_cycle}-{out_name}");
                            if let Err(e) =
                                write_text_file(&self.config.run_dir.join(&archive), &content)
                            {
                                let msg = format!("falha ao arquivar artefato '{archive}': {e:#}");
                                push_exec("ERROR", None, Some(msg.clone()));
                                end_run(RunStatus::Error, Some(msg), None);
                                break;
                            }
                            collected_output = Some(content);
                        }
                        Err(e) => {
                            // If a read-only stage also touched the workspace, revert before failing.
                            if !stage.edits {
                                if let (Some(prev), Ok(snap)) =
                                    (current_tree.as_deref(), self.workspace.snapshot())
                                {
                                    if snap != prev {
                                        let _ = self.workspace.restore(prev);
                                    }
                                }
                            }
                            let msg = format!(
                                "falha ao recolher saída '{}' da etapa '{}': {e:#}",
                                out_name, stage.id
                            );
                            push_exec("ERROR", None, Some(msg.clone()));
                            end_run(RunStatus::Error, Some(msg), None);
                            break;
                        }
                    }
                }

                // Snapshot and enforce `edits = false`.
                let prev_tree = current_tree.as_deref().unwrap_or_default().to_string();
                match self.workspace.snapshot() {
                    Ok(new_tree) => {
                        if !stage.edits && new_tree != prev_tree {
                            let restore_err = self.workspace.restore(&prev_tree).err();
                            let msg = match restore_err {
                                Some(re) => format!(
                                    "etapa '{}' (edits=false) alterou o workspace e falhou ao restaurar: {re:#}",
                                    stage.id
                                ),
                                None => format!(
                                    "etapa '{}' (edits=false) alterou o workspace; alterações revertidas",
                                    stage.id
                                ),
                            };
                            push_exec("ERROR", None, Some(msg.clone()));
                            end_run(RunStatus::Error, Some(msg), None);
                            break;
                        }
                        current_tree = Some(new_tree);
                    }
                    Err(e) => {
                        let msg = format!("falha ao capturar snapshot após '{}': {e:#}", stage.id);
                        push_exec("ERROR", None, Some(msg.clone()));
                        end_run(RunStatus::Error, Some(msg), None);
                        break;
                    }
                }

                if stage.verdict_gate {
                    let output_text = collected_output.as_deref().unwrap_or("");
                    match parse_verdict(output_text) {
                        Some(crate::loops::verdict::Verdict::Pass) => {
                            push_exec("PASS", Some(crate::loops::verdict::Verdict::Pass), None);
                            let sum = summary_line(output_text);
                            match stage.on_pass.as_deref() {
                                Some("done") => {
                                    end_run(RunStatus::Pass, None, Some(sum));
                                    break;
                                }
                                Some(target) => {
                                    current_stage_id = target.to_string();
                                    malformed_retried = false;
                                }
                                None => {
                                    end_run(
                                        RunStatus::Error,
                                        Some(format!("etapa '{}' sem 'on_pass'", stage.id)),
                                        None,
                                    );
                                    break;
                                }
                            }
                        }
                        Some(crate::loops::verdict::Verdict::Retry) => {
                            push_exec("RETRY", Some(crate::loops::verdict::Verdict::Retry), None);
                            let sum = summary_line(output_text);
                            if current_cycle >= self.config.workflow.max_cycles {
                                end_run(
                                    RunStatus::Escalate,
                                    Some(format!(
                                        "máximo de ciclos atingido ({}/{})",
                                        current_cycle, self.config.workflow.max_cycles
                                    )),
                                    (!sum.is_empty()).then_some(sum),
                                );
                                break;
                            }
                            match stage.on_retry.as_deref() {
                                Some(target) if target != "done" => {
                                    current_cycle += 1;
                                    current_stage_id = target.to_string();
                                    malformed_retried = false;
                                }
                                _ => {
                                    end_run(
                                        RunStatus::Error,
                                        Some(format!(
                                            "etapa '{}' com 'on_retry' inválido",
                                            stage.id
                                        )),
                                        None,
                                    );
                                    break;
                                }
                            }
                        }
                        None => {
                            let err_msg = "veredito ausente ou malformado".to_string();
                            push_exec("MALFORMED_VERDICT", None, Some(err_msg));
                            if !malformed_retried {
                                malformed_retried = true;
                            } else {
                                end_run(
                                    RunStatus::Error,
                                    Some(
                                        "veredito ausente ou malformado após nova tentativa"
                                            .to_string(),
                                    ),
                                    None,
                                );
                                break;
                            }
                        }
                    }
                } else {
                    push_exec("OK", None, None);
                    match stage.next.as_deref() {
                        Some(target) if target != "done" => {
                            current_stage_id = target.to_string();
                            malformed_retried = false;
                        }
                        _ => {
                            end_run(
                                RunStatus::Error,
                                Some(format!("etapa '{}' sem 'next' válido", stage.id)),
                                None,
                            );
                            break;
                        }
                    }
                }

                if let Err(err) = persist_now() {
                    end_run(RunStatus::Error, Some(err), None);
                    break;
                }
            }
        });

        // Background heartbeat thread has stopped and joined; finalize safely.
        let mut final_state = shared.into_inner().unwrap_or_else(|e| e.into_inner()).state;

        if let (Some(base), Some(cur)) = (base_tree.as_deref(), current_tree.as_deref()) {
            match self.workspace.changed_files(cur) {
                Ok(files) => {
                    final_state.files = files
                        .into_iter()
                        .map(|f| sanitize_text(&f, None, extra_secrets))
                        .filter(|f| !f.is_empty())
                        .collect();
                    if !final_state.files.is_empty() || cur != base {
                        let commit_msg = format!(
                            "loop({}): {} [{}]",
                            final_state.workflow, final_state.run_id, final_state.status
                        );
                        match self.workspace.publish(cur, &commit_msg) {
                            Ok(()) => {
                                final_state.branch = Some(self.config.branch.clone());
                            }
                            Err(e) => {
                                let pub_err = sanitize_reason(
                                    &format!(
                                        "falha ao publicar branch '{}': {e:#}",
                                        self.config.branch
                                    ),
                                    extra_secrets,
                                );
                                if final_state.status == RunStatus::Pass {
                                    final_state.status = RunStatus::Error;
                                    final_state.reason = Some(pub_err);
                                } else if final_state.reason.is_none() {
                                    final_state.reason = Some(pub_err);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let diff_err = sanitize_reason(
                        &format!("falha ao listar arquivos alterados: {e:#}"),
                        extra_secrets,
                    );
                    if final_state.status == RunStatus::Pass {
                        final_state.status = RunStatus::Error;
                        final_state.reason = Some(diff_err);
                    }
                }
            }

            if let Ok(diff_text) = self.workspace.diff(cur) {
                let _ = write_text_file(&self.config.run_dir.join("current.diff"), &diff_text);
            }
        }

        let finished_now = self.clock.now_ms();
        final_state.finished_at_ms = Some(finished_now);
        final_state.heartbeat_at_ms = finished_now;

        let report_md =
            render_report_with_secrets(&final_state, &self.config.run_dir, extra_secrets);
        let _ = write_text_file(&self.config.run_dir.join("report.md"), &report_md);

        if let Err(e) = save_state(&self.config.run_dir, &final_state) {
            final_state.status = RunStatus::Error;
            if final_state.reason.is_none() {
                final_state.reason = Some(sanitize_reason(
                    &format!("falha ao gravar state.json final: {e:#}"),
                    extra_secrets,
                ));
            }
        }

        // Only now: whoever is told must find the result on disk.
        if let Some(hook) = self.on_final.take() {
            hook(&final_state);
        }

        final_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::artifacts::{EXCHANGE_DIR, IN_DIR, OUT_DIR};
    use crate::loops::process::SystemClock;
    use crate::loops::state::{load_state, sum_usage, StopLimit, ToolSource, Total};
    use crate::loops::workflow::{parse_workflow, Provider, RoleDefault, RoleDefaults};
    use anyhow::Result;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct FakeClock {
        now_ms: Mutex<i64>,
        mono_ms: Mutex<u64>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now_ms: Mutex::new(1_700_000_000_000),
                mono_ms: Mutex::new(1_000),
            }
        }

        fn advance(&self, delta_ms: u64) {
            *self.now_ms.lock().unwrap() += delta_ms as i64;
            *self.mono_ms.lock().unwrap() += delta_ms;
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            *self.now_ms.lock().unwrap()
        }

        fn monotonic_ms(&self) -> u64 {
            *self.mono_ms.lock().unwrap()
        }
    }

    impl Clock for Arc<FakeClock> {
        fn now_ms(&self) -> i64 {
            FakeClock::now_ms(self)
        }

        fn monotonic_ms(&self) -> u64 {
            FakeClock::monotonic_ms(self)
        }
    }

    struct FakeWorkspace {
        root: PathBuf,
        base_tree: String,
        current_files: BTreeMap<String, String>,
        published: Vec<(String, String)>,
        restores: Vec<String>,
        snapshot_counter: usize,
    }

    impl FakeWorkspace {
        fn new(root: PathBuf) -> Self {
            fs::create_dir_all(&root).unwrap();
            Self {
                root,
                base_tree: "tree-0".to_string(),
                current_files: BTreeMap::new(),
                published: Vec::new(),
                restores: Vec::new(),
                snapshot_counter: 0,
            }
        }

        fn compute_tree(&self) -> String {
            if self.current_files.is_empty() {
                return self.base_tree.clone();
            }
            let mut s = String::new();
            for (k, v) in &self.current_files {
                s.push_str(k);
                s.push('=');
                s.push_str(v);
                s.push(';');
            }
            format!("tree-{:x}-{}", s.len(), s)
        }

        fn sync_disk(&mut self) {
            // Sync tracked files in `root` into `current_files` (excluding `.tchelvis-loop`).
            let mut map = BTreeMap::new();
            if let Ok(entries) = fs::read_dir(&self.root) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == EXCHANGE_DIR || name == ".git" {
                        continue;
                    }
                    if entry.path().is_file() {
                        if let Ok(content) = fs::read_to_string(entry.path()) {
                            map.insert(name, content);
                        }
                    }
                }
            }
            self.current_files = map;
        }
    }

    impl Workspace for FakeWorkspace {
        fn snapshot(&mut self) -> Result<String> {
            self.snapshot_counter += 1;
            self.sync_disk();
            Ok(self.compute_tree())
        }

        fn restore(&mut self, tree: &str) -> Result<()> {
            self.restores.push(tree.to_string());
            if let Ok(entries) = fs::read_dir(&self.root) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == EXCHANGE_DIR || name == ".git" {
                        continue;
                    }
                    let _ = fs::remove_file(entry.path());
                }
            }
            if let Some((_, pairs)) = tree.strip_prefix("tree-").and_then(|s| s.split_once('-')) {
                for item in pairs.split(';').filter(|s| !s.is_empty()) {
                    if let Some((k, v)) = item.split_once('=') {
                        fs::write(self.root.join(k), v)?;
                    }
                }
            }
            self.sync_disk();
            Ok(())
        }

        fn diff(&self, tree: &str) -> Result<String> {
            Ok(format!("diff against {tree}"))
        }

        fn changed_files(&self, _tree: &str) -> Result<Vec<String>> {
            Ok(self.current_files.keys().cloned().collect())
        }

        fn publish(&mut self, tree: &str, message: &str) -> Result<()> {
            self.published.push((tree.to_string(), message.to_string()));
            Ok(())
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    type StageAction = Box<dyn FnMut(&StageRequest<'_>) -> StageResult>;

    struct ScriptedRunner {
        actions: VecDeque<StageAction>,
        recorded_prompts: Vec<(String, u32, String)>,
        recorded_ins: Vec<(String, u32, BTreeMap<String, String>)>,
    }

    impl ScriptedRunner {
        fn new() -> Self {
            Self {
                actions: VecDeque::new(),
                recorded_prompts: Vec::new(),
                recorded_ins: Vec::new(),
            }
        }

        fn push(&mut self, f: impl FnMut(&StageRequest<'_>) -> StageResult + 'static) {
            self.actions.push_back(Box::new(f));
        }
    }

    impl StageRunner for ScriptedRunner {
        fn run(&mut self, req: StageRequest<'_>) -> StageResult {
            self.recorded_prompts
                .push((req.stage.id.clone(), req.cycle, req.prompt.to_string()));
            let mut ins = BTreeMap::new();
            let in_dir = req.workspace.join(EXCHANGE_DIR).join(IN_DIR);
            if let Ok(entries) = fs::read_dir(&in_dir) {
                for entry in entries.flatten() {
                    if let Ok(text) = fs::read_to_string(entry.path()) {
                        ins.insert(entry.file_name().to_string_lossy().into_owned(), text);
                    }
                }
            }
            self.recorded_ins
                .push((req.stage.id.clone(), req.cycle, ins));

            let mut action = self
                .actions
                .pop_front()
                .expect("unexpected extra stage execution in ScriptedRunner");
            action(&req)
        }
    }

    fn default_roles() -> BTreeMap<String, String> {
        let mut roles = BTreeMap::new();
        roles.insert("worker".to_string(), "Voce e o worker.".to_string());
        roles.insert("reviewer".to_string(), "Voce e o reviewer.".to_string());
        roles.insert("critic".to_string(), "Voce e o critic.".to_string());
        roles
    }

    fn wrc_workflow(max_cycles: u32) -> Workflow {
        let mut defaults: RoleDefaults = BTreeMap::new();
        for role in ["worker", "reviewer", "critic"] {
            defaults.insert(
                role.to_string(),
                RoleDefault {
                    provider: Provider::Claude,
                    model: None,
                },
            );
        }
        let raw = format!(
            r#"
name = "worker-reviewer-critic"
description = "test"
max_cycles = {max_cycles}
stage_timeout_secs = 60
stall_secs = 30
max_duration_secs = 300

[[stages]]
id = "implement"
role = "worker"
reads = ["input.md", "review.md", "critic.md"]
edits = true
next = "review"

[[stages]]
id = "review"
role = "reviewer"
reads = ["input.md", "current.diff"]
writes = "review.md"
next = "audit"

[[stages]]
id = "audit"
role = "critic"
reads = ["input.md", "current.diff", "review.md"]
writes = "critic.md"
verdict_gate = true
on_pass = "done"
on_retry = "implement"
"#
        );
        parse_workflow(&raw, &defaults).unwrap()
    }

    fn make_config(tmp: &TempDir, wf: Workflow) -> EngineConfig {
        EngineConfig {
            run_id: "20261123-182500-a1b2".to_string(),
            workflow: wf,
            roles: default_roles(),
            run_dir: tmp.path().join("run"),
            input_prompt: "Implemente X".to_string(),
            sandbox_kind: "bwrap".to_string(),
            sandbox_exposures: vec!["rede compartilhada".to_string()],
            branch: "loop/worker-reviewer-critic-20261123-182500-a1b2".to_string(),
            base_commit: Some("deadbeef".to_string()),
            caller: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn write_out(ws: &Path, name: &str, content: &str) {
        fs::write(ws.join(EXCHANGE_DIR).join(OUT_DIR).join(name), content).unwrap();
    }

    #[test]
    fn pass_on_first_cycle() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();

        runner.push(|req| {
            fs::write(req.workspace.join("a.rs"), "pub fn a() {}\n").unwrap();
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(2),
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    cache_read_tokens: Some(50),
                    cost_usd: Some(0.01),
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });
        runner.push(|req| {
            write_out(req.workspace, "review.md", "Sem bugs.\n");
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(1),
                    input_tokens: Some(50),
                    output_tokens: Some(10),
                    cache_read_tokens: Some(0),
                    cost_usd: Some(0.005),
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });
        runner.push(|req| {
            write_out(
                req.workspace,
                "critic.md",
                "Aprovado no ciclo 1.\nVERDICT: PASS\n",
            );
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(1),
                    input_tokens: Some(60),
                    output_tokens: Some(15),
                    cache_read_tokens: Some(10),
                    cost_usd: Some(0.005),
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });

        let cfg = make_config(&tmp, wrc_workflow(3));
        let run_dir = cfg.run_dir.clone();
        let state = Engine::new(cfg, ws, runner, FakeClock::new()).run();

        assert_eq!(state.status, RunStatus::Pass);
        assert_eq!(state.current_cycle, 1);
        assert_eq!(
            state.branch.as_deref(),
            Some("loop/worker-reviewer-critic-20261123-182500-a1b2")
        );
        assert_eq!(state.files, vec!["a.rs".to_string()]);
        assert_eq!(state.summary, "Aprovado no ciclo 1.");
        assert_eq!(state.executions.len(), 3);
        assert!(run_dir.join("state.json").is_file());
        assert!(run_dir.join("report.md").is_file());
        assert!(run_dir.join("c1-review.md").is_file());
        assert!(run_dir.join("c1-critic.md").is_file());
    }

    #[test]
    fn retry_then_pass_sums_usage() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();

        // Cycle 1: worker, reviewer, critic (RETRY)
        runner.push(|req| {
            fs::write(req.workspace.join("a.rs"), "v1\n").unwrap();
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(3),
                    input_tokens: Some(1000),
                    output_tokens: Some(100),
                    cache_read_tokens: Some(200),
                    cost_usd: Some(0.10),
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });
        runner.push(|req| {
            write_out(req.workspace, "review.md", "Falta tratamento de erro.\n");
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(1),
                    input_tokens: Some(500),
                    output_tokens: Some(50),
                    cache_read_tokens: None,
                    cost_usd: None,
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });
        runner.push(|req| {
            write_out(
                req.workspace,
                "critic.md",
                "Corrigir erro.\nVERDICT: RETRY\n",
            );
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(1),
                    input_tokens: Some(400),
                    output_tokens: Some(40),
                    cache_read_tokens: Some(100),
                    cost_usd: Some(0.05),
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });

        // Cycle 2: worker, reviewer, critic (PASS)
        runner.push(|req| {
            fs::write(req.workspace.join("a.rs"), "v2\n").unwrap();
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(2),
                    input_tokens: Some(800),
                    output_tokens: Some(80),
                    cache_read_tokens: Some(300),
                    cost_usd: Some(0.08),
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });
        runner.push(|req| {
            write_out(req.workspace, "review.md", "Tudo certo agora.\n");
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(1),
                    input_tokens: Some(300),
                    output_tokens: Some(30),
                    cache_read_tokens: None,
                    cost_usd: None,
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });
        runner.push(|req| {
            write_out(req.workspace, "critic.md", "Aprovado.\nVERDICT: PASS\n");
            StageResult {
                outcome: StageOutcome::Ok,
                usage: Usage {
                    turns: Some(1),
                    input_tokens: Some(200),
                    output_tokens: Some(20),
                    cache_read_tokens: Some(50),
                    cost_usd: Some(0.02),
                },
                error: None,
                stop: None,
                tool_silence: None,
            }
        });

        let state = Engine::new(
            make_config(&tmp, wrc_workflow(3)),
            ws,
            runner,
            FakeClock::new(),
        )
        .run();
        assert_eq!(state.status, RunStatus::Pass);
        assert_eq!(state.current_cycle, 2);
        assert_eq!(state.executions.len(), 6);

        let totals = sum_usage(&state.executions);
        assert_eq!(
            totals.turns,
            Total {
                value: 9,
                partial: false
            }
        );
        assert_eq!(
            totals.input_tokens,
            Total {
                value: 3200,
                partial: false
            }
        );
        assert_eq!(
            totals.output_tokens,
            Total {
                value: 320,
                partial: false
            }
        );
        assert_eq!(
            totals.cache_read_tokens,
            Total {
                value: 650,
                partial: true
            }
        );
        let cost = totals.cost_usd.unwrap();
        assert!((cost.value - 0.25).abs() < 1e-9);
        assert!(cost.partial);
    }

    #[test]
    fn third_retry_escalates_and_runs_exactly_three_gates() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();

        for c in 1..=3 {
            runner.push(move |req| {
                fs::write(req.workspace.join("a.rs"), format!("try {c}\n")).unwrap();
                StageResult::default()
            });
            runner.push(move |req| {
                write_out(req.workspace, "review.md", &format!("review {c}\n"));
                StageResult::default()
            });
            runner.push(move |req| {
                write_out(
                    req.workspace,
                    "critic.md",
                    &format!("ainda falha {c}\nVERDICT: RETRY\n"),
                );
                StageResult::default()
            });
        }

        let state = Engine::new(
            make_config(&tmp, wrc_workflow(3)),
            ws,
            runner,
            FakeClock::new(),
        )
        .run();
        assert_eq!(state.status, RunStatus::Escalate);
        assert_eq!(state.current_cycle, 3);
        let gates = state
            .executions
            .iter()
            .filter(|e| e.stage_id == "audit")
            .count();
        assert_eq!(gates, 3);
        assert!(state.branch.is_some());
    }

    #[test]
    fn malformed_verdict_reruns_only_the_gate_once_then_errors() {
        // Case 1: first malformed, rerun passes -> PASS without re-running worker/reviewer
        {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let mut runner = ScriptedRunner::new();

            runner.push(|req| {
                fs::write(req.workspace.join("a.rs"), "code\n").unwrap();
                StageResult::default()
            });
            runner.push(|req| {
                write_out(req.workspace, "review.md", "ok\n");
                StageResult::default()
            });
            runner.push(|req| {
                write_out(req.workspace, "critic.md", "Esqueci o veredito.\n");
                StageResult::default()
            });
            runner.push(|req| {
                write_out(req.workspace, "critic.md", "Agora sim.\nVERDICT: PASS\n");
                StageResult::default()
            });

            let state = Engine::new(
                make_config(&tmp, wrc_workflow(3)),
                ws,
                runner,
                FakeClock::new(),
            )
            .run();
            assert_eq!(state.status, RunStatus::Pass);
            let ids: Vec<&str> = state
                .executions
                .iter()
                .map(|e| e.stage_id.as_str())
                .collect();
            assert_eq!(ids, vec!["implement", "review", "audit", "audit"]);
            assert_eq!(state.current_cycle, 1);
        }

        // Case 2: malformed twice -> ERROR without running a 3rd time
        {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let mut runner = ScriptedRunner::new();

            runner.push(|req| {
                fs::write(req.workspace.join("a.rs"), "code\n").unwrap();
                StageResult::default()
            });
            runner.push(|req| {
                write_out(req.workspace, "review.md", "ok\n");
                StageResult::default()
            });
            runner.push(|req| {
                write_out(req.workspace, "critic.md", "Sem veredito 1\n");
                StageResult::default()
            });
            runner.push(|req| {
                write_out(req.workspace, "critic.md", "VERDICT: MAYBE\n");
                StageResult::default()
            });

            let state = Engine::new(
                make_config(&tmp, wrc_workflow(3)),
                ws,
                runner,
                FakeClock::new(),
            )
            .run();
            assert_eq!(state.status, RunStatus::Error);
            let ids: Vec<&str> = state
                .executions
                .iter()
                .map(|e| e.stage_id.as_str())
                .collect();
            assert_eq!(ids, vec!["implement", "review", "audit", "audit"]);
            assert!(state.reason.is_some());
        }
    }

    #[test]
    fn missing_output_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();

        runner.push(|_| StageResult::default()); // worker
        runner.push(|_| StageResult::default()); // reviewer does NOT write review.md

        let state = Engine::new(
            make_config(&tmp, wrc_workflow(3)),
            ws,
            runner,
            FakeClock::new(),
        )
        .run();
        assert_eq!(state.status, RunStatus::Error);
        assert!(state.reason.is_some());
    }

    #[test]
    fn reviewer_that_edits_is_reverted_and_errors() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let ws_path = ws.path().to_path_buf();
        let mut runner = ScriptedRunner::new();

        runner.push(|req| {
            fs::write(req.workspace.join("a.rs"), "clean\n").unwrap();
            StageResult::default()
        });
        runner.push(|req| {
            // Reviewer illegally edits workspace file
            fs::write(req.workspace.join("a.rs"), "tampered\n").unwrap();
            fs::write(req.workspace.join("extra.rs"), "sneaky\n").unwrap();
            write_out(req.workspace, "review.md", "ok\n");
            StageResult::default()
        });

        let state = Engine::new(
            make_config(&tmp, wrc_workflow(3)),
            ws,
            runner,
            FakeClock::new(),
        )
        .run();
        assert_eq!(state.status, RunStatus::Error);
        assert_eq!(fs::read_to_string(ws_path.join("a.rs")).unwrap(), "clean\n");
        assert!(!ws_path.join("extra.rs").exists());
        assert_eq!(state.files, vec!["a.rs".to_string()]);
    }

    #[test]
    fn stall_timeout_and_max_duration_escalate_with_reason() {
        // Stall
        {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let mut runner = ScriptedRunner::new();
            runner.push(|_| StageResult {
                outcome: StageOutcome::Stalled,
                ..Default::default()
            });
            let state = Engine::new(
                make_config(&tmp, wrc_workflow(3)),
                ws,
                runner,
                FakeClock::new(),
            )
            .run();
            assert_eq!(state.status, RunStatus::Escalate);
            assert!(state.reason.as_deref().unwrap_or("").contains("STALLED"));
        }

        // Stage timeout
        {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let mut runner = ScriptedRunner::new();
            runner.push(|_| StageResult {
                outcome: StageOutcome::TimedOut,
                ..Default::default()
            });
            let state = Engine::new(
                make_config(&tmp, wrc_workflow(3)),
                ws,
                runner,
                FakeClock::new(),
            )
            .run();
            assert_eq!(state.status, RunStatus::Escalate);
            assert!(state
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("STAGE_TIMEOUT"));
        }

        // Max duration
        {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let clock = Arc::new(FakeClock::new());
            let clock_for_stage = Arc::clone(&clock);
            let mut runner = ScriptedRunner::new();
            runner.push(move |_| {
                clock_for_stage.advance(301_000);
                StageResult::default()
            });
            let state = Engine::new(make_config(&tmp, wrc_workflow(3)), ws, runner, clock).run();
            assert_eq!(state.status, RunStatus::Escalate);
            assert!(state
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("MAX_DURATION"));
        }
    }

    #[test]
    fn cancel_ends_as_cancelled() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();
        let cfg = make_config(&tmp, wrc_workflow(3));
        let cancel = Arc::clone(&cfg.cancel);

        runner.push(move |req| {
            fs::write(req.workspace.join("a.rs"), "partial\n").unwrap();
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            StageResult {
                outcome: StageOutcome::Cancelled,
                ..Default::default()
            }
        });

        let state = Engine::new(cfg, ws, runner, FakeClock::new()).run();
        assert_eq!(state.status, RunStatus::Cancelled);
        assert!(state.branch.is_some());
    }

    #[test]
    fn nonzero_exit_or_adapter_error_is_error() {
        // NonZeroExit
        {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let mut runner = ScriptedRunner::new();
            runner.push(|_| StageResult {
                outcome: StageOutcome::NonZeroExit(2),
                error: Some("cli falhou\nAuthorization: Bearer tok".to_string()),
                ..Default::default()
            });
            let state = Engine::new(
                make_config(&tmp, wrc_workflow(3)),
                ws,
                runner,
                FakeClock::new(),
            )
            .run();
            assert_eq!(state.status, RunStatus::Error);
            let reason = state.reason.unwrap();
            assert!(!reason.contains("tok"));
            assert!(reason.contains("[REDACTED]"));
        }

        // Adapter error with Ok outcome
        {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let mut runner = ScriptedRunner::new();
            runner.push(|_| StageResult {
                outcome: StageOutcome::Error,
                error: Some("modelo invalido".to_string()),
                ..Default::default()
            });
            let state = Engine::new(
                make_config(&tmp, wrc_workflow(3)),
                ws,
                runner,
                FakeClock::new(),
            )
            .run();
            assert_eq!(state.status, RunStatus::Error);
            assert!(state.reason.unwrap().contains("modelo invalido"));
        }
    }

    #[test]
    fn first_cycle_omits_missing_reads() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();

        runner.push(|req| {
            let in_dir = req.workspace.join(EXCHANGE_DIR).join(IN_DIR);
            assert!(in_dir.join("input.md").is_file());
            assert!(!in_dir.join("review.md").exists());
            assert!(!in_dir.join("critic.md").exists());
            assert!(req.prompt.contains("input.md"));
            assert!(!req.prompt.contains("review.md"));
            assert!(!req.prompt.contains("critic.md"));
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "review.md", "ok\n");
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "critic.md", "ok\nVERDICT: PASS\n");
            StageResult::default()
        });

        let state = Engine::new(
            make_config(&tmp, wrc_workflow(3)),
            ws,
            runner,
            FakeClock::new(),
        )
        .run();
        assert_eq!(state.status, RunStatus::Pass);
    }

    #[test]
    fn second_cycle_worker_receives_latest_review() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();

        // Cycle 1
        runner.push(|_| StageResult::default());
        runner.push(|req| {
            write_out(req.workspace, "review.md", "Review ciclo 1");
            StageResult::default()
        });
        runner.push(|req| {
            write_out(
                req.workspace,
                "critic.md",
                "Critic ciclo 1\nVERDICT: RETRY\n",
            );
            StageResult::default()
        });

        // Cycle 2 worker checks `.tchelvis-loop/in/review.md` and `critic.md`
        runner.push(|req| {
            let in_dir = req.workspace.join(EXCHANGE_DIR).join(IN_DIR);
            assert_eq!(
                fs::read_to_string(in_dir.join("review.md")).unwrap(),
                "Review ciclo 1"
            );
            assert_eq!(
                fs::read_to_string(in_dir.join("critic.md")).unwrap(),
                "Critic ciclo 1\nVERDICT: RETRY\n"
            );
            assert!(req.prompt.contains("review.md"));
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "review.md", "Review ciclo 2");
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "critic.md", "Aprovado\nVERDICT: PASS\n");
            StageResult::default()
        });

        let state = Engine::new(
            make_config(&tmp, wrc_workflow(3)),
            ws,
            runner,
            FakeClock::new(),
        )
        .run();
        assert_eq!(state.status, RunStatus::Pass);
    }

    #[test]
    fn report_failure_does_not_change_status() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();
        let cfg = make_config(&tmp, wrc_workflow(3));
        // Create `report.md` as a directory so writing the report file fails!
        fs::create_dir_all(cfg.run_dir.join("report.md")).unwrap();

        runner.push(|_| StageResult::default());
        runner.push(|req| {
            write_out(req.workspace, "review.md", "ok\n");
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "critic.md", "Aprovado\nVERDICT: PASS\n");
            StageResult::default()
        });

        let state = Engine::new(cfg, ws, runner, FakeClock::new()).run();
        assert_eq!(state.status, RunStatus::Pass);
    }

    #[test]
    fn state_that_stops_being_writable_during_a_stage_does_not_hang() {
        let tmp = TempDir::new().unwrap();
        let clock = Arc::new(FakeClock::new());
        let cfg = make_config(&tmp, wrc_workflow(3));
        let root = tmp.path().join("ws");
        let (run_dir, stage_clock) = (cfg.run_dir.clone(), Arc::clone(&clock));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut runner = ScriptedRunner::new();
            runner.push(move |req| {
                fs::remove_dir_all(&run_dir).unwrap();
                stage_clock.advance(HEARTBEAT_INTERVAL_MS + 1);
                (req.pulse)(true);
                StageResult::default()
            });
            let state = Engine::new(cfg, FakeWorkspace::new(root), runner, clock).run();
            let _ = tx.send(state);
        });
        let state = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("engine must return");
        assert_eq!(state.status, RunStatus::Error);
        assert_eq!(state.executions.len(), 1);
    }

    #[test]
    fn an_interrupted_reader_leaves_no_edit_and_a_failed_worker_keeps_its_work() {
        let outcomes = [
            StageOutcome::TimedOut,
            StageOutcome::Stalled,
            StageOutcome::Cancelled,
            StageOutcome::NonZeroExit(3),
            StageOutcome::Error,
        ];
        for outcome in outcomes {
            // The worker itself is interrupted.
            let tmp = TempDir::new().unwrap();
            let mut runner = ScriptedRunner::new();
            runner.push(move |req| {
                fs::write(req.workspace.join("a.rs"), "partial\n").unwrap();
                StageResult {
                    outcome,
                    ..Default::default()
                }
            });
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let cfg = make_config(&tmp, wrc_workflow(3));
            let state = Engine::new(cfg, ws, runner, FakeClock::new()).run();
            assert_eq!(state.files, ["a.rs"], "{outcome:?}");
            assert!(state.branch.is_some(), "{outcome:?}");

            // The reviewer is interrupted after editing.
            let tmp = TempDir::new().unwrap();
            let mut runner = ScriptedRunner::new();
            runner.push(|req| {
                fs::write(req.workspace.join("a.rs"), "clean\n").unwrap();
                StageResult::default()
            });
            runner.push(move |req| {
                fs::write(req.workspace.join("a.rs"), "tampered\n").unwrap();
                StageResult {
                    outcome,
                    ..Default::default()
                }
            });
            let root = tmp.path().join("ws");
            let ws = FakeWorkspace::new(root.clone());
            let cfg = make_config(&tmp, wrc_workflow(3));
            Engine::new(cfg, ws, runner, FakeClock::new()).run();
            let text = fs::read_to_string(root.join("a.rs")).unwrap();
            assert_eq!(text, "clean\n", "{outcome:?}");
        }
    }

    #[test]
    fn unwritable_run_dir_ends_as_error_without_hanging() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let runner = ScriptedRunner::new();
        let mut cfg = make_config(&tmp, wrc_workflow(3));
        // Point run_dir to an existing regular file so creating/writing inside it fails immediately
        let file_blocking_dir = tmp.path().join("not_a_dir");
        fs::write(&file_blocking_dir, "blocker").unwrap();
        cfg.run_dir = file_blocking_dir;

        let state = Engine::new(cfg, ws, runner, FakeClock::new()).run();
        assert_eq!(state.status, RunStatus::Error);
        assert!(state.reason.is_some());

        // Also test run_dir removed mid-run
        let tmp2 = TempDir::new().unwrap();
        let ws2 = FakeWorkspace::new(tmp2.path().join("ws"));
        let mut runner2 = ScriptedRunner::new();
        let cfg2 = make_config(&tmp2, wrc_workflow(3));
        let run_dir2 = cfg2.run_dir.clone();
        runner2.push(move |_| {
            fs::remove_dir_all(&run_dir2).unwrap();
            StageResult::default()
        });
        let state2 = Engine::new(cfg2, ws2, runner2, FakeClock::new()).run();
        assert_eq!(state2.status, RunStatus::Error);
        assert!(state2.reason.is_some());
    }

    #[test]
    fn heartbeat_and_activity_are_updated() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let clock = Arc::new(FakeClock::new());
        let clock_ref = Arc::clone(&clock);
        let cfg = make_config(&tmp, wrc_workflow(3));
        let run_dir = cfg.run_dir.clone();

        let mut runner = ScriptedRunner::new();
        runner.push(move |req| {
            let initial_on_disk = load_state(&run_dir).unwrap();
            clock_ref.advance(11_000);
            (req.pulse)(true);
            let updated_on_disk = load_state(&run_dir).unwrap();
            assert!(updated_on_disk.heartbeat_at_ms > initial_on_disk.heartbeat_at_ms);
            assert!(updated_on_disk.last_activity_at_ms > initial_on_disk.last_activity_at_ms);
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "review.md", "ok\n");
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "critic.md", "ok\nVERDICT: PASS\n");
            StageResult::default()
        });

        let state = Engine::new(cfg, ws, runner, clock).run();
        assert_eq!(state.status, RunStatus::Pass);
    }

    #[test]
    fn workspace_probe_is_throttled_and_never_follows_symlinks() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&ws).unwrap();
        fs::write(outside.join("secret.txt"), "v1").unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("escape_link")).unwrap();

        let clock = FakeClock::new();
        let mut sampler = WorkspaceSampler::new(&ws, &clock);

        // Modify file in workspace, but clock has not advanced 10s -> throttled (returns false)
        fs::write(ws.join("work.txt"), "step1").unwrap();
        clock.advance(5_000);
        assert!(!sampler.sample_throttled());

        // Advance past 10s -> detects work.txt
        clock.advance(5_000);
        assert!(sampler.sample_throttled());

        // Modify file behind symlinked directory -> must NOT be followed or trigger activity
        fs::write(outside.join("secret.txt"), "v2-much-longer-content").unwrap();
        clock.advance(15_000);
        assert!(!sampler.sample_throttled());
    }

    struct PassthroughSandbox {
        bin_dir: PathBuf,
    }

    impl crate::loops::sandbox::Sandbox for PassthroughSandbox {
        fn kind(&self) -> &'static str {
            "test"
        }

        fn check(&self, _workspace: &Path) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        fn wrap(
            &self,
            argv: &[std::ffi::OsString],
            workspace: &Path,
        ) -> Result<crate::loops::sandbox::SandboxedCommand> {
            let mut new_argv = argv.to_vec();
            new_argv[0] = self.bin_dir.join(&argv[0]).into_os_string();
            Ok(crate::loops::sandbox::SandboxedCommand {
                argv: new_argv,
                env: vec![
                    (
                        std::ffi::OsString::from("PATH"),
                        std::env::var_os("PATH")
                            .unwrap_or_else(|| std::ffi::OsString::from("/usr/bin:/bin")),
                    ),
                    (
                        std::ffi::OsString::from("HOME"),
                        std::env::var_os("HOME")
                            .unwrap_or_else(|| std::ffi::OsString::from("/tmp")),
                    ),
                ],
                cwd: workspace.to_path_buf(),
            })
        }
    }

    #[test]
    fn process_stage_runner_cleans_up_child_on_error() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let child_pid_file = tmp.path().join("bg.pid");
        crate::test_support::write_executable(
            bin_dir.join("claude"),
            format!(
                "#!/usr/bin/env sh\n\
                 setsid sleep 60 </dev/null >/dev/null 2>&1 &\n\
                 echo $! > \"{}\"\n\
                 printf '%s\\n' '{{\"type\":\"result\",\"is_error\":true,\"num_turns\":1,\"result\":\"falha simulada\"}}'\n\
                 exit 1\n",
                child_pid_file.display()
            ),
        );

        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let sb = PassthroughSandbox { bin_dir };
        let clock = SystemClock::new();
        let runner = ProcessStageRunner::new(&sb, &clock);
        let cfg = make_config(&tmp, wrc_workflow(3));
        let state = Engine::new(cfg, ws, runner, SystemClock::new()).run();
        assert_eq!(state.status, RunStatus::Error);
        assert!(state
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("falha simulada"));

        if let Ok(raw) = fs::read_to_string(&child_pid_file) {
            if let Ok(pid) = raw.trim().parse::<i32>() {
                for _ in 0..40 {
                    let stat_path = format!("/proc/{pid}/stat");
                    let alive = match fs::read_to_string(&stat_path) {
                        Ok(s) => {
                            s.rfind(')')
                                .and_then(|i| s[i + 1..].trim_start().chars().next())
                                != Some('Z')
                        }
                        Err(_) => unsafe { libc::kill(pid, 0) == 0 },
                    };
                    if !alive {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                panic!("background process {pid} survived engine exit");
            }
        }
    }

    #[test]
    fn engine_persists_model_sources_and_preflight_in_state_and_report() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();
        runner.push(|_req| StageResult::default());
        runner.push(|req| {
            write_out(req.workspace, "review.md", "ok\n");
            StageResult::default()
        });
        runner.push(|req| {
            write_out(req.workspace, "critic.md", "aprovado\nVERDICT: PASS\n");
            StageResult::default()
        });

        let mut sources = BTreeMap::new();
        sources.insert("implement".to_string(), ModelSource::RunFlag);
        sources.insert("audit".to_string(), ModelSource::ProjectStage);

        let probes = vec![ProbeRecord {
            provider: crate::loops::workflow::Provider::Claude,
            model: Some("haiku".to_string()),
            method: crate::loops::preflight::ProbeMethod::Probe,
            usage: Usage {
                turns: Some(1),
                input_tokens: Some(400),
                output_tokens: Some(20),
                cache_read_tokens: Some(0),
                cost_usd: Some(0.002),
            },
            duration_ms: 1200,
        }];

        let cfg = make_config(&tmp, wrc_workflow(3));
        let state = Engine::new(cfg, ws, runner, FakeClock::new())
            .with_model_sources(sources)
            .with_preflight(probes.clone())
            .run();

        assert_eq!(state.status, RunStatus::Pass);
        assert_eq!(state.preflight, probes);
        assert_eq!(state.executions[0].model_source, Some(ModelSource::RunFlag));
        assert_eq!(state.executions[1].model_source, None);
        assert_eq!(
            state.executions[2].model_source,
            Some(ModelSource::ProjectStage)
        );
        let report = fs::read_to_string(tmp.path().join("run/report.md")).unwrap();
        assert!(report.contains("Pré-teste de modelos"));
        assert!(report.contains("--use"));
        assert!(report.contains("projeto (.loops/models.toml etapa)"));
    }

    #[test]
    fn pi_bad_model_with_exit_zero_fails_stage() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let bad = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/loop/pi_bad_model.jsonl"
        );
        crate::test_support::write_executable(
            bin_dir.join("pi"),
            format!("#!/usr/bin/env sh\ncat \"{bad}\"\nexit 0\n"),
        );

        let mut wf = wrc_workflow(1);
        wf.stages[0].provider = Provider::Pi;
        wf.stages[0].model = Some("opencode-go/glm-5.3".into());

        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let sb = PassthroughSandbox { bin_dir };
        let clock = SystemClock::new();
        let runner = ProcessStageRunner::new(&sb, &clock);
        let cfg = make_config(&tmp, wf);
        let state = Engine::new(cfg, ws, runner, SystemClock::new()).run();

        assert_eq!(state.status, RunStatus::Error);
        let reason = state.reason.unwrap_or_default();
        assert!(reason.contains("isn't included"), "got: {reason}");
    }

    #[test]
    fn engine_passes_opencode_api_key_only_to_opencode_and_redacts_from_state_errors_and_report() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let secret = "sk-engine-opencode-topsecret-888";
        let opencode_saw = tmp.path().join("opencode_env_key");
        let claude_saw = tmp.path().join("claude_env_key");

        crate::test_support::write_executable(
            bin_dir.join("opencode"),
            format!(
                "#!/usr/bin/env sh\n\
                 printf '%s' \"${{OPENCODE_API_KEY:-ABSENT}}\" > \"{}\"\n\
                 printf '%s\\n' '{{\"type\":\"step_finish\",\"part\":{{\"tokens\":{{\"input\":5,\"output\":5,\"cache\":{{\"read\":0}}}},\"cost\":0.0}}}}'\n\
                 printf 'done\\n' > \"$PWD/.tchelvis-loop/out/review.md\" 2>/dev/null || true\n",
                opencode_saw.display()
            ),
        );
        crate::test_support::write_executable(
            bin_dir.join("claude"),
            format!(
                "#!/usr/bin/env sh\n\
                 printf '%s' \"${{OPENCODE_API_KEY:-ABSENT}}\" > \"{}\"\n\
                 printf '%s\\n' '{{\"type\":\"result\",\"is_error\":true,\"num_turns\":1,\"result\":\"bad key {secret}\"}}'\n\
                 exit 1\n",
                claude_saw.display()
            ),
        );

        let mut wf = wrc_workflow(2);
        wf.stages[0].provider = crate::loops::workflow::Provider::Opencode;
        wf.stages[0].model = Some("zai/glm".into());
        wf.stages[1].provider = crate::loops::workflow::Provider::Claude;

        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let sb = PassthroughSandbox { bin_dir };
        let clock = SystemClock::new();
        let runner =
            ProcessStageRunner::new(&sb, &clock).with_opencode_api_key(Some(secret.into()));
        let cfg = make_config(&tmp, wf);
        let state = Engine::new(cfg, ws, runner, SystemClock::new())
            .with_extra_secrets(vec![secret.to_string()])
            .run();

        assert_eq!(state.status, RunStatus::Error);
        assert_eq!(fs::read_to_string(&opencode_saw).unwrap(), secret);
        assert_eq!(fs::read_to_string(&claude_saw).unwrap(), "ABSENT");

        let raw_state = fs::read_to_string(tmp.path().join("run/state.json")).unwrap();
        assert!(!raw_state.contains(secret), "secret leaked into state.json");
        assert!(raw_state.contains("[REDACTED]"));

        let raw_report = fs::read_to_string(tmp.path().join("run/report.md")).unwrap();
        assert!(!raw_report.contains(secret), "secret leaked into report.md");
        assert!(raw_report.contains("[REDACTED]"));
    }

    #[test]
    fn the_result_is_on_disk_before_anyone_is_told() {
        let tmp = TempDir::new().unwrap();
        let cfg = make_config(&tmp, wrc_workflow(1));
        cfg.cancel.store(true, Ordering::Relaxed);
        let dir = cfg.run_dir.clone();
        let seen = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        Engine::new(cfg, ws, ScriptedRunner::new(), FakeClock::new())
            .with_on_final(move |_| {
                *sink.lock().unwrap() = load_state(&dir).ok().map(|s| s.status);
            })
            .run();
        assert_eq!(*seen.lock().unwrap(), Some(RunStatus::Cancelled));
    }

    type EvidenceEvent = (String, Provider, Option<String>);

    #[derive(Clone, Default)]
    struct RecordingEvidence(Arc<Mutex<Vec<EvidenceEvent>>>);

    impl EvidenceRecorder for RecordingEvidence {
        fn record(&mut self, provider: Provider, model: Option<&str>, _now_ms: i64) {
            self.0
                .lock()
                .unwrap()
                .push(("record".into(), provider, model.map(str::to_string)));
        }

        fn discard(&mut self, provider: Provider, model: Option<&str>) {
            self.0
                .lock()
                .unwrap()
                .push(("discard".into(), provider, model.map(str::to_string)));
        }
    }

    #[test]
    fn engine_records_ok_discards_only_failing_pair_and_keeps_on_cancel_missing_and_malformed() {
        let mut wf = wrc_workflow(2);
        wf.stages[0].provider = Provider::Claude;
        wf.stages[0].model = Some("sonnet".into());
        wf.stages[1].provider = Provider::Agy;
        wf.stages[1].model = None;
        wf.stages[2].provider = Provider::Claude;
        wf.stages[2].model = Some("haiku".into());

        // 1. Failing stage discards ONLY its own (provider, model) pair; earlier OK stage records its own.
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let mut runner = ScriptedRunner::new();
        runner.push(|req| {
            fs::write(req.workspace.join("a.txt"), "ok").unwrap();
            StageResult::default()
        });
        runner.push(|_req| StageResult {
            outcome: StageOutcome::NonZeroExit(1),
            usage: Usage::default(),
            error: Some("quota exhausted".into()),
            stop: None,
            tool_silence: None,
        });
        let events = RecordingEvidence::default();
        let state = Engine::new(make_config(&tmp, wf.clone()), ws, runner, FakeClock::new())
            .with_evidence_recorder(events.clone())
            .run();
        assert_eq!(state.status, RunStatus::Error);
        assert_eq!(
            *events.0.lock().unwrap(),
            vec![
                ("record".into(), Provider::Claude, Some("sonnet".into())),
                ("discard".into(), Provider::Agy, None),
            ]
        );

        // 2. Cancelled stage touches neither record nor discard.
        let tmp2 = TempDir::new().unwrap();
        let ws2 = FakeWorkspace::new(tmp2.path().join("ws"));
        let mut runner2 = ScriptedRunner::new();
        runner2.push(|_req| StageResult {
            outcome: StageOutcome::Cancelled,
            usage: Usage::default(),
            error: None,
            stop: None,
            tool_silence: None,
        });
        let events2 = RecordingEvidence::default();
        let state2 = Engine::new(
            make_config(&tmp2, wf.clone()),
            ws2,
            runner2,
            FakeClock::new(),
        )
        .with_evidence_recorder(events2.clone())
        .run();
        assert_eq!(state2.status, RunStatus::Cancelled);
        assert!(events2.0.lock().unwrap().is_empty());

        // 3. Missing artifact: model exited 0 without error -> keeps/records evidence, never discards.
        let tmp3 = TempDir::new().unwrap();
        let ws3 = FakeWorkspace::new(tmp3.path().join("ws"));
        let mut runner3 = ScriptedRunner::new();
        runner3.push(|_| StageResult::default());
        runner3.push(|_| StageResult::default()); // does NOT write review.md!
        let events3 = RecordingEvidence::default();
        let state3 = Engine::new(
            make_config(&tmp3, wf.clone()),
            ws3,
            runner3,
            FakeClock::new(),
        )
        .with_evidence_recorder(events3.clone())
        .run();
        assert_eq!(state3.status, RunStatus::Error);
        assert_eq!(
            *events3.0.lock().unwrap(),
            vec![
                ("record".into(), Provider::Claude, Some("sonnet".into())),
                ("record".into(), Provider::Agy, None),
            ]
        );

        // 4. Malformed verdict: model responded cleanly -> keeps/records evidence, never discards.
        let tmp4 = TempDir::new().unwrap();
        let ws4 = FakeWorkspace::new(tmp4.path().join("ws"));
        let mut runner4 = ScriptedRunner::new();
        runner4.push(|_| StageResult::default());
        runner4.push(|req| {
            write_out(req.workspace, "review.md", "ok\n");
            StageResult::default()
        });
        runner4.push(|req| {
            write_out(req.workspace, "critic.md", "bad verdict without marker\n");
            StageResult::default()
        });
        runner4.push(|req| {
            write_out(req.workspace, "critic.md", "still malformed\n");
            StageResult::default()
        });
        let events4 = RecordingEvidence::default();
        let state4 = Engine::new(make_config(&tmp4, wf), ws4, runner4, FakeClock::new())
            .with_evidence_recorder(events4.clone())
            .run();
        assert_eq!(state4.status, RunStatus::Error);
        assert!(events4
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|(kind, _, _)| kind == "record"));
    }

    /// Runs one stage with a fake CLI. Once `lines` lines were read, each probe advances the clock by
    /// 100 s, `steps` times; on the last one the file `@GO@` appears. `stall` 600 s, `tool_stall` 1200 s.
    fn run_silent_stage(provider: Provider, lines: u32, steps: u32, body: &str) -> StageResult {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let go = tmp.path().join("go");
        let wait = "i=0; while [ ! -f \"@GO@\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.02; done";
        let script = format!("#!/usr/bin/env sh\n{}\n", body.replace("@WAIT@", wait))
            .replace("@GO@", &go.display().to_string());
        crate::test_support::write_executable(bin_dir.join(provider.as_str()), script);

        let mut stage = wrc_workflow(1).stages[0].clone();
        stage.provider = provider;
        stage.model = (provider == Provider::Pi).then(|| "opencode-go/glm-5.3".to_string());

        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let sb = PassthroughSandbox { bin_dir };
        let clock = FakeClock::new();
        let (seen, stepped) = (std::cell::Cell::new(0u32), std::cell::Cell::new(0u32));
        let pulse = |active: bool| {
            if active {
                seen.set(seen.get() + 1);
            } else if seen.get() >= lines && stepped.get() < steps {
                clock.advance(100_000);
                stepped.set(stepped.get() + 1);
                if stepped.get() == steps {
                    fs::write(&go, "").unwrap();
                }
            }
        };
        let cancel = AtomicBool::new(false);
        let log_path = tmp.path().join("stage.log");
        ProcessStageRunner::new(&sb, &clock).run(StageRequest {
            run_id: "20261125-000000-tst0",
            cycle: 1,
            exec_seq: 1,
            stage: &stage,
            workspace: ws.path(),
            prompt: "faça algo",
            log_path: &log_path,
            limits: Limits {
                timeout: Duration::from_secs(100_000),
                stall: Duration::from_secs(600),
            },
            tool_stall: Duration::from_secs(1200),
            cancel: &cancel,
            pulse: &pulse,
        })
    }

    const PI_START: &str = r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash"}"#;
    const PI_END: &str = r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash"}"#;
    const PI_RESULT: &str = r#"{"type":"agent_settled"}"#;
    const CLAUDE_START: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#;
    const CLAUDE_END: &str =
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
    const CLAUDE_RESULT: &str = r#"{"type":"result","is_error":false,"num_turns":1}"#;
    const AGY_START: &str = r#"{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"tool","tool_name":"shell"}}"#;
    const AGY_END: &str = r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"tool","tool_name":"shell"}}"#;
    const AGY_RESULT: &str = r#"{"event":"result","result":{"status":"SUCCESS"}}"#;

    fn printed(lines: &[&str]) -> String {
        let mut out = String::from("printf '%s\\n'");
        for line in lines {
            out.push(' ');
            out.push_str(&format!("'{}'", line.replace('\'', "'\\''")));
        }
        out
    }

    #[test]
    fn silent_open_tool_survives_stall_and_is_recorded() {
        for (provider, start, end, result) in [
            (Provider::Pi, PI_START, PI_END, PI_RESULT),
            (Provider::Claude, CLAUDE_START, CLAUDE_END, CLAUDE_RESULT),
            (Provider::Agy, AGY_START, AGY_END, AGY_RESULT),
        ] {
            let body = format!(
                "{}\n@WAIT@\n{}\n{}",
                printed(&[start]),
                printed(&[end, result]),
                ""
            );
            let out = run_silent_stage(provider, 1, 8, &body);
            assert_eq!(out.outcome, StageOutcome::Ok, "{provider:?}");
            assert_eq!(out.error, None, "{provider:?}");
            assert_eq!(
                out.tool_silence,
                Some(ToolSilence {
                    longest_ms: 700_000,
                    source: ToolSource::Events,
                }),
                "{provider:?}"
            );
            assert_eq!(out.stop, None, "{provider:?}");
        }
    }

    #[test]
    fn closed_tool_with_a_background_shell_stalls_at_stall() {
        for (provider, start, end) in [
            (Provider::Pi, PI_START, PI_END),
            (Provider::Claude, CLAUDE_START, CLAUDE_END),
            (Provider::Agy, AGY_START, AGY_END),
        ] {
            let body = format!(
                "sleep 60 >/dev/null 2>&1 &\n{}\n@WAIT@",
                printed(&[start, end])
            );
            let out = run_silent_stage(provider, 2, 30, &body);
            assert_eq!(out.outcome, StageOutcome::Stalled, "{provider:?}");
            let stop = out.stop.expect("a stalled stage records a stop");
            assert_eq!(stop.limit, Some(StopLimit::Stall));
            assert_eq!(stop.limit_secs, None);
            assert_eq!(stop.tool_signal, None);
            assert_eq!(out.tool_silence, None);
            assert_eq!(stop.silent_ms, 700_000, "{provider:?}");
        }
    }

    #[test]
    fn silent_opencode_stage_stalls_as_before() {
        let body = "printf '%s\\n' '{\"type\":\"step_start\"}'\nsleep 60 >/dev/null 2>&1 &\n@WAIT@"
            .to_string();
        let out = run_silent_stage(Provider::Opencode, 1, 30, &body);
        assert_eq!(out.outcome, StageOutcome::Stalled);
        let stop = out.stop.expect("a stalled stage records a stop");
        assert_eq!(stop.limit, Some(StopLimit::Stall));
        assert_eq!(stop.limit_secs, None);
        assert_eq!(stop.tool_signal, None);
        assert_eq!(out.tool_silence, None);
        assert_eq!(stop.silent_ms, 700_000);
    }

    #[test]
    fn open_tool_that_never_ends_stops_at_the_ceiling() {
        let body = format!("{}\n@WAIT@", printed(&[PI_START]));
        let out = run_silent_stage(Provider::Pi, 1, 30, &body);
        assert_eq!(out.outcome, StageOutcome::Stalled);
        let stop = out.stop.expect("a stalled stage records a stop");
        assert_eq!(stop.limit, Some(StopLimit::ToolStall));
        assert_eq!(stop.limit_secs, Some(1200));
        assert_eq!(
            stop.tool_signal,
            Some(ToolSignal {
                source: ToolSource::Events,
                names: vec!["bash".to_string()],
            })
        );
        assert_eq!(
            out.tool_silence,
            Some(ToolSilence {
                longest_ms: 1_200_000,
                source: ToolSource::Events,
            })
        );
    }

    #[test]
    fn failed_stage_keeps_its_tool_silence() {
        let body = format!("{}\n@WAIT@\nexit 3", printed(&[PI_START]));
        let out = run_silent_stage(Provider::Pi, 1, 8, &body);
        assert_eq!(out.outcome, StageOutcome::NonZeroExit(3));
        assert_eq!(
            out.tool_silence,
            Some(ToolSilence {
                longest_ms: 700_000,
                source: ToolSource::Events,
            })
        );
    }

    #[test]
    fn stage_request_carries_tool_stall_and_max_duration_still_wins() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let clock = Arc::new(FakeClock::new());
        let clock_for_stage = Arc::clone(&clock);
        let mut runner = ScriptedRunner::new();
        runner.push(move |_| {
            clock_for_stage.advance(250_000);
            StageResult::default()
        });
        let clock_for_stage = Arc::clone(&clock);
        runner.push(move |req| {
            assert_eq!(req.tool_stall, Duration::from_secs(1200));
            assert_eq!(req.limits.stall, Duration::from_secs(30));
            assert_eq!(req.limits.timeout, Duration::from_secs(50));
            clock_for_stage.advance(50_000);
            StageResult {
                outcome: StageOutcome::TimedOut,
                ..Default::default()
            }
        });
        let state = Engine::new(make_config(&tmp, wrc_workflow(3)), ws, runner, clock).run();
        assert_eq!(state.status, RunStatus::Escalate);
        assert!(state
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("MAX_DURATION"));
    }

    #[test]
    fn tool_stall_reason_and_silence_reach_the_state_without_a_secret() {
        let secret = "sk-tool-secret-123456";
        let run = |limit: Option<StopLimit>| {
            let tmp = TempDir::new().unwrap();
            let ws = FakeWorkspace::new(tmp.path().join("ws"));
            let mut runner = ScriptedRunner::new();
            runner.push(move |_| StageResult {
                outcome: StageOutcome::Stalled,
                stop: Some(StopDiagnostic {
                    last_event: Some("tool_execution_start (bash)".into()),
                    last_output_at_ms: Some(0),
                    silent_ms: 1_200_000,
                    cli_alive: true,
                    running_tools: Some(vec!["bash".to_string()]),
                    processes: Vec::new(),
                    processes_total: 0,
                    limit,
                    limit_secs: limit.map(|_| 1200),
                    tool_signal: Some(ToolSignal {
                        source: ToolSource::Events,
                        names: vec![format!("bash {secret}")],
                    }),
                }),
                tool_silence: Some(ToolSilence {
                    longest_ms: 1_200_000,
                    source: ToolSource::Events,
                }),
                ..Default::default()
            });
            let state = Engine::new(
                make_config(&tmp, wrc_workflow(3)),
                ws,
                runner,
                FakeClock::new(),
            )
            .with_extra_secrets(vec![secret.to_string()])
            .run();
            (tmp, state)
        };

        let (tmp, state) = run(Some(StopLimit::ToolStall));
        assert_eq!(state.status, RunStatus::Escalate);
        let reason = state.reason.clone().unwrap_or_default();
        assert!(
            reason.contains(
                "etapa travada sem atividade com ferramenta aberta (STALLED, tool_stall_secs): ferramenta em execução: bash"
            ),
            "got: {reason}"
        );
        let stop = state.executions[0]
            .stop
            .clone()
            .expect("a cut stage records a stop");
        assert_eq!(stop.limit, Some(StopLimit::ToolStall));
        assert_eq!(stop.limit_secs, Some(1200));
        let loaded = load_state(&tmp.path().join("run")).unwrap();
        assert_eq!(
            loaded.executions[0].stop.as_ref().unwrap().limit,
            Some(StopLimit::ToolStall)
        );
        assert_eq!(
            loaded.executions[0].tool_silence,
            Some(ToolSilence {
                longest_ms: 1_200_000,
                source: ToolSource::Events,
            })
        );
        let raw_state = fs::read_to_string(tmp.path().join("run/state.json")).unwrap();
        assert!(!raw_state.contains(secret), "secret leaked into state.json");
        let raw_report = fs::read_to_string(tmp.path().join("run/report.md")).unwrap();
        assert!(!raw_report.contains(secret), "secret leaked into report.md");

        let (_tmp, state) = run(Some(StopLimit::Stall));
        let reason = state.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("etapa travada sem atividade (STALLED): ferramenta em execução: bash"),
            "got: {reason}"
        );
    }

    #[test]
    fn stalled_stage_records_stop_and_hint_without_leaking_a_secret() {
        let tmp = TempDir::new().unwrap();
        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let secret = "sk-stop-secret-123456";
        let mut runner = ScriptedRunner::new();
        runner.push(move |_| StageResult {
            outcome: StageOutcome::Stalled,
            usage: Usage::default(),
            error: None,
            stop: Some(StopDiagnostic {
                last_event: Some("tool_execution_update (bash)".into()),
                last_output_at_ms: Some(1_000),
                silent_ms: 1_500,
                cli_alive: true,
                running_tools: Some(vec![
                    format!("bash {secret}"),
                    format!("{}{secret}", "x".repeat(55)),
                ]),
                processes: vec![StopProcess {
                    name: format!("tool-{secret}"),
                    age_ms: 100,
                    cpu_ms: 5,
                    after_last_output: true,
                }],
                processes_total: 3,
                limit: None,
                limit_secs: None,
                tool_signal: None,
            }),
            tool_silence: None,
        });

        let state = Engine::new(
            make_config(&tmp, wrc_workflow(3)),
            ws,
            runner,
            FakeClock::new(),
        )
        .with_extra_secrets(vec![secret.to_string()])
        .run();

        assert_eq!(state.status, RunStatus::Escalate);
        let reason = state.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("etapa travada sem atividade (STALLED): ferramenta em execução: bash"),
            "got: {reason}"
        );
        assert!(
            !reason.contains(secret),
            "secret leaked into reason: {reason}"
        );

        let stop = state.executions[0]
            .stop
            .clone()
            .expect("a cut stage must record a stop");
        assert_eq!(stop.silent_ms, 1_500);
        assert_eq!(stop.processes_total, 3);
        assert!(stop.cli_alive);
        assert!(
            !format!("{stop:?}").contains("sk-stop"),
            "secret kept in state"
        );

        let raw_state = fs::read_to_string(tmp.path().join("run/state.json")).unwrap();
        assert!(!raw_state.contains(secret), "secret leaked into state.json");
        assert!(raw_state.contains("[REDACTED]"));
        let raw_report = fs::read_to_string(tmp.path().join("run/report.md")).unwrap();
        assert!(!raw_report.contains(secret), "secret leaked into report.md");
        assert!(
            raw_report.contains("## Etapas interrompidas"),
            "{raw_report}"
        );
        assert!(raw_report.contains("[REDACTED]"), "{raw_report}");
    }

    #[test]
    fn sanitize_stop_cleans_the_tool_signal() {
        let secret = "sk-signal-secret-1";
        let mut names: Vec<String> = (0..10).map(|i| format!("tool{i}")).collect();
        names.push(format!("bash {secret}"));
        names.push("\n".to_string());
        let stop = StopDiagnostic {
            last_event: None,
            last_output_at_ms: None,
            silent_ms: 0,
            cli_alive: true,
            running_tools: None,
            processes: Vec::new(),
            processes_total: 0,
            limit: Some(StopLimit::ToolStall),
            limit_secs: Some(1200),
            tool_signal: Some(ToolSignal {
                source: ToolSource::Events,
                names,
            }),
        };

        let out = sanitize_stop(stop, &[secret]);
        let signal = out.tool_signal.expect("the signal survives sanitizing");
        assert!(signal.names.len() <= MAX_STOP_TOOLS, "{signal:?}");
        assert!(
            signal
                .names
                .iter()
                .all(|n| !n.contains(secret) && !n.is_empty()),
            "{signal:?}"
        );
        assert_eq!(out.limit, Some(StopLimit::ToolStall));
        assert_eq!(out.limit_secs, Some(1200));
        assert_eq!(signal.source, ToolSource::Events);
    }

    #[test]
    fn interrupted_pi_stage_hides_the_missing_settled_error() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        crate::test_support::write_executable(
            bin_dir.join("pi"),
            "#!/usr/bin/env sh\n\
             printf '%s\\n' '{\"type\":\"tool_execution_start\",\"toolCallId\":\"c1\",\"toolName\":\"bash\"}'\n\
             printf '%s\\n' '{\"type\":\"tool_execution_update\",\"toolCallId\":\"c1\",\"toolName\":\"bash\"}'\n\
             sleep 30\n",
        );

        let mut wf = wrc_workflow(1);
        wf.stages[0].provider = Provider::Pi;
        wf.stages[0].model = Some("opencode-go/glm-5.3".into());
        let stage = wf.stages[0].clone();

        let ws = FakeWorkspace::new(tmp.path().join("ws"));
        let sb = PassthroughSandbox { bin_dir };
        let clock = SystemClock::new();
        let mut runner = ProcessStageRunner::new(&sb, &clock);
        let log_path = tmp.path().join("implement.log");
        let cancel = AtomicBool::new(false);
        let pulse = |_active: bool| {};
        let result = runner.run(StageRequest {
            run_id: "20261125-000000-tst0",
            cycle: 1,
            exec_seq: 1,
            stage: &stage,
            workspace: ws.path(),
            prompt: "faça algo",
            log_path: &log_path,
            limits: Limits {
                timeout: Duration::from_secs(30),
                stall: Duration::from_millis(800),
            },
            tool_stall: Duration::from_millis(800),
            cancel: &cancel,
            pulse: &pulse,
        });

        assert_eq!(result.outcome, StageOutcome::Stalled);
        assert_eq!(
            result.error, None,
            "an interrupted stage must not report the missing settled event"
        );
        let stop = result.stop.expect("a cut stage must record a stop");
        assert_eq!(stop.running_tools, Some(vec!["bash".to_string()]));
        assert_eq!(
            stop.last_event.as_deref(),
            Some("tool_execution_update (bash)")
        );
        assert!(stop.cli_alive, "the fake pi is still sleeping");
        assert!(
            stop.silent_ms >= 800,
            "silent_ms {} must cover the stall threshold",
            stop.silent_ms
        );
        assert_eq!(stop.hint(), "ferramenta em execução: bash");
    }
}
