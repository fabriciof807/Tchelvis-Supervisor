use crate::config::{CockpitConfig, CockpitPaths};
use crate::correlate::resolve_git_workspace;
use crate::daemon::query_socket_once;
use crate::herdr::{HcTerminal, HerdrClient};
use crate::model::{IpcOp, IpcRequest};
use crate::orch::preamble::{build_dispatch_preamble, orchestration_skill_markdown};
use crate::orch::store::OrchStore;
use crate::orch::{
    generate_orch_id, DispatchContext, DispatchStatus, InboxCheckResult, MessageType, OrchMessage,
    OrchRun, OrchTask, PointerState, QuestionThread, RunStatus, TaskStatus, MAX_DISPATCH_FAILURES,
    MAX_ORCH_DEPTH,
};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
}

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
pub struct OrchAskArgs {
    pub dispatch_id: String,
    pub question: String,
    pub request_id: Option<String>,
    pub caller_terminal: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum OrchCommands {
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
        json: bool,
    },
    /// Send a worker completion, escalation, or status message to the parent coordinator
    Send {
        #[arg(long = "type")]
        msg_type: String,
        #[arg(long)]
        outcome: Option<String>,
        #[arg(long)]
        dispatch_id: String,
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        body: String,
        #[arg(long)]
        request_id: Option<String>,
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

async fn fetch_hc_terminal(herdr: &HerdrClient, terminal: &str) -> Result<HcTerminal> {
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

            let coord_term = coordinator
                .or_else(|| std::env::var("HC_TERMINAL_ID").ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .context(
                    "coordinator terminal not specified; pass --coordinator <term> or run inside Herdr ($HC_TERMINAL_ID)",
                )?;

            let now_ms = chrono::Utc::now().timestamp_millis();
            let term_info = fetch_hc_terminal(&herdr, &coord_term).await.ok();
            let coordinator_created_ms = term_info.and_then(|t| t.created_ms).unwrap_or(now_ms);

            let run_id = generate_orch_id("run");
            let run_title = title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
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
            let resp_json = serde_json::to_string(&run)?;
            if let Some(ref req_id) = request_id {
                store.save_mutation_receipt(req_id, "run-create", &resp_json, now_ms)?;
            }

            let labels = [
                ("cockpit.orch.role", "coordinator"),
                ("cockpit.orch.run", run_id.as_str()),
            ];
            let _ = apply_hc_labels(&herdr, &coord_term, &labels).await;
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
                    caller_terminal: None,
                },
            )
            .await?;
            notify_daemon_orch_wake(paths).await;
            print_dispatch(&out, json)?;
            Ok(())
        }
        OrchCommands::Send {
            msg_type,
            outcome,
            dispatch_id,
            task_id: _,
            body,
            request_id,
            json,
        } => {
            let sleeper = SystemOrchSleeper;
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
                    caller_terminal: None,
                },
            )
            .await?;
            notify_daemon_orch_wake(paths).await;
            print_message(&msg, json)?;
            Ok(())
        }
        OrchCommands::Ask {
            dispatch_id,
            question,
            request_id,
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
                    caller_terminal: None,
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
            let env_term = std::env::var("HC_TERMINAL_ID")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

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
                    open_questions,
                    unacknowledged_messages,
                });
            }

            let out = OrchStatusOutput { runs: views };
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for v in &out.runs {
                    println!(
                        "Run {} ({}) [status={} coordinator={} unack_msgs={}]",
                        v.run.id,
                        v.run.title,
                        v.run.status,
                        v.run.coordinator_terminal,
                        v.unacknowledged_messages
                    );
                    for t in &v.tasks {
                        println!(
                            "  Task {}: {} [status={} failures={} deps={:?}]",
                            t.id, t.title, t.status, t.failure_count, t.deps
                        );
                    }
                    for d in &v.dispatches {
                        println!(
                            "  Dispatch {}: task={} assignee={} parent={} status={} worktree={}",
                            d.id,
                            d.task_id,
                            d.assignee_terminal,
                            d.parent_terminal,
                            d.status,
                            d.worktree_mode
                        );
                    }
                    for q in &v.open_questions {
                        println!(
                            "  Open Question {}: dispatch={} status={}",
                            q.id, q.dispatch_id, q.status
                        );
                    }
                }
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

            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                Command::new(herdr.hc_bin())
                    .args(["kill", &disp.assignee_terminal])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            )
            .await;
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                Command::new(herdr.hc_bin())
                    .args(["rm", &disp.assignee_terminal])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            )
            .await;

            let now_ms = chrono::Utc::now().timestamp_millis();
            let final_status = if matches!(
                disp.status,
                DispatchStatus::Completed | DispatchStatus::Failed | DispatchStatus::Stopped
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

    let caller_terminal = args
        .caller_terminal
        .or_else(|| std::env::var("HC_TERMINAL_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| run_row.coordinator_terminal.clone());

    let caller_term_info = fetch_hc_terminal(herdr, &caller_terminal).await.ok();
    let caller_created_ms = caller_term_info
        .as_ref()
        .and_then(|t| t.created_ms)
        .unwrap_or(run_row.coordinator_created_ms);

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
    let (agent_kind_str, mut worker_term) = if let Some(ref existing_term) = args.terminal {
        let fetched = fetch_hc_terminal(herdr, existing_term).await?;
        let _ = apply_hc_labels(herdr, existing_term, &labels).await;
        ("attached".to_string(), fetched)
    } else {
        let raw_agent = args.agent.unwrap_or_else(|| "claude".to_string());
        let agent_cmd = map_agent_preset(&raw_agent);
        let spawned = herdr
            .run_worker(
                &caller_terminal,
                &dispatch_cwd,
                &labels,
                &agent_cmd,
                &preamble,
            )
            .await?;
        (raw_agent, spawned)
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
        worktree_mode: args.worktree,
        worktree_path: worktree_path_opt,
        depth,
        status: DispatchStatus::Dispatched,
        idle_since_ms: None,
        blocked_since_ms: None,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };

    store.create_dispatch(&disp)?;

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

    let (failure_reason, exit_code, output_tail) = match failure_info {
        Some((reason, code, tail)) => {
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
            (Some(reason), code, tail)
        }
        None => (None, None, None),
    };

    let out = DispatchOutput {
        dispatch: disp,
        failure_reason,
        exit_code,
        output_tail,
    };

    let resp_json = serde_json::to_string(&out)?;
    if let Some(ref req_id) = args.request_id {
        store.save_mutation_receipt(req_id, "dispatch", &resp_json, sleeper.now_ms())?;
    }

    Ok(out)
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

    let disp = wait_for_dispatch(
        store,
        &args.dispatch_id,
        DISPATCH_WAIT_TIMEOUT,
        DISPATCH_WAIT_POLL_INTERVAL,
        sleeper,
    )
    .await?;

    let caller_terminal = args
        .caller_terminal
        .or_else(|| std::env::var("HC_TERMINAL_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| disp.assignee_terminal.clone());

    let caller_created_ms = fetch_hc_terminal(herdr, &caller_terminal)
        .await
        .ok()
        .and_then(|t| t.created_ms)
        .unwrap_or(disp.terminal_created_ms);

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

    let disp = wait_for_dispatch(
        store,
        &args.dispatch_id,
        DISPATCH_WAIT_TIMEOUT,
        DISPATCH_WAIT_POLL_INTERVAL,
        sleeper,
    )
    .await?;

    let caller_terminal = args
        .caller_terminal
        .or_else(|| std::env::var("HC_TERMINAL_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| disp.assignee_terminal.clone());

    let caller_created_ms = fetch_hc_terminal(herdr, &caller_terminal)
        .await
        .ok()
        .and_then(|t| t.created_ms)
        .unwrap_or(disp.terminal_created_ms);

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
