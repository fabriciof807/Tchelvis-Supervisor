use crate::config::{CockpitConfig, CockpitPaths};
use crate::correlate::resolve_git_workspace;
use crate::daemon::query_socket_once;
use crate::herdr::{HcRunError, HcTerminal, HerdrClient};
use crate::model::{IpcOp, IpcRequest};
use crate::orch::preamble::{build_dispatch_preamble, orchestration_skill_markdown};
use crate::orch::store::OrchStore;
use crate::orch::{
    generate_orch_id, DispatchContext, DispatchStatus, InboxCheckResult, MessageType, OrchMessage,
    OrchRun, OrchTask, PointerState, QuestionThread, RunStatus, TaskStatus, MAX_DISPATCH_FAILURES,
    MAX_ORCH_DEPTH,
};
use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;
use tokio::process::Command;

pub const DISPATCH_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DISPATCH_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
pub const DISPATCH_LIVENESS_RECHECK_TIMEOUT: Duration = Duration::from_secs(2);
pub const DISPATCH_LIVENESS_RECHECK_INTERVAL: Duration = Duration::from_millis(250);
pub const WORKER_LAUNCH_TAIL_LINES: usize = 20;

pub trait OrchSleeper: Send + Sync {
    fn now_ms(&self) -> i64;
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemOrchSleeper;

impl OrchSleeper for SystemOrchSleeper {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchOutput {
    #[serde(flatten)]
    pub dispatch: DispatchContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_cleanup: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OrchLaunchArgs {
    pub agent: String,
    pub prompt: String,
    pub title: Option<String>,
    pub run: Option<String>,
    pub worktree: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOutput {
    pub run: OrchRun,
    pub task: OrchTask,
    pub dispatch: DispatchContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchLaunchError {
    pub failed_stage: String,
    pub agent: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
}

impl fmt::Display for OrchLaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "orch launch failed at stage '{}' for agent '{}': {}",
            self.failed_stage, self.agent, self.detail
        )?;
        if let Some(code) = self.exit_code {
            write!(f, " (exit_code={code})")?;
        }
        if let Some(ref tail) = self.output_tail {
            if !tail.trim().is_empty() {
                write!(f, "\n--- last lines ---\n{}", tail.trim())?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for OrchLaunchError {}

#[derive(Debug, Clone)]
pub struct OrchDispatchArgs {
    pub run: String,
    pub task: String,
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub terminal: Option<String>,
    pub agent: Option<String>,
    pub worktree: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OrchSendArgs {
    pub msg_type: String,
    pub outcome: Option<String>,
    pub dispatch_id: String,
    pub body: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OrchSendToArgs {
    pub to: String,
    pub body: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendToOutput {
    pub message: OrchMessage,
    pub dispatch_id: String,
    pub dispatch_status: DispatchStatus,
    pub delivered_immediately: bool,
}

#[derive(Debug, Clone)]
pub struct OrchRetryArgs {
    pub dispatch_id: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryOutput {
    pub previous_dispatch_id: String,
    pub previous_status: DispatchStatus,
    #[serde(flatten)]
    pub dispatch_output: DispatchOutput,
}

#[derive(Debug, Clone)]
pub struct OrchStopArgs {
    pub dispatch_id: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopOutput {
    pub dispatch_id: String,
    pub run_id: String,
    pub task_id: String,
    pub assignee_terminal: String,
    pub dispatch_status: DispatchStatus,
    pub task_status: TaskStatus,
    pub terminal_closed: bool,
    pub worktree_kept: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OrchAskArgs {
    pub dispatch_id: String,
    pub question: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum OrchCommands {
    /// Preflight an agent and create a run (optional), task, and dispatch in one command
    Launch {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        run: Option<String>,
        #[arg(long, default_value = "current")]
        worktree: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        caller: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create a new orchestration run and bind the coordinator terminal
    RunCreate {
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        coordinator: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create or update a DAG task within an orchestration run
    TaskCreate {
        #[arg(long)]
        run: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        deps: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Dispatch a task to a spawned or existing worker terminal
    Dispatch {
        #[arg(long)]
        run: String,
        #[arg(long)]
        task: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        terminal: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value = "current")]
        worktree: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        caller: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a worker report (--type) or coordinator directive (--to dispatch:<id>)
    Send {
        #[arg(long)]
        to: Option<String>,
        #[arg(long = "type")]
        msg_type: Option<String>,
        #[arg(long)]
        outcome: Option<String>,
        #[arg(long)]
        dispatch_id: Option<String>,
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        body: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        caller: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Retry a dispatch by launching a new attempt for the same task and superseding the old one
    Retry {
        dispatch_id: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        caller: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Stop a worker dispatch, close its terminal, and restore its task to a retryable state
    Stop {
        dispatch_id: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        caller: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Ask a blocking question to the parent coordinator and wait for reply wakeup
    Ask {
        #[arg(long)]
        dispatch_id: String,
        #[arg(long)]
        question: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        caller: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reply to an open worker question thread and wake the worker terminal
    Reply {
        #[arg(long)]
        question_id: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Check and acknowledge pending orchestration messages for the caller terminal
    Check {
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        terminal: Option<String>,
        #[arg(long)]
        peek: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect orchestration run DAG tasks, dispatches, and mailbox counts
    Status {
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Opt-in release of a worker terminal and its linked git worktree
    Release {
        #[arg(long)]
        dispatch_id: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print the self-contained Markdown orchestration guide for coordinator and worker agents
    Skill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStatusView {
    pub run: OrchRun,
    pub tasks: Vec<OrchTask>,
    pub dispatches: Vec<DispatchContext>,
    #[serde(default)]
    pub messages: Vec<OrchMessage>,
    pub open_questions: Vec<QuestionThread>,
    pub unacknowledged_messages: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchStatusOutput {
    pub runs: Vec<RunStatusView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseOutput {
    pub dispatch_id: String,
    pub assignee_terminal: String,
    pub status: DispatchStatus,
    pub terminal_released: bool,
    pub worktree_removed: bool,
    pub worktree_path: Option<String>,
}

pub fn map_agent_preset(agent: &str) -> String {
    let trimmed = agent.trim();
    match trimmed {
        "codex" => "codex --no-daemon".to_string(),
        "claude" | "claude-code" => "claude".to_string(),
        "opencode" => "opencode".to_string(),
        "pi" => "pi".to_string(),
        "gemini" => "gemini".to_string(),
        // agy rejects a positional prompt; `-i` starts an interactive session with it.
        "agy" | "antigravity" => "agy -i".to_string(),
        other => other.to_string(),
    }
}

pub fn parse_deps_arg(deps: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = deps.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    if raw.starts_with('[') {
        let parsed: Vec<String> =
            serde_json::from_str(raw).context("invalid JSON array in --deps")?;
        return Ok(parsed
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect());
    }
    Ok(raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

pub(crate) async fn fetch_hc_terminal(herdr: &HerdrClient, terminal: &str) -> Result<HcTerminal> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(herdr.hc_bin())
            .args(["get", terminal, "--json"])
            .stdin(Stdio::null())
            .output(),
    )
    .await;

    if let Ok(Ok(get_out)) = out {
        if get_out.status.success() {
            if let Ok(mut term) = serde_json::from_slice::<HcTerminal>(&get_out.stdout) {
                if term.address.is_empty() {
                    term.address = terminal.to_string();
                }
                return Ok(term);
            }
        }
    }

    let mut rpc = None;
    herdr.get_terminal(&mut rpc, terminal).await
}

#[derive(Debug, Clone)]
struct ResolvedCallerTerminal {
    terminal: String,
    created_ms: i64,
    term_info: Option<HcTerminal>,
}

fn non_empty_trimmed(val: Option<String>) -> Option<String> {
    val.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn env_hc_terminal_id() -> Option<String> {
    non_empty_trimmed(std::env::var("HC_TERMINAL_ID").ok())
}

async fn resolve_caller_terminal_with_env(
    herdr: &HerdrClient,
    explicit: Option<String>,
    env_val: Option<String>,
    fallback_terminal: Option<&str>,
    fallback_created_ms: Option<i64>,
    strict_fencing: bool,
    missing_msg: &str,
) -> Result<ResolvedCallerTerminal> {
    let explicit_or_env = non_empty_trimmed(explicit).or_else(|| non_empty_trimmed(env_val));
    let terminal = match explicit_or_env {
        Some(t) => t,
        None if !strict_fencing => fallback_terminal
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .with_context(|| missing_msg.to_string())?,
        None => bail!("{missing_msg}"),
    };

    match fetch_hc_terminal(herdr, &terminal).await {
        Ok(info) => {
            let created_ms = match info.created_ms {
                Some(ms) => ms,
                None if !strict_fencing => fallback_created_ms.with_context(|| {
                    format!("hc lookup for terminal '{terminal}' omitted created_ms")
                })?,
                None => bail!(
                    "attempt fencing identity check failed: hc lookup for terminal '{}' did not include created_ms",
                    terminal
                ),
            };
            Ok(ResolvedCallerTerminal {
                terminal,
                created_ms,
                term_info: Some(info),
            })
        }
        Err(err) => {
            if strict_fencing {
                bail!(
                    "attempt fencing identity check failed: unable to resolve terminal '{}' via hc ({})",
                    terminal,
                    err
                );
            }
            let created_ms = fallback_created_ms.with_context(|| {
                format!("unable to resolve created_ms for terminal '{terminal}' via hc: {err}")
            })?;
            Ok(ResolvedCallerTerminal {
                terminal,
                created_ms,
                term_info: None,
            })
        }
    }
}

async fn resolve_caller_terminal(
    herdr: &HerdrClient,
    explicit: Option<String>,
    fallback_terminal: Option<&str>,
    fallback_created_ms: Option<i64>,
    strict_fencing: bool,
    missing_msg: &str,
) -> Result<ResolvedCallerTerminal> {
    resolve_caller_terminal_with_env(
        herdr,
        explicit,
        std::env::var("HC_TERMINAL_ID").ok(),
        fallback_terminal,
        fallback_created_ms,
        strict_fencing,
        missing_msg,
    )
    .await
}

async fn close_worker_terminal(herdr: &HerdrClient, terminal: &str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(herdr.hc_bin())
            .args(["kill", terminal])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(herdr.hc_bin())
            .args(["rm", terminal])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
}

async fn apply_hc_labels(
    herdr: &HerdrClient,
    terminal: &str,
    labels: &[(&str, &str)],
) -> Result<()> {
    let mut cmd = Command::new(herdr.hc_bin());
    cmd.arg("label").arg(terminal);
    for (k, v) in labels {
        cmd.arg(format!("{k}={v}"));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(5), cmd.status())
        .await
        .context("hc label timed out")??;
    if !out.success() {
        bail!("hc label {} exited with {}", terminal, out);
    }
    Ok(())
}

async fn notify_daemon_orch_wake(paths: &CockpitPaths) {
    let req = IpcRequest {
        op: IpcOp::OrchWake,
        terminal: None,
        include_all: false,
    };
    let _ = query_socket_once::<Value>(&paths.socket_file(), &req).await;
}

pub async fn create_child_worktree(
    repo_root: &Path,
    run_id: &str,
    task_id: &str,
) -> Result<PathBuf> {
    let branch_name = format!("orch/{run_id}/{task_id}");
    let wt_dir = repo_root
        .join(".work")
        .join(format!("orch-{run_id}-{task_id}"));
    if wt_dir.join(".git").exists() {
        return Ok(wt_dir);
    }
    if let Some(parent) = wt_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let out = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["worktree", "add", "-B", &branch_name])
            .arg(&wt_dir)
            .arg("HEAD")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("git worktree add timed out")??;

    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(wt_dir)
}

pub async fn verify_and_remove_child_worktree(
    worktree_path: &Path,
    run_id: &str,
    task_id: &str,
    force: bool,
) -> Result<()> {
    if !worktree_path.exists() {
        return Ok(());
    }
    let ws = resolve_git_workspace(&worktree_path.to_string_lossy());
    let repo_root = ws
        .repo_root
        .map(PathBuf::from)
        .or_else(|| {
            worktree_path
                .parent()
                .and_then(|p| p.parent())
                .map(Path::to_path_buf)
        })
        .context("unable to resolve git repo_root for worktree release")?;

    let branch_name = format!("orch/{run_id}/{task_id}");

    if !force {
        let status_out = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("git")
                .arg("-C")
                .arg(worktree_path)
                .args(["status", "--porcelain"])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("git status --porcelain timed out")??;

        if !status_out.status.success() {
            bail!(
                "git status --porcelain failed: {}",
                String::from_utf8_lossy(&status_out.stderr).trim()
            );
        }
        let porcelain = String::from_utf8_lossy(&status_out.stdout);
        if !porcelain.trim().is_empty() {
            bail!(
                "refusing to release dirty worktree '{}' without --force (uncommitted changes present)",
                worktree_path.display()
            );
        }

        let ancestor_out = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("git")
                .arg("-C")
                .arg(&repo_root)
                .args(["merge-base", "--is-ancestor", &branch_name, "HEAD"])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("git merge-base --is-ancestor timed out")??;

        if !ancestor_out.status.success() {
            bail!(
                "refusing to release unmerged worktree branch '{}' without --force (not an ancestor of HEAD)",
                branch_name
            );
        }
    }

    let mut rm_cmd = Command::new("git");
    rm_cmd
        .arg("-C")
        .arg(&repo_root)
        .args(["worktree", "remove"]);
    if force {
        rm_cmd.arg("--force");
    }
    rm_cmd.arg(worktree_path).stdin(Stdio::null());

    let rm_out = tokio::time::timeout(Duration::from_secs(15), rm_cmd.output())
        .await
        .context("git worktree remove timed out")??;

    if !rm_out.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&rm_out.stderr).trim()
        );
    }
    Ok(())
}

pub async fn execute_orch_command(
    cmd: OrchCommands,
    _cfg: &CockpitConfig,
    paths: &CockpitPaths,
) -> Result<()> {
    if matches!(cmd, OrchCommands::Skill) {
        print!("{}", orchestration_skill_markdown());
        return Ok(());
    }

    let store = OrchStore::open(&paths.orch_db_file())?;
    let herdr = HerdrClient::default();

    match cmd {
        OrchCommands::Skill => unreachable!(),
        OrchCommands::Launch {
            agent,
            prompt,
            title,
            run,
            worktree,
            request_id,
            caller,
            json,
        } => {
            let _ = crate::daemon::ensure_daemon_running(paths).await;
            let sleeper = SystemOrchSleeper;
            match execute_launch(
                &store,
                &herdr,
                &sleeper,
                OrchLaunchArgs {
                    agent,
                    prompt,
                    title,
                    run,
                    worktree,
                    request_id,
                    caller_terminal: caller,
                },
            )
            .await
            {
                Ok(out) => {
                    notify_daemon_orch_wake(paths).await;
                    print_launch(&out, json)?;
                    Ok(())
                }
                Err(err) => {
                    if json {
                        if let Some(launch_err) = err.downcast_ref::<OrchLaunchError>() {
                            println!("{}", serde_json::to_string_pretty(launch_err)?);
                        }
                    }
                    Err(err)
                }
            }
        }
        OrchCommands::RunCreate {
            title,
            coordinator,
            request_id,
            json,
        } => {
            if let Some(ref req_id) = request_id {
                if let Some(cached) = store.get_mutation_receipt(req_id)? {
                    let run: OrchRun = serde_json::from_str(&cached)?;
                    print_run_create(&run, json)?;
                    return Ok(());
                }
            }

            let _ = crate::daemon::ensure_daemon_running(paths).await;

            let now_ms = chrono::Utc::now().timestamp_millis();
            let run = create_run_record(
                &store,
                &herdr,
                title.as_deref(),
                coordinator,
                "coordinator terminal not specified; pass --coordinator <term> or run inside Herdr ($HC_TERMINAL_ID)",
                now_ms,
            )
            .await?;

            let resp_json = serde_json::to_string(&run)?;
            if let Some(ref req_id) = request_id {
                store.save_mutation_receipt(req_id, "run-create", &resp_json, now_ms)?;
            }

            notify_daemon_orch_wake(paths).await;

            print_run_create(&run, json)?;
            Ok(())
        }
        OrchCommands::TaskCreate {
            run,
            id,
            title,
            prompt,
            deps,
            request_id,
            json,
        } => {
            if let Some(ref req_id) = request_id {
                if let Some(cached) = store.get_mutation_receipt(req_id)? {
                    let task: OrchTask = serde_json::from_str(&cached)?;
                    print_task_create(&task, json)?;
                    return Ok(());
                }
            }

            let now_ms = chrono::Utc::now().timestamp_millis();
            let parsed_deps = parse_deps_arg(deps.as_deref())?;
            let task_input = OrchTask {
                id,
                run_id: run,
                title,
                prompt,
                deps: parsed_deps,
                failure_count: 0,
                status: TaskStatus::Pending,
                result_summary: None,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
            };

            let saved = store.create_or_update_task(&task_input)?;
            let resp_json = serde_json::to_string(&saved)?;
            if let Some(ref req_id) = request_id {
                store.save_mutation_receipt(req_id, "task-create", &resp_json, now_ms)?;
            }

            notify_daemon_orch_wake(paths).await;
            print_task_create(&saved, json)?;
            Ok(())
        }
        OrchCommands::Dispatch {
            run,
            task,
            title,
            prompt,
            terminal,
            agent,
            worktree,
            request_id,
            caller,
            json,
        } => {
            let _ = crate::daemon::ensure_daemon_running(paths).await;
            let sleeper = SystemOrchSleeper;
            let out = execute_dispatch(
                &store,
                &herdr,
                &sleeper,
                OrchDispatchArgs {
                    run,
                    task,
                    title,
                    prompt,
                    terminal,
                    agent,
                    worktree,
                    request_id,
                    caller_terminal: caller,
                },
            )
            .await?;
            notify_daemon_orch_wake(paths).await;
            print_dispatch(&out, json)?;
            Ok(())
        }
        OrchCommands::Send {
            to,
            msg_type,
            outcome,
            dispatch_id,
            task_id: _,
            body,
            request_id,
            caller,
            json,
        } => {
            let sleeper = SystemOrchSleeper;
            if let Some(to_target) = to {
                if msg_type.is_some() || outcome.is_some() || dispatch_id.is_some() {
                    bail!("--to cannot be combined with --type, --outcome, or --dispatch-id");
                }
                let out = execute_send_to(
                    &store,
                    &herdr,
                    &sleeper,
                    OrchSendToArgs {
                        to: to_target,
                        body,
                        request_id,
                        caller_terminal: caller,
                    },
                )
                .await?;
                notify_daemon_orch_wake(paths).await;
                print_send_to(&out, json)?;
                return Ok(());
            }

            let msg_type =
                msg_type.context("--type is required unless --to dispatch:<id> is specified")?;
            let dispatch_id = dispatch_id
                .context("--dispatch-id is required unless --to dispatch:<id> is specified")?;
            let msg = execute_send(
                &store,
                &herdr,
                &sleeper,
                OrchSendArgs {
                    msg_type,
                    outcome,
                    dispatch_id,
                    body,
                    request_id,
                    caller_terminal: caller,
                },
            )
            .await?;
            notify_daemon_orch_wake(paths).await;
            print_message(&msg, json)?;
            Ok(())
        }
        OrchCommands::Retry {
            dispatch_id,
            request_id,
            caller,
            json,
        } => {
            let _ = crate::daemon::ensure_daemon_running(paths).await;
            let sleeper = SystemOrchSleeper;
            let out = execute_retry(
                &store,
                &herdr,
                &sleeper,
                OrchRetryArgs {
                    dispatch_id,
                    request_id,
                    caller_terminal: caller,
                },
            )
            .await?;
            notify_daemon_orch_wake(paths).await;
            print_retry(&out, json)?;
            Ok(())
        }
        OrchCommands::Stop {
            dispatch_id,
            request_id,
            caller,
            json,
        } => {
            let sleeper = SystemOrchSleeper;
            let out = execute_stop(
                &store,
                &herdr,
                &sleeper,
                OrchStopArgs {
                    dispatch_id,
                    request_id,
                    caller_terminal: caller,
                },
            )
            .await?;
            notify_daemon_orch_wake(paths).await;
            print_stop(&out, json)?;
            Ok(())
        }
        OrchCommands::Ask {
            dispatch_id,
            question,
            request_id,
            caller,
            json,
        } => {
            let sleeper = SystemOrchSleeper;
            let (q_thread, msg) = execute_ask(
                &store,
                &herdr,
                &sleeper,
                OrchAskArgs {
                    dispatch_id,
                    question,
                    request_id,
                    caller_terminal: caller,
                },
            )
            .await?;
            notify_daemon_orch_wake(paths).await;
            print_ask(&q_thread, &msg, json)?;
            Ok(())
        }
        OrchCommands::Reply {
            question_id,
            body,
            request_id,
            json,
        } => {
            if let Some(ref req_id) = request_id {
                if let Some(cached) = store.get_mutation_receipt(req_id)? {
                    let (q_thread, msg): (QuestionThread, OrchMessage) =
                        serde_json::from_str(&cached)?;
                    print_reply(&q_thread, &msg, json)?;
                    return Ok(());
                }
            }

            let q_existing = store
                .get_question_thread(&question_id)?
                .with_context(|| format!("question thread '{}' does not exist", question_id))?;
            let disp = store
                .get_dispatch(&q_existing.dispatch_id)?
                .with_context(|| format!("dispatch '{}' does not exist", q_existing.dispatch_id))?;

            let child_current_turn = fetch_hc_terminal(&herdr, &disp.assignee_terminal)
                .await
                .ok()
                .map(|t| t.turn)
                .unwrap_or(disp.turn_at_dispatch);

            let now_ms = chrono::Utc::now().timestamp_millis();
            let (q_thread, msg) =
                store.record_reply(&question_id, &body, child_current_turn, now_ms)?;

            let resp_json = serde_json::to_string(&(&q_thread, &msg))?;
            if let Some(ref req_id) = request_id {
                store.save_mutation_receipt(req_id, "reply", &resp_json, now_ms)?;
            }

            notify_daemon_orch_wake(paths).await;
            print_reply(&q_thread, &msg, json)?;
            Ok(())
        }
        OrchCommands::Check {
            run,
            terminal,
            peek,
            json,
        } => {
            let env_term = env_hc_terminal_id();

            let target_terminal = match (env_term, terminal) {
                (Some(caller), Some(override_term)) => {
                    if override_term != caller && !peek {
                        let caller_is_active_worker = store
                            .list_active_dispatches()?
                            .iter()
                            .any(|d| d.assignee_terminal == caller);
                        if caller_is_active_worker {
                            bail!(
                                "worker terminal '{}' cannot claim inbox messages for '{}' without --peek",
                                caller,
                                override_term
                            );
                        }
                    }
                    override_term
                }
                (Some(caller), None) => caller,
                (None, Some(explicit)) => explicit,
                (None, None) => {
                    if let Some(ref r_id) = run {
                        store
                            .get_run(r_id)?
                            .map(|r| r.coordinator_terminal)
                            .context(
                                "pass --terminal <term> or run inside Herdr ($HC_TERMINAL_ID)",
                            )?
                    } else {
                        bail!("target terminal not specified; pass --terminal <term> or run inside Herdr ($HC_TERMINAL_ID)");
                    }
                }
            };

            let now_ms = chrono::Utc::now().timestamp_millis();
            let res = store.claim_inbox_messages(&target_terminal, run.as_deref(), peek, now_ms)?;
            print_inbox_check(&res, json)?;
            Ok(())
        }
        OrchCommands::Status { run, json } => {
            let runs = if let Some(ref run_id) = run {
                let r = store
                    .get_run(run_id)?
                    .with_context(|| format!("run '{}' does not exist", run_id))?;
                vec![r]
            } else {
                store.list_runs()?
            };

            let now_ms = chrono::Utc::now().timestamp_millis();
            let pending_targets = store.pending_pointer_targets(now_ms)?;
            let mut views = Vec::new();
            for r in runs {
                let tasks = store.list_tasks(&r.id)?;
                let dispatches = store.list_dispatches_for_run(&r.id)?;
                let messages = store.list_messages_for_run(&r.id)?;
                let open_questions = store.list_open_questions_for_run(&r.id)?;
                let unacknowledged_messages = pending_targets
                    .iter()
                    .filter(|t| t.run_id == r.id)
                    .map(|t| t.pending_count)
                    .sum();
                views.push(RunStatusView {
                    run: r,
                    tasks,
                    dispatches,
                    messages,
                    open_questions,
                    unacknowledged_messages,
                });
            }

            let out = OrchStatusOutput { runs: views };
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                print_human_status(&out);
            }
            Ok(())
        }
        OrchCommands::Release {
            dispatch_id,
            force,
            json,
        } => {
            let disp = store
                .get_dispatch(&dispatch_id)?
                .with_context(|| format!("dispatch '{}' does not exist", dispatch_id))?;

            let mut worktree_removed = false;
            if disp.worktree_mode == "new-child" {
                if let Some(ref wt_path) = disp.worktree_path {
                    verify_and_remove_child_worktree(
                        Path::new(wt_path),
                        &disp.run_id,
                        &disp.task_id,
                        force,
                    )
                    .await?;
                    worktree_removed = true;
                }
            }

            close_worker_terminal(&herdr, &disp.assignee_terminal).await;

            let now_ms = chrono::Utc::now().timestamp_millis();
            let final_status = if matches!(
                disp.status,
                DispatchStatus::Completed
                    | DispatchStatus::Failed
                    | DispatchStatus::Stopped
                    | DispatchStatus::Superseded
            ) {
                disp.status
            } else {
                store.update_dispatch_status(&disp.id, DispatchStatus::Stopped, now_ms)?;
                DispatchStatus::Stopped
            };

            notify_daemon_orch_wake(paths).await;

            let out = ReleaseOutput {
                dispatch_id: disp.id,
                assignee_terminal: disp.assignee_terminal,
                status: final_status,
                terminal_released: true,
                worktree_removed,
                worktree_path: disp.worktree_path,
            };

            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "Released dispatch {} (terminal={}, status={}, worktree_removed={})",
                    out.dispatch_id, out.assignee_terminal, out.status, out.worktree_removed
                );
            }
            Ok(())
        }
    }
}

pub async fn execute_dispatch(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchDispatchArgs,
) -> Result<DispatchOutput> {
    if let Some(ref req_id) = args.request_id {
        if let Some(cached) = store.get_mutation_receipt(req_id)? {
            let out: DispatchOutput = serde_json::from_str(&cached)?;
            return Ok(out);
        }
    }

    if !matches!(args.worktree.as_str(), "current" | "new-child") {
        bail!(
            "invalid --worktree mode '{}'; expected 'current' or 'new-child'",
            args.worktree
        );
    }

    let now_ms = sleeper.now_ms();
    let run_row = store
        .get_run(&args.run)?
        .with_context(|| format!("run '{}' does not exist", args.run))?;

    let resolved_caller = resolve_caller_terminal(
        herdr,
        args.caller_terminal,
        Some(&run_row.coordinator_terminal),
        Some(run_row.coordinator_created_ms),
        false,
        "caller terminal could not be resolved",
    )
    .await?;
    let caller_terminal = resolved_caller.terminal;
    let caller_created_ms = resolved_caller.created_ms;
    let caller_term_info = resolved_caller.term_info;

    let depth = store.caller_orchestration_depth(&caller_terminal, caller_created_ms)?;
    if depth > MAX_ORCH_DEPTH {
        bail!(
            "orchestration depth {} exceeds MAX_ORCH_DEPTH ({})",
            depth,
            MAX_ORCH_DEPTH
        );
    }

    let task_row = match store.get_task(&args.run, &args.task)? {
        Some(mut existing) => {
            if existing.failure_count >= MAX_DISPATCH_FAILURES
                || existing.status == TaskStatus::CircuitBroken
            {
                bail!(
                    "circuit breaker tripped for task '{}' in run '{}' (failure_count = {})",
                    args.task,
                    args.run,
                    existing.failure_count
                );
            }
            if let Some(new_prompt) = args.prompt {
                existing.prompt = new_prompt;
                if let Some(new_title) = args.title {
                    existing.title = new_title;
                }
                existing.updated_at_ms = now_ms;
                store.create_or_update_task(&existing)?
            } else {
                existing
            }
        }
        None => {
            let inline_prompt = args.prompt.with_context(|| {
                format!(
                    "task '{}' does not exist in run '{}'; provide --prompt to create it inline",
                    args.task, args.run
                )
            })?;
            let inline_title = args.title.unwrap_or_else(|| format!("Task {}", args.task));
            store.create_or_update_task(&OrchTask {
                id: args.task.clone(),
                run_id: args.run.clone(),
                title: inline_title,
                prompt: inline_prompt,
                deps: Vec::new(),
                failure_count: 0,
                status: TaskStatus::Ready,
                result_summary: None,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
            })?
        }
    };

    let base_cwd = caller_term_info
        .as_ref()
        .map(|t| PathBuf::from(t.effective_cwd()))
        .filter(|p| p.exists())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let (dispatch_cwd, worktree_path_opt) = if args.worktree == "new-child" {
        let ws = resolve_git_workspace(&base_cwd.to_string_lossy());
        let repo_root = ws
            .repo_root
            .map(PathBuf::from)
            .unwrap_or_else(|| base_cwd.clone());
        let wt_dir = create_child_worktree(&repo_root, &args.run, &args.task).await?;
        let wt_str = wt_dir.to_string_lossy().to_string();
        (wt_dir, Some(wt_str))
    } else {
        (base_cwd, None)
    };

    let dispatch_id = generate_orch_id("disp");
    let preamble = build_dispatch_preamble(
        &args.run,
        &task_row.id,
        &dispatch_id,
        &caller_terminal,
        depth,
        &task_row.prompt,
    );

    let labels = [
        ("cockpit.orch.role", "worker"),
        ("cockpit.orch.run", args.run.as_str()),
        ("cockpit.orch.task", task_row.id.as_str()),
        ("cockpit.orch.dispatch", dispatch_id.as_str()),
        ("cockpit.parent", caller_terminal.as_str()),
    ];

    let is_attached = args.terminal.is_some();
    let launch_res: Result<(String, HcTerminal)> = if let Some(ref existing_term) = args.terminal {
        match fetch_hc_terminal(herdr, existing_term).await {
            Ok(fetched) => {
                let _ = apply_hc_labels(herdr, existing_term, &labels).await;
                Ok(("attached".to_string(), fetched))
            }
            Err(err) => Err(err),
        }
    } else {
        let raw_agent = args.agent.unwrap_or_else(|| "claude".to_string());
        let agent_cmd = map_agent_preset(&raw_agent);
        match herdr
            .run_worker(
                &caller_terminal,
                &dispatch_cwd,
                &labels,
                &agent_cmd,
                &preamble,
            )
            .await
        {
            Ok(spawned) => Ok((raw_agent, spawned)),
            Err(err) => Err(err),
        }
    };

    let (agent_kind_str, mut worker_term) = match launch_res {
        Ok(pair) => pair,
        Err(err) => {
            if let Some(ref wt_path) = worktree_path_opt {
                let note =
                    match cleanup_failed_child_worktree(wt_path, &args.run, &task_row.id).await {
                        Ok(cleaned) | Err(cleaned) => cleaned,
                    };
                return Err(anyhow::Error::new(LaunchNoteError { source: err, note }));
            }
            return Err(err);
        }
    };

    let worker_created_ms = worker_term.created_ms.unwrap_or(now_ms);
    let mut disp = DispatchContext {
        id: dispatch_id.clone(),
        run_id: args.run.clone(),
        task_id: task_row.id.clone(),
        parent_terminal: caller_terminal,
        assignee_terminal: worker_term.address.clone(),
        terminal_created_ms: worker_created_ms,
        turn_at_dispatch: worker_term.turn,
        agent_kind: agent_kind_str,
        worktree_mode: args.worktree.clone(),
        worktree_path: worktree_path_opt.clone(),
        depth,
        status: DispatchStatus::Dispatched,
        idle_since_ms: None,
        blocked_since_ms: None,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };

    if let Err(err) = store.create_dispatch(&disp) {
        if !is_attached {
            close_worker_terminal(herdr, &worker_term.address).await;
        }
        if let Some(ref wt_path) = worktree_path_opt {
            let note = match cleanup_failed_child_worktree(wt_path, &args.run, &task_row.id).await {
                Ok(cleaned) | Err(cleaned) => cleaned,
            };
            return Err(anyhow!("{err} ({note})"));
        }
        return Err(err);
    }

    let attach_send_failed = if let Some(ref existing_term) = args.terminal {
        !herdr
            .send_guarded_if_idle(existing_term, &preamble)
            .await
            .unwrap_or(false)
    } else {
        if worker_term.first_message.is_none()
            && worker_term.turn == 0
            && worker_term.state != "finished"
            && matches!(worker_term.status_name(), None | Some("idle" | "done" | ""))
        {
            let _ = herdr
                .send_guarded_if_idle(&worker_term.address, &preamble)
                .await;
        }
        false
    };

    let first_message_failed = matches!(worker_term.first_message, Some(ref fm) if !fm.sent);

    if !is_attached
        && !first_message_failed
        && worker_term.state != "finished"
        && worker_term.status_name() != Some("error")
    {
        let recheck_start_ms = sleeper.now_ms();
        let recheck_timeout_ms = DISPATCH_LIVENESS_RECHECK_TIMEOUT.as_millis() as i64;
        let recheck_step_ms = (DISPATCH_LIVENESS_RECHECK_INTERVAL.as_millis() as i64).max(1);

        loop {
            if let Some(current) = store.get_dispatch(&dispatch_id)? {
                if current.status != DispatchStatus::Dispatched {
                    disp = current;
                    break;
                }
            }
            let elapsed_ms = sleeper.now_ms().saturating_sub(recheck_start_ms);
            if elapsed_ms >= recheck_timeout_ms {
                break;
            }
            let step_ms = (recheck_timeout_ms - elapsed_ms)
                .min(recheck_step_ms)
                .max(1);
            sleeper.sleep(Duration::from_millis(step_ms as u64)).await;

            if let Some(current) = store.get_dispatch(&dispatch_id)? {
                if current.status != DispatchStatus::Dispatched {
                    disp = current;
                    break;
                }
            }
            if let Ok(fetched) = fetch_hc_terminal(herdr, &worker_term.address).await {
                if let Some(current) = store.get_dispatch(&dispatch_id)? {
                    if current.status != DispatchStatus::Dispatched {
                        disp = current;
                        break;
                    }
                }
                let preserved_fm = worker_term.first_message.clone();
                worker_term = fetched;
                if worker_term.first_message.is_none() {
                    worker_term.first_message = preserved_fm;
                }
                if worker_term.state == "finished" || worker_term.status_name() == Some("error") {
                    break;
                }
            }
        }
    }

    if let Some(current) = store.get_dispatch(&dispatch_id)? {
        disp = current;
    }

    let failure_info: Option<(String, Option<i32>, Option<String>)> = if attach_send_failed {
        Some((
            "failed delivering dispatch preamble to attached terminal (not idle)".to_string(),
            None,
            None,
        ))
    } else if disp.status != DispatchStatus::Dispatched {
        None
    } else if let Some(fm) = worker_term.first_message.as_ref().filter(|fm| !fm.sent) {
        let base = format!(
            "first_message not sent ({})",
            fm.reason.as_deref().unwrap_or("blocked_or_trust_dialog")
        );
        if worker_term.state == "finished" || worker_term.status_name() == Some("error") {
            let tail = herdr
                .read_tail_lines(&worker_term.address, WORKER_LAUNCH_TAIL_LINES)
                .await
                .unwrap_or_default();
            let tail_trimmed = tail.trim().to_string();
            let tail_opt = (!tail_trimmed.is_empty()).then_some(tail_trimmed);
            let reason = format_launch_failure_reason(
                &base,
                worker_term.exit_code,
                worker_term.status_msg().or(worker_term.reason.as_deref()),
                tail_opt.as_deref(),
            );
            Some((reason, worker_term.exit_code, tail_opt))
        } else {
            Some((base, None, None))
        }
    } else if worker_term.state == "finished" || worker_term.status_name() == Some("error") {
        let tail = herdr
            .read_tail_lines(&worker_term.address, WORKER_LAUNCH_TAIL_LINES)
            .await
            .unwrap_or_default();
        let tail_trimmed = tail.trim().to_string();
        let tail_opt = (!tail_trimmed.is_empty()).then_some(tail_trimmed);
        let reason = format_launch_failure_reason(
            "worker terminal exited or entered error state at spawn",
            worker_term.exit_code,
            worker_term.status_msg().or(worker_term.reason.as_deref()),
            tail_opt.as_deref(),
        );
        Some((reason, worker_term.exit_code, tail_opt))
    } else {
        None
    };

    let (failure_reason, exit_code, output_tail, worktree_cleanup) = match failure_info {
        Some((mut reason, code, tail)) => {
            if !is_attached {
                close_worker_terminal(herdr, &worker_term.address).await;
            }
            let wt_cleanup = if args.worktree == "new-child" {
                if let Some(ref wt_path) = disp.worktree_path.clone() {
                    match cleanup_failed_child_worktree(wt_path, &args.run, &task_row.id).await {
                        Ok(cleaned) => {
                            store.clear_dispatch_worktree_path(&dispatch_id, sleeper.now_ms())?;
                            Some(cleaned)
                        }
                        Err(kept) => {
                            reason.push_str(&format!("\n{kept}"));
                            Some(kept)
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };

            store.record_task_failure(
                &args.run,
                &task_row.id,
                Some(&dispatch_id),
                &reason,
                sleeper.now_ms(),
            )?;
            if let Some(updated) = store.get_dispatch(&dispatch_id)? {
                disp = updated;
            }
            (Some(reason), code, tail, wt_cleanup)
        }
        None => (None, None, None, None),
    };

    let out = DispatchOutput {
        dispatch: disp,
        failure_reason,
        exit_code,
        output_tail,
        worktree_cleanup,
    };

    let resp_json = serde_json::to_string(&out)?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "dispatch", &resp_json, sleeper.now_ms())?;
    }

    Ok(out)
}

async fn cleanup_failed_child_worktree(
    wt_path: &str,
    run_id: &str,
    task_id: &str,
) -> std::result::Result<String, String> {
    match verify_and_remove_child_worktree(Path::new(wt_path), run_id, task_id, false).await {
        Ok(()) => Ok(format!("removed clean child worktree '{}'", wt_path)),
        Err(wt_err) => Err(format!(
            "kept child worktree '{}' because it is dirty or unmerged ({})",
            wt_path, wt_err
        )),
    }
}

fn format_launch_failure_reason(
    base: &str,
    exit_code: Option<i32>,
    detail: Option<&str>,
    tail: Option<&str>,
) -> String {
    let mut meta = Vec::new();
    if let Some(code) = exit_code {
        meta.push(format!("exit_code={code}"));
    }
    if let Some(d) = detail.map(str::trim).filter(|s| !s.is_empty()) {
        meta.push(format!("detail={d}"));
    }
    let header = if meta.is_empty() {
        base.to_string()
    } else {
        format!("{base} ({})", meta.join(", "))
    };
    match tail.map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => format!("{header}\n--- last lines ---\n{t}"),
        None => header,
    }
}

pub async fn wait_for_dispatch(
    store: &OrchStore,
    dispatch_id: &str,
    timeout: Duration,
    poll_interval: Duration,
    sleeper: &dyn OrchSleeper,
) -> Result<DispatchContext> {
    let start_ms = sleeper.now_ms();
    let timeout_ms = timeout.as_millis() as i64;
    let poll_ms = (poll_interval.as_millis() as i64).max(1);

    loop {
        if let Some(disp) = store.get_dispatch(dispatch_id)? {
            if disp.status != DispatchStatus::Starting && !disp.assignee_terminal.is_empty() {
                return Ok(disp);
            }
        }
        let elapsed_ms = sleeper.now_ms().saturating_sub(start_ms);
        if elapsed_ms >= timeout_ms {
            bail!("dispatch '{}' does not exist", dispatch_id);
        }
        let step_ms = (timeout_ms - elapsed_ms).min(poll_ms).max(1);
        sleeper.sleep(Duration::from_millis(step_ms as u64)).await;
    }
}

pub async fn execute_send(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchSendArgs,
) -> Result<OrchMessage> {
    if let Some(ref req_id) = args.request_id {
        if let Some(cached) = store.get_mutation_receipt(req_id)? {
            let msg: OrchMessage = serde_json::from_str(&cached)?;
            return Ok(msg);
        }
    }

    let parsed_type = MessageType::from_str(args.msg_type.trim())?;
    if !matches!(
        parsed_type,
        MessageType::WorkerDone | MessageType::Escalation | MessageType::Status
    ) {
        bail!(
            "`tchelvis orch send` supports worker_done, escalation, or status (got {})",
            parsed_type
        );
    }

    let _disp = wait_for_dispatch(
        store,
        &args.dispatch_id,
        DISPATCH_WAIT_TIMEOUT,
        DISPATCH_WAIT_POLL_INTERVAL,
        sleeper,
    )
    .await?;

    let resolved_caller = resolve_caller_terminal(
        herdr,
        args.caller_terminal,
        None,
        None,
        true,
        "caller terminal identity could not be established for attempt fencing; pass --caller <term> or run inside Herdr ($HC_TERMINAL_ID)",
    )
    .await?;
    let caller_terminal = resolved_caller.terminal;
    let caller_created_ms = resolved_caller.created_ms;

    let now_ms = sleeper.now_ms();
    let msg = match parsed_type {
        MessageType::WorkerDone => {
            let out_str = args.outcome.as_deref().unwrap_or("succeeded");
            store.record_worker_done(
                &args.dispatch_id,
                &caller_terminal,
                caller_created_ms,
                out_str,
                &args.body,
                now_ms,
            )?
        }
        MessageType::Escalation | MessageType::Status => store.record_worker_message(
            &args.dispatch_id,
            &caller_terminal,
            caller_created_ms,
            parsed_type,
            &args.body,
            now_ms,
        )?,
        _ => unreachable!(),
    };

    let resp_json = serde_json::to_string(&msg)?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "send", &resp_json, now_ms)?;
    }

    Ok(msg)
}

pub async fn execute_ask(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchAskArgs,
) -> Result<(QuestionThread, OrchMessage)> {
    if let Some(ref req_id) = args.request_id {
        if let Some(cached) = store.get_mutation_receipt(req_id)? {
            let (q_thread, msg): (QuestionThread, OrchMessage) = serde_json::from_str(&cached)?;
            return Ok((q_thread, msg));
        }
    }

    let _disp = wait_for_dispatch(
        store,
        &args.dispatch_id,
        DISPATCH_WAIT_TIMEOUT,
        DISPATCH_WAIT_POLL_INTERVAL,
        sleeper,
    )
    .await?;

    let resolved_caller = resolve_caller_terminal(
        herdr,
        args.caller_terminal,
        None,
        None,
        true,
        "caller terminal identity could not be established for attempt fencing; pass --caller <term> or run inside Herdr ($HC_TERMINAL_ID)",
    )
    .await?;
    let caller_terminal = resolved_caller.terminal;
    let caller_created_ms = resolved_caller.created_ms;

    let now_ms = sleeper.now_ms();
    let (q_thread, msg) = store.record_question(
        &args.dispatch_id,
        &caller_terminal,
        caller_created_ms,
        &args.question,
        now_ms,
    )?;

    let resp_json = serde_json::to_string(&(&q_thread, &msg))?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "ask", &resp_json, now_ms)?;
    }

    Ok((q_thread, msg))
}

#[derive(Debug)]
struct LaunchNoteError {
    source: anyhow::Error,
    note: String,
}

impl fmt::Display for LaunchNoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.source, self.note)
    }
}

impl std::error::Error for LaunchNoteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.root_cause())
    }
}

#[cfg(unix)]
fn is_executable_file_path(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file_path(path: &Path) -> bool {
    path.is_file()
}

pub fn preflight_agent_with_path(
    agent: &str,
    path_override: Option<&OsStr>,
) -> std::result::Result<String, Box<OrchLaunchError>> {
    let mapped = map_agent_preset(agent);
    let Some(program) = mapped.split_whitespace().next() else {
        return Err(Box::new(OrchLaunchError {
            failed_stage: "agent_preflight".to_string(),
            agent: agent.to_string(),
            detail: "agent command cannot be empty".to_string(),
            exit_code: None,
            output_tail: None,
            run_id: None,
            task_id: None,
            dispatch_id: None,
        }));
    };

    let found = if program.contains(std::path::MAIN_SEPARATOR) || program.contains('/') {
        let p = Path::new(program);
        is_executable_file_path(p).then(|| p.to_path_buf())
    } else {
        let search_path = path_override
            .map(std::ffi::OsString::from)
            .or_else(|| std::env::var_os("PATH"));
        search_path.and_then(|pv| {
            std::env::split_paths(&pv)
                .map(|dir| dir.join(program))
                .find(|cand| is_executable_file_path(cand))
        })
    };

    match found {
        Some(resolved) => Ok(resolved.to_string_lossy().to_string()),
        None => Err(Box::new(OrchLaunchError {
            failed_stage: "agent_preflight".to_string(),
            agent: agent.to_string(),
            detail: format!(
                "agent executable '{}' (resolved from preset '{}') not found on PATH",
                program, agent
            ),
            exit_code: None,
            output_tail: None,
            run_id: None,
            task_id: None,
            dispatch_id: None,
        })),
    }
}

async fn create_run_record(
    store: &OrchStore,
    herdr: &HerdrClient,
    title: Option<&str>,
    coordinator: Option<String>,
    missing_msg: &str,
    now_ms: i64,
) -> Result<OrchRun> {
    let resolved_coord =
        resolve_caller_terminal(herdr, coordinator, None, Some(now_ms), false, missing_msg).await?;
    let coord_term = resolved_coord.terminal;
    let coordinator_created_ms = resolved_coord.created_ms;

    let run_id = generate_orch_id("run");
    let run_title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Orchestration Run {run_id}"));

    let run = OrchRun {
        id: run_id.clone(),
        title: run_title,
        coordinator_terminal: coord_term.clone(),
        coordinator_created_ms,
        status: RunStatus::Active,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };

    store.create_run(&run)?;
    let labels = [
        ("cockpit.orch.role", "coordinator"),
        ("cockpit.orch.run", run_id.as_str()),
    ];
    let _ = apply_hc_labels(herdr, &coord_term, &labels).await;
    Ok(run)
}

pub async fn execute_launch(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchLaunchArgs,
) -> Result<LaunchOutput> {
    execute_launch_with_path(store, herdr, sleeper, args, None).await
}

pub async fn execute_launch_with_path(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchLaunchArgs,
    path_override: Option<&OsStr>,
) -> Result<LaunchOutput> {
    if let Some(ref req_id) = args.request_id {
        if let Some(cached) = store.get_mutation_receipt(req_id)? {
            let out: LaunchOutput = serde_json::from_str(&cached)?;
            return Ok(out);
        }
    }

    if !matches!(args.worktree.as_str(), "current" | "new-child") {
        bail!(
            "invalid --worktree mode '{}'; expected 'current' or 'new-child'",
            args.worktree
        );
    }

    if let Err(preflight_err) = preflight_agent_with_path(&args.agent, path_override) {
        return Err(anyhow::Error::new(*preflight_err));
    }

    let now_ms = sleeper.now_ms();
    let run = if let Some(ref run_id) = args.run {
        store
            .get_run(run_id)?
            .with_context(|| format!("run '{}' does not exist", run_id))?
    } else {
        create_run_record(
            store,
            herdr,
            args.title.as_deref(),
            args.caller_terminal.clone(),
            "caller terminal not specified; pass --caller <term> or run inside Herdr ($HC_TERMINAL_ID)",
            now_ms,
        )
        .await?
    };

    let task_id = generate_orch_id("task");
    let dispatch_caller = if args.run.is_some() {
        args.caller_terminal.clone()
    } else {
        Some(run.coordinator_terminal.clone())
    };

    let disp_out = match execute_dispatch(
        store,
        herdr,
        sleeper,
        OrchDispatchArgs {
            run: run.id.clone(),
            task: task_id.clone(),
            title: args.title.clone(),
            prompt: Some(args.prompt.clone()),
            terminal: None,
            agent: Some(args.agent.clone()),
            worktree: args.worktree.clone(),
            request_id: None,
            caller_terminal: dispatch_caller,
        },
    )
    .await
    {
        Ok(out) => out,
        Err(err) => {
            let run_err = err.chain().find_map(|e| e.downcast_ref::<HcRunError>());
            let exit_code = run_err.and_then(|e| e.exit_code);
            let output_tail = run_err.and_then(|e| {
                let raw = if !e.stdout.trim().is_empty() {
                    e.stdout.trim()
                } else {
                    e.stderr.trim()
                };
                (!raw.is_empty()).then(|| raw.to_string())
            });
            return Err(anyhow::Error::new(OrchLaunchError {
                failed_stage: "spawn".to_string(),
                agent: args.agent,
                detail: err.to_string(),
                exit_code,
                output_tail,
                run_id: Some(run.id),
                task_id: Some(task_id),
                dispatch_id: None,
            }));
        }
    };

    if let Some(reason) = disp_out.failure_reason {
        return Err(anyhow::Error::new(OrchLaunchError {
            failed_stage: "launch".to_string(),
            agent: args.agent,
            detail: reason,
            exit_code: disp_out.exit_code,
            output_tail: disp_out.output_tail,
            run_id: Some(run.id),
            task_id: Some(disp_out.dispatch.task_id),
            dispatch_id: Some(disp_out.dispatch.id),
        }));
    }

    let task = store
        .get_task(&run.id, &disp_out.dispatch.task_id)?
        .context("task missing after dispatch")?;

    let out = LaunchOutput {
        run,
        task,
        dispatch: disp_out.dispatch,
    };

    let resp_json = serde_json::to_string(&out)?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "launch", &resp_json, sleeper.now_ms())?;
    }

    Ok(out)
}

pub async fn execute_send_to(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchSendToArgs,
) -> Result<SendToOutput> {
    if let Some(ref req_id) = args.request_id {
        if let Some(cached) = store.get_mutation_receipt(req_id)? {
            let out: SendToOutput = serde_json::from_str(&cached)?;
            return Ok(out);
        }
    }

    let target_dispatch_id = args
        .to
        .trim()
        .strip_prefix("dispatch:")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| {
            format!(
                "invalid --to target '{}'; expected 'dispatch:<id>'",
                args.to
            )
        })?;

    let resolved_caller = resolve_caller_terminal(
        herdr,
        args.caller_terminal,
        None,
        None,
        true,
        "caller terminal identity could not be established for attempt fencing; pass --caller <term> or run inside Herdr ($HC_TERMINAL_ID)",
    )
    .await?;

    let disp = store.verify_parent_caller(
        target_dispatch_id,
        &resolved_caller.terminal,
        resolved_caller.created_ms,
    )?;

    if matches!(
        disp.status,
        DispatchStatus::Completed
            | DispatchStatus::Failed
            | DispatchStatus::Stopped
            | DispatchStatus::Superseded
    ) {
        bail!(
            "cannot send message to dispatch '{}' because its status is '{}'; use `tchelvis orch retry {}` to launch a new attempt",
            disp.id,
            disp.status,
            disp.id
        );
    }

    let worker_term = match fetch_hc_terminal(herdr, &disp.assignee_terminal).await {
        Ok(term) => term,
        Err(err) => {
            bail!(
                "worker terminal '{}' for dispatch '{}' is unavailable ({}); use `tchelvis orch retry {}` to launch a new attempt",
                disp.assignee_terminal,
                disp.id,
                err,
                disp.id
            );
        }
    };

    if worker_term
        .created_ms
        .is_some_and(|c| c != disp.terminal_created_ms)
    {
        bail!(
            "worker terminal '{}' for dispatch '{}' was recreated (expected created_ms={}, got {:?}); use `tchelvis orch retry {}` to launch a new attempt",
            disp.assignee_terminal,
            disp.id,
            disp.terminal_created_ms,
            worker_term.created_ms,
            disp.id
        );
    }

    if worker_term.state == "finished" || worker_term.status_name() == Some("error") {
        bail!(
            "worker terminal '{}' for dispatch '{}' is '{}' and cannot receive messages; use `tchelvis orch retry {}` to launch a new attempt",
            disp.assignee_terminal,
            disp.id,
            worker_term.state,
            disp.id
        );
    }

    let can_try_immediate = matches!(worker_term.status_name(), Some("idle" | "done" | "") | None)
        && !args.body.contains('\n')
        && !args.body.contains('\r')
        && args.body.len() <= 500;

    let delivered_immediately = if can_try_immediate {
        herdr
            .send_guarded_if_idle(&disp.assignee_terminal, &args.body)
            .await
            .unwrap_or(false)
    } else {
        false
    };

    let now_ms = sleeper.now_ms();
    let (message, dispatch_status) = store.record_coordinator_message(
        &disp.id,
        &resolved_caller.terminal,
        resolved_caller.created_ms,
        &args.body,
        delivered_immediately,
        worker_term.turn,
        now_ms,
    )?;

    let out = SendToOutput {
        message,
        dispatch_id: disp.id,
        dispatch_status,
        delivered_immediately,
    };

    let resp_json = serde_json::to_string(&out)?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "send-to", &resp_json, now_ms)?;
    }

    Ok(out)
}

pub async fn execute_retry(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchRetryArgs,
) -> Result<RetryOutput> {
    if let Some(ref req_id) = args.request_id {
        if let Some(cached) = store.get_mutation_receipt(req_id)? {
            let out: RetryOutput = serde_json::from_str(&cached)?;
            return Ok(out);
        }
    }

    let resolved_caller = resolve_caller_terminal(
        herdr,
        args.caller_terminal,
        None,
        None,
        true,
        "caller terminal identity could not be established for attempt fencing; pass --caller <term> or run inside Herdr ($HC_TERMINAL_ID)",
    )
    .await?;

    let old_disp = store.verify_parent_caller(
        &args.dispatch_id,
        &resolved_caller.terminal,
        resolved_caller.created_ms,
    )?;

    if matches!(
        old_disp.status,
        DispatchStatus::Completed | DispatchStatus::Superseded
    ) {
        bail!(
            "dispatch '{}' has status '{}' and cannot be retried",
            old_disp.id,
            old_disp.status
        );
    }

    if old_disp.agent_kind == "attached" {
        bail!(
            "cannot retry attached dispatch '{}' (no agent command recorded); use `tchelvis orch dispatch --agent <agent>` instead",
            old_disp.id
        );
    }

    let task = store
        .get_task(&old_disp.run_id, &old_disp.task_id)?
        .with_context(|| {
            format!(
                "task '{}' not found in run '{}'",
                old_disp.task_id, old_disp.run_id
            )
        })?;
    if task.failure_count >= MAX_DISPATCH_FAILURES || task.status == TaskStatus::CircuitBroken {
        bail!(
            "circuit breaker tripped for task '{}' in run '{}' (failure_count = {})",
            task.id,
            task.run_id,
            task.failure_count
        );
    }

    close_worker_terminal(herdr, &old_disp.assignee_terminal).await;

    let dispatch_output = execute_dispatch(
        store,
        herdr,
        sleeper,
        OrchDispatchArgs {
            run: old_disp.run_id.clone(),
            task: old_disp.task_id.clone(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some(old_disp.agent_kind.clone()),
            worktree: old_disp.worktree_mode.clone(),
            request_id: None,
            caller_terminal: Some(resolved_caller.terminal),
        },
    )
    .await?;

    let superseded = store.supersede_dispatch(&old_disp.id, sleeper.now_ms())?;

    let out = RetryOutput {
        previous_dispatch_id: superseded.id,
        previous_status: superseded.status,
        dispatch_output,
    };

    let resp_json = serde_json::to_string(&out)?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "retry", &resp_json, sleeper.now_ms())?;
    }

    Ok(out)
}

pub async fn execute_stop(
    store: &OrchStore,
    herdr: &HerdrClient,
    sleeper: &dyn OrchSleeper,
    args: OrchStopArgs,
) -> Result<StopOutput> {
    if let Some(ref req_id) = args.request_id {
        if let Some(cached) = store.get_mutation_receipt(req_id)? {
            let out: StopOutput = serde_json::from_str(&cached)?;
            return Ok(out);
        }
    }

    let resolved_caller = resolve_caller_terminal(
        herdr,
        args.caller_terminal,
        None,
        None,
        true,
        "caller terminal identity could not be established for attempt fencing; pass --caller <term> or run inside Herdr ($HC_TERMINAL_ID)",
    )
    .await?;

    let disp = store.verify_parent_caller(
        &args.dispatch_id,
        &resolved_caller.terminal,
        resolved_caller.created_ms,
    )?;

    if matches!(
        disp.status,
        DispatchStatus::Completed | DispatchStatus::Superseded
    ) {
        bail!(
            "dispatch '{}' has status '{}' and cannot be stopped",
            disp.id,
            disp.status
        );
    }

    close_worker_terminal(herdr, &disp.assignee_terminal).await;

    let now_ms = sleeper.now_ms();
    let (worktree_kept, worktree_note) = if disp.worktree_mode == "new-child" {
        if let Some(ref wt_path) = disp.worktree_path {
            match cleanup_failed_child_worktree(wt_path, &disp.run_id, &disp.task_id).await {
                Ok(cleaned) => {
                    store.clear_dispatch_worktree_path(&disp.id, now_ms)?;
                    (false, Some(cleaned))
                }
                Err(kept) => (true, Some(kept)),
            }
        } else {
            (false, None)
        }
    } else {
        (false, None)
    };

    let (stopped_disp, task_status) = store.stop_dispatch(&disp.id, now_ms)?;

    let out = StopOutput {
        dispatch_id: stopped_disp.id,
        run_id: stopped_disp.run_id,
        task_id: stopped_disp.task_id,
        assignee_terminal: stopped_disp.assignee_terminal,
        dispatch_status: stopped_disp.status,
        task_status,
        terminal_closed: true,
        worktree_kept,
        worktree_path: stopped_disp.worktree_path,
        worktree_note,
    };

    let resp_json = serde_json::to_string(&out)?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "stop", &resp_json, now_ms)?;
    }

    Ok(out)
}

fn print_run_create(run: &OrchRun, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(run)?);
    } else {
        println!(
            "Created orchestration run {} (coordinator={}, title=\"{}\")",
            run.id, run.coordinator_terminal, run.title
        );
    }
    Ok(())
}

fn print_launch(out: &LaunchOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(out)?);
    } else {
        println!(
            "Launched dispatch {} for task {} in run {} -> worker {} (agent={}, worktree={}, status={}, depth={}/{})",
            out.dispatch.id,
            out.task.id,
            out.run.id,
            out.dispatch.assignee_terminal,
            out.dispatch.agent_kind,
            out.dispatch.worktree_mode,
            out.dispatch.status,
            out.dispatch.depth,
            MAX_ORCH_DEPTH
        );
    }
    Ok(())
}

fn print_task_create(task: &OrchTask, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(task)?);
    } else {
        println!(
            "Task {} in run {} is {} (deps={:?})",
            task.id, task.run_id, task.status, task.deps
        );
    }
    Ok(())
}

fn print_dispatch(out: &DispatchOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(out)?);
    } else if let Some(ref reason) = out.failure_reason {
        let exit_part = out
            .exit_code
            .map(|c| format!(", exit_code={c}"))
            .unwrap_or_default();
        println!(
            "Dispatch {} for task {} in run {} -> worker {} failed at launch (status={}{}, depth={}/{}): {}",
            out.dispatch.id,
            out.dispatch.task_id,
            out.dispatch.run_id,
            out.dispatch.assignee_terminal,
            out.dispatch.status,
            exit_part,
            out.dispatch.depth,
            MAX_ORCH_DEPTH,
            reason
        );
        if let Some(ref wt_note) = out.worktree_cleanup {
            println!("Worktree cleanup: {wt_note}");
        }
    } else {
        println!(
            "Dispatched {} for task {} in run {} -> worker {} (status={}, depth={}/{})",
            out.dispatch.id,
            out.dispatch.task_id,
            out.dispatch.run_id,
            out.dispatch.assignee_terminal,
            out.dispatch.status,
            out.dispatch.depth,
            MAX_ORCH_DEPTH
        );
    }
    Ok(())
}

fn print_message(msg: &OrchMessage, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(msg)?);
    } else {
        println!(
            "Recorded {} message {} -> {} (pointer_state={})",
            msg.msg_type, msg.id, msg.target_terminal, msg.pointer_state
        );
    }
    Ok(())
}

fn print_send_to(out: &SendToOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(out)?);
    } else if out.delivered_immediately {
        println!(
            "Delivered directive {} -> worker {} immediately (dispatch={}, status={}, pointer_state={})",
            out.message.id,
            out.message.target_terminal,
            out.dispatch_id,
            out.dispatch_status,
            out.message.pointer_state
        );
    } else {
        println!(
            "Queued directive {} -> worker {} for pointer delivery (dispatch={}, status={}, pointer_state={})",
            out.message.id,
            out.message.target_terminal,
            out.dispatch_id,
            out.dispatch_status,
            out.message.pointer_state
        );
    }
    Ok(())
}

fn print_retry(out: &RetryOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(out)?);
    } else {
        println!(
            "Superseded dispatch {} (status={}); launching retry:",
            out.previous_dispatch_id, out.previous_status
        );
        print_dispatch(&out.dispatch_output, false)?;
    }
    Ok(())
}

fn print_stop(out: &StopOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(out)?);
    } else {
        println!(
            "Stopped dispatch {} (terminal={} closed={}, task={} status={}, worktree_kept={})",
            out.dispatch_id,
            out.assignee_terminal,
            out.terminal_closed,
            out.task_id,
            out.task_status,
            out.worktree_kept
        );
        if let Some(ref note) = out.worktree_note {
            println!("Worktree: {note}");
        }
    }
    Ok(())
}

fn print_ask(q_thread: &QuestionThread, msg: &OrchMessage, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "question": q_thread,
                "message": msg,
                "notice": "Question recorded. End your turn immediately; Tchelvis will wake this terminal when the coordinator replies."
            }))?
        );
    } else {
        println!(
            "Question {} recorded for dispatch {} (message {}).",
            q_thread.id, q_thread.dispatch_id, msg.id
        );
        println!(
            "Question recorded. End your turn immediately; Tchelvis will wake this terminal when the coordinator replies."
        );
    }
    Ok(())
}

fn print_reply(q_thread: &QuestionThread, msg: &OrchMessage, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "question": q_thread,
                "message": msg,
            }))?
        );
    } else {
        println!(
            "Replied to question {} -> worker {} (message {}, pointer_state={})",
            q_thread.id,
            msg.target_terminal,
            msg.id,
            PointerState::Pending
        );
    }
    Ok(())
}

fn format_human_status(out: &OrchStatusOutput) -> String {
    use std::fmt::Write;
    let mut buf = String::new();
    for v in &out.runs {
        let _ = writeln!(
            buf,
            "Run {} ({}) [status={} coordinator={} unack_msgs={}]",
            v.run.id,
            v.run.title,
            v.run.status,
            v.run.coordinator_terminal,
            v.unacknowledged_messages
        );
        for t in &v.tasks {
            let _ = writeln!(
                buf,
                "  Task {}: {} [status={} failures={} deps={:?}]",
                t.id, t.title, t.status, t.failure_count, t.deps
            );
        }
        for d in &v.dispatches {
            let _ = writeln!(
                buf,
                "  Dispatch {}: task={} assignee={} parent={} status={} worktree={}",
                d.id, d.task_id, d.assignee_terminal, d.parent_terminal, d.status, d.worktree_mode
            );
        }
        for m in &v.messages {
            let _ = writeln!(
                buf,
                "  Message {}: [{}] {} -> {} (dispatch={}, pointer={}): {}",
                m.id,
                m.msg_type,
                m.from_handle,
                m.to_handle,
                m.dispatch_id.as_deref().unwrap_or("-"),
                m.pointer_state,
                m.body
            );
        }
        for q in &v.open_questions {
            let _ = writeln!(
                buf,
                "  Open Question {}: dispatch={} status={}",
                q.id, q.dispatch_id, q.status
            );
        }
    }
    buf
}

fn print_human_status(out: &OrchStatusOutput) {
    print!("{}", format_human_status(out));
}

fn print_inbox_check(res: &InboxCheckResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(res)?);
    } else {
        println!(
            "Orchestration Inbox for {} (run_filter={}): {} message(s)",
            res.target_terminal,
            res.run_id_filter.as_deref().unwrap_or("*"),
            res.messages.len()
        );
        for m in &res.messages {
            let outcome = m
                .outcome
                .as_deref()
                .map(|o| format!(" outcome={o}"))
                .unwrap_or_default();
            println!(
                "  - [{}] {} from {}{} (task={} dispatch={}): {}",
                m.msg_type,
                m.id,
                m.from_handle,
                outcome,
                m.task_id.as_deref().unwrap_or("-"),
                m.dispatch_id.as_deref().unwrap_or("-"),
                m.body
            );
        }
        if !res.ready_tasks.is_empty() {
            let ids: Vec<&str> = res.ready_tasks.iter().map(|t| t.id.as_str()).collect();
            println!("Ready tasks: {}", ids.join(", "));
        }
        if !res.blocked_tasks.is_empty() {
            let ids: Vec<&str> = res.blocked_tasks.iter().map(|t| t.id.as_str()).collect();
            println!("Blocked/Circuit-Broken tasks: {}", ids.join(", "));
        }
        if !res.active_dispatches.is_empty() {
            let summaries: Vec<String> = res
                .active_dispatches
                .iter()
                .map(|d| format!("{}({}:{})", d.id, d.assignee_terminal, d.status))
                .collect();
            println!("Active dispatches: {}", summaries.join(", "));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Parser)]
    struct TestOrchCli {
        #[command(subcommand)]
        cmd: OrchCommands,
    }

    #[test]
    fn test_orch_commands_parse_launch_send_to_retry_stop() {
        let launch = TestOrchCli::try_parse_from([
            "orch",
            "launch",
            "--agent",
            "claude",
            "--prompt",
            "Do work",
            "--title",
            "T",
            "--worktree",
            "new-child",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            launch.cmd,
            OrchCommands::Launch { json: true, .. }
        ));

        let send_to = TestOrchCli::try_parse_from([
            "orch",
            "send",
            "--to",
            "dispatch:disp_1",
            "--body",
            "Hello worker",
        ])
        .unwrap();
        assert!(matches!(
            send_to.cmd,
            OrchCommands::Send {
                to: Some(_),
                msg_type: None,
                ..
            }
        ));

        let send_worker = TestOrchCli::try_parse_from([
            "orch",
            "send",
            "--type",
            "worker_done",
            "--outcome",
            "succeeded",
            "--dispatch-id",
            "disp_1",
            "--body",
            "Done",
        ])
        .unwrap();
        assert!(matches!(
            send_worker.cmd,
            OrchCommands::Send {
                to: None,
                msg_type: Some(_),
                dispatch_id: Some(_),
                ..
            }
        ));

        let retry = TestOrchCli::try_parse_from(["orch", "retry", "disp_1", "--json"]).unwrap();
        assert!(matches!(retry.cmd, OrchCommands::Retry { json: true, .. }));

        let stop = TestOrchCli::try_parse_from(["orch", "stop", "disp_1"]).unwrap();
        assert!(matches!(stop.cmd, OrchCommands::Stop { json: false, .. }));
    }

    #[test]
    fn test_preflight_agent_with_path_checks_executable_bit() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        // Missing binary fails agent_preflight
        let err_missing =
            preflight_agent_with_path("codex", Some(bin_dir.as_os_str())).unwrap_err();
        assert_eq!(err_missing.failed_stage, "agent_preflight");
        assert_eq!(err_missing.agent, "codex");

        // Non-executable file fails agent_preflight
        let codex_file = bin_dir.join("codex");
        fs::write(&codex_file, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&codex_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(preflight_agent_with_path("codex", Some(bin_dir.as_os_str())).is_err());

        // Executable file passes (even for preset with flags like `codex --no-daemon`)
        fs::set_permissions(&codex_file, fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = preflight_agent_with_path("codex", Some(bin_dir.as_os_str())).unwrap();
        assert_eq!(resolved, codex_file.to_string_lossy());
    }

    #[test]
    fn test_format_human_status_includes_messages() {
        let out = OrchStatusOutput {
            runs: vec![RunStatusView {
                run: OrchRun {
                    id: "run_1".into(),
                    title: "Run 1".into(),
                    coordinator_terminal: "t1".into(),
                    coordinator_created_ms: 1_000,
                    status: RunStatus::Active,
                    created_at_ms: 1_000,
                    updated_at_ms: 1_000,
                },
                tasks: vec![],
                dispatches: vec![],
                messages: vec![OrchMessage {
                    id: "msg_1".into(),
                    run_id: "run_1".into(),
                    task_id: Some("task_a".into()),
                    dispatch_id: Some("disp_1".into()),
                    from_handle: "t1".into(),
                    to_handle: "t2".into(),
                    target_terminal: "t2".into(),
                    msg_type: MessageType::Directive,
                    outcome: None,
                    body: "Please add unit tests".into(),
                    pointer_state: PointerState::Acknowledged,
                    pointer_terminal: Some("t2".into()),
                    delivery_attempts: 1,
                    next_retry_at_ms: None,
                    delivered_at_ms: Some(1_100),
                    acknowledged_at_ms: Some(1_100),
                    created_at_ms: 1_100,
                }],
                open_questions: vec![],
                unacknowledged_messages: 0,
            }],
        };
        let rendered = format_human_status(&out);
        assert!(rendered.contains("Message msg_1: [directive] t1 -> t2"));
        assert!(rendered.contains("Please add unit tests"));
    }

    #[tokio::test]
    async fn test_resolve_caller_terminal_strict_fencing_and_non_strict_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_hc_ok = tmp.path().join("hc-ok");
        fs::write(
            &fake_hc_ok,
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"get\" ]; then\n\
               echo '{\"address\":\"t2\",\"created_ms\":2000,\"state\":\"live\"}'\n\
               exit 0\n\
             fi\n\
             exit 1\n",
        )
        .unwrap();
        fs::set_permissions(&fake_hc_ok, fs::Permissions::from_mode(0o755)).unwrap();

        let fake_hc_no_created = tmp.path().join("hc-no-created");
        fs::write(
            &fake_hc_no_created,
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"get\" ]; then\n\
               echo '{\"address\":\"t2\",\"state\":\"live\"}'\n\
               exit 0\n\
             fi\n\
             exit 1\n",
        )
        .unwrap();
        fs::set_permissions(&fake_hc_no_created, fs::Permissions::from_mode(0o755)).unwrap();

        let fake_hc_fail = tmp.path().join("hc-fail");
        fs::write(&fake_hc_fail, "#!/usr/bin/env bash\nexit 1\n").unwrap();
        fs::set_permissions(&fake_hc_fail, fs::Permissions::from_mode(0o755)).unwrap();

        let ok_client = HerdrClient::new(fake_hc_ok.to_string_lossy().to_string());
        let no_created_client = HerdrClient::new(fake_hc_no_created.to_string_lossy().to_string());
        let fail_client = HerdrClient::new(fake_hc_fail.to_string_lossy().to_string());

        // Strict mode rejects missing explicit & env regardless of fallback values
        let err_missing = resolve_caller_terminal_with_env(
            &ok_client,
            None,
            None,
            Some("t2"),
            Some(2_000),
            true,
            "missing caller",
        )
        .await
        .unwrap_err();
        assert!(err_missing.to_string().contains("missing caller"));

        // Strict mode rejects when hc omits created_ms
        let err_omitted = resolve_caller_terminal_with_env(
            &no_created_client,
            None,
            Some("t2".into()),
            None,
            Some(2_000),
            true,
            "missing caller",
        )
        .await
        .unwrap_err();
        assert!(err_omitted.to_string().contains("created_ms"));

        // Strict mode rejects when hc lookup fails
        let err_fail = resolve_caller_terminal_with_env(
            &fail_client,
            Some("t2".into()),
            None,
            None,
            Some(2_000),
            true,
            "missing caller",
        )
        .await
        .unwrap_err();
        assert!(err_fail.to_string().contains("attempt fencing"));

        // Strict mode succeeds when env or explicit provides terminal and hc returns created_ms
        let ok = resolve_caller_terminal_with_env(
            &ok_client,
            None,
            Some(" t2 ".into()),
            None,
            None,
            true,
            "missing caller",
        )
        .await
        .unwrap();
        assert_eq!(ok.terminal, "t2");
        assert_eq!(ok.created_ms, 2_000);

        // Non-strict mode uses fallback terminal and fallback created_ms when hc is unavailable
        let fb = resolve_caller_terminal_with_env(
            &fail_client,
            None,
            None,
            Some("t1"),
            Some(1_000),
            false,
            "missing caller",
        )
        .await
        .unwrap();
        assert_eq!(fb.terminal, "t1");
        assert_eq!(fb.created_ms, 1_000);
    }
}
