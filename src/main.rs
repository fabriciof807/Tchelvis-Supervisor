use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use tchelvis::config::{CockpitConfig, CockpitPaths};
use tchelvis::daemon::{collect_once, query_socket_once, run_daemon};
use tchelvis::doctor::run_doctor;
use tchelvis::herdr::HerdrClient;
use tchelvis::home::{self, unix_now_secs, DailyTaskStore, HomeSnapshot, TaskPriority};
use tchelvis::logging::{init_logging, read_filtered_logs};
use tchelvis::model::{
    AgentRow, AttentionState, CockpitSnapshot, InspectResponse, IpcOp, IpcRequest,
};
use tchelvis::providers::ProviderCoordinator;
use tchelvis::setup::{run_setup_with, run_uninstall, SetupOptions};
use tchelvis::store::CacheStore;
use tchelvis::supervisor::state::SupervisorMode;
use tchelvis::supervisor::{
    approve_action, current_status, ignore_action, load_model, run_supervisor_daemon,
    run_tick_once, run_tui, set_gpu, set_or_cycle_mode, unload_model,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

#[derive(Debug, Parser)]
#[command(
    name = "tchelvis",
    version = tchelvis::version::long_version(),
    about = "Local-first, read-only dashboard and CLI for tracking coding agents running through Herdr (hc)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run the single-instance background service or a single collection cycle
    Daemon {
        /// Execute one snapshot cycle and exit
        #[arg(long)]
        once: bool,
        /// Custom Unix domain socket path
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Show live and recent coding agent fleet status
    Status {
        /// Output full CockpitSnapshot JSON
        #[arg(long)]
        json: bool,
        /// Force immediate external provider refresh
        #[arg(long)]
        refresh: bool,
        /// Include plain non-agent shells
        #[arg(long)]
        all: bool,
        /// Do not auto-spawn background daemon if socket is absent
        #[arg(long)]
        no_spawn: bool,
    },
    /// Stream live fleet updates from the Cockpit daemon
    Watch {
        /// Output newline-delimited CockpitSnapshot JSON lines
        #[arg(long)]
        json: bool,
    },
    /// Inspect correlation provenance, PR/CI/review/deploy details, tickets, Slack, and runbooks for one terminal
    Inspect {
        /// Terminal address (e.g., t7) or name
        terminal: String,
        /// Output InspectResponse JSON
        #[arg(long)]
        json: bool,
    },
    /// Attach, remove (`k=`), or view `cockpit.*` provenance labels on a Herdr terminal
    Label {
        /// Terminal address or name
        terminal: String,
        /// Key=value pairs (e.g., cockpit.issue=ENG-42 or issue=ENG-42; `k=` removes key)
        pairs: Vec<String>,
    },
    /// Verify hc runtime, hook hashes, Herdr mod schema, SQLite store, and provider health
    Doctor {
        /// Output DoctorReport JSON
        #[arg(long)]
        json: bool,
        /// Write a secret-redacted diagnostic bundle to the specified path
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    /// Configure shell helpers, coding agent hooks, and Herdr Desktop mod assets
    Setup {
        /// Preview planned file changes without modifying disk
        #[arg(long)]
        dry_run: bool,
        /// Comma-separated targets: shell,claude,opencode,pi,herdr-mod,skills,systemd
        #[arg(long)]
        only: Option<String>,
        /// Comma-separated targets to skip
        #[arg(long)]
        skip: Option<String>,
        /// Verify installed asset SHA-256 hashes without modifying files
        #[arg(long)]
        verify: bool,
        /// Roll back all Cockpit-managed changes using setup-manifest.json
        #[arg(long)]
        undo: bool,
        /// Set `tchelvis` as the default layout in herdr-app/settings.json
        #[arg(long)]
        set_default_layout: bool,
        /// Write ~/.config/systemd/user/tchelvis.service
        #[arg(long)]
        systemd: bool,
        /// When used with --undo, also purge Cockpit directories and remove the tchelvis binary
        #[arg(long)]
        purge: bool,
    },
    /// Remove all Cockpit-managed hooks, fenced shell blocks, mod files, and manifest state
    Uninstall {
        /// Preview planned rollback actions without modifying disk
        #[arg(long)]
        dry_run: bool,
        /// Proceed even if warnings occur
        #[arg(long)]
        force: bool,
        /// Also delete ~/.config/tchelvis, ~/.local/share/tchelvis, ~/.local/state/tchelvis, runtime sockets, and the tchelvis binary
        #[arg(long)]
        purge: bool,
    },
    /// Update tchelvis from GitHub Releases
    Update {
        /// Release channel (stable or nightly)
        #[arg(long)]
        channel: Option<String>,
        /// Check for updates without installing (exits 0 if available, 1 if up to date)
        #[arg(long)]
        check: bool,
        /// Output JSON (with --check)
        #[arg(long)]
        json: bool,
        /// Skip running `tchelvis setup --set-default-layout` after replacing the binary
        #[arg(long)]
        no_setup: bool,
        /// Reinstall even if already on the latest version/commit
        #[arg(long)]
        force: bool,
    },
    /// View redacted structured JSONL logs
    Logs {
        /// Filter by terminal/agent ID
        #[arg(long)]
        agent: Option<String>,
        /// Filter by provider name
        #[arg(long)]
        provider: Option<String>,
        /// Maximum number of log lines to display
        #[arg(long, default_value_t = 50)]
        lines: usize,
    },
    /// Launch the interactive Ratatui Cockpit & Durable Supervisor TUI dashboard
    Tui,
    /// Show Projects in Development and Daily Tasks summary
    Home {
        /// Output full HomeSnapshot JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage Daily Tasks
    Task {
        #[command(subcommand)]
        action: TaskCommand,
    },
    /// Manage the Pi-Durable FSM Supervisor (tick, status, approve, ignore, mode, unload, watch)
    Supervisor {
        #[command(subcommand)]
        action: SupervisorCommands,
    },
    /// Show AI account usage limits and quotas via ai-usagebar
    Usage {
        /// Output UsageBarSnapshot JSON
        #[arg(long)]
        json: bool,
        /// Auto-detect local provider credentials before fetching (`ai-usagebar detect`)
        #[arg(long)]
        detect: bool,
    },
    /// Ask read-only questions about active agents and Daily Tasks
    Chat {
        #[command(subcommand)]
        action: ChatCommand,
    },
    /// Multi-agent parent-child orchestration and mailbox commands
    Orch {
        #[command(subcommand)]
        action: tchelvis::orch::cli::OrchCommands,
    },
    /// Execute and manage declarative multi-agent loops in sandbox
    Loop {
        #[command(subcommand)]
        action: tchelvis::loops::cli::LoopCommands,
    },
}

#[derive(Debug, Subcommand)]
enum ChatCommand {
    /// Ask a single read-only question using the supervisor's effective model
    Ask {
        /// Output JSON (requires --request)
        #[arg(long)]
        json: bool,
        /// JSON request payload {"question":"...","history":[...]}
        #[arg(long)]
        request: Option<String>,
        /// Question text (when --request is not used)
        question: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// List all daily tasks
    List {
        /// Output full HomeSnapshot JSON
        #[arg(long)]
        json: bool,
    },
    /// Add a new daily task
    Add {
        /// Optional project name to associate with this task
        #[arg(long)]
        project: Option<String>,
        /// Task priority (high, normal, low)
        #[arg(long, default_value = "normal")]
        priority: String,
        /// Output full HomeSnapshot JSON
        #[arg(long)]
        json: bool,
        /// Open $VISUAL / $EDITOR to compose a multi-line task title
        #[arg(long)]
        edit: bool,
        /// Task title
        #[arg(required_unless_present = "edit")]
        title: Vec<String>,
    },
    /// Toggle a daily task's completed state
    Toggle {
        /// Task ID (e.g. t-1)
        id: String,
        /// Output full HomeSnapshot JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove a daily task by ID
    Remove {
        /// Task ID (e.g. t-1)
        id: String,
        /// Output full HomeSnapshot JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove all completed daily tasks
    ClearDone {
        /// Output full HomeSnapshot JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SupervisorCommands {
    /// Return the current DesktopModSnapshot JSON
    Status {
        /// Output JSON (always emits DesktopModSnapshot JSON)
        #[arg(long)]
        json: bool,
    },
    /// Execute one deterministic supervisor evaluation tick
    Tick {
        /// Output JSON (always emits OnceOutput JSON)
        #[arg(long)]
        json: bool,
    },
    /// Run the supervisor watcher loop and persist supervisor_mod_snapshot.json
    Watch,
    /// Approve a pending operator Action Card and send payload to the agent
    Approve {
        /// Agent / terminal ID
        agent_id: String,
        /// Optional command payload override
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        payload: Vec<String>,
    },
    /// Ignore/dismiss a pending operator Action Card
    Ignore {
        /// Agent / terminal ID
        agent_id: String,
    },
    /// Send a deterministic Enter/Esc response to a blocked terminal's pending prompt
    Respond {
        /// Terminal / agent ID
        terminal: String,
        /// Action: approve or deny
        action: String,
    },
    /// Set or cycle the supervisor operating mode (semi-auto, alert-only, paused, cycle)
    Mode {
        /// Target mode or "cycle"
        mode: String,
    },
    /// Start/load the managed LLM container (or warm the Ollama model) and wait until healthy
    Load,
    /// Stop/unload the managed LLM container (or unload the Ollama model with keep_alive: 0)
    Unload,
    /// Toggle GPU offload for the local LLM (`off` = CPU-only num_gpu 0, default; `on` = offload all layers)
    Gpu {
        /// `on` or `off`
        state: String,
        /// Skip warm-loading the model after the toggle
        #[arg(long)]
        no_warm: bool,
    },
    /// List, select, filter, or reset the managed llama.cpp container model
    Model {
        #[command(subcommand)]
        action: ModelCommand,
    },
    /// Configure and control the API fallback model for container failures
    Fallback {
        #[command(subcommand)]
        action: FallbackCommand,
    },
    /// Benchmark CPU vs GPU speed on the supervisor's evaluation request
    Bench {
        /// Number of measured runs per mode (default 3)
        #[arg(long)]
        runs: Option<u32>,
        /// Output JSON
        #[arg(long)]
        json: bool,
        /// Print the last benchmark result without running
        #[arg(long)]
        last: bool,
        /// Print the current benchmark progress JSON (or `{}`)
        #[arg(long)]
        progress: bool,
    },
    /// Show the durable supervisor activity log (newest first)
    Log {
        /// Output JSON array
        #[arg(long)]
        json: bool,
        /// Maximum number of events to return
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Filter by event kind (comma-separated or repeated: auto_reply, auto_reply_blocked, alert, evaluation, eval_error, mode_change, llm_load, llm_unload, gpu_toggle, respond)
        #[arg(long)]
        kind: Vec<String>,
        /// Filter by agent / terminal ID
        #[arg(long)]
        agent: Option<String>,
    },
    /// Inspect effective [guardrails] rules or dry-run a command/screen against them
    Rules {
        /// Output JSON
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: Option<RulesCommand>,
    },
}

#[derive(Debug, Subcommand)]
enum RulesCommand {
    /// Dry-run fixed guardrail rules against a command or captured screen (never sends anything)
    Test {
        /// Command to test (mutually exclusive with --screen)
        #[arg(
            value_name = "COMMAND",
            required_unless_present = "screen",
            conflicts_with = "screen"
        )]
        command: Option<String>,
        /// Tool name when testing a command (default: Bash)
        #[arg(long, requires = "command", conflicts_with = "screen")]
        tool: Option<String>,
        /// Path to captured terminal screen file (mutually exclusive with <COMMAND>)
        #[arg(long, value_name = "FILE", conflicts_with = "command")]
        screen: Option<PathBuf>,
        /// Simulated hc status message (only with --screen)
        #[arg(long, requires = "screen", conflicts_with = "command")]
        msg: Option<String>,
        /// Simulated hc status kind (only with --screen)
        #[arg(long, requires = "screen", conflicts_with = "command")]
        kind: Option<String>,
        /// Output RuleTestReport JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum FallbackCommand {
    /// Show fallback configuration and runtime state
    Status {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Set fallback endpoint host and/or model name
    Set {
        /// OpenAI-compatible base URL (e.g. https://openrouter.ai/api)
        #[arg(long)]
        host: Option<String>,
        /// Model identifier (e.g. openai/gpt-4.1-mini)
        #[arg(long)]
        model: Option<String>,
    },
    /// Save the fallback API key from stdin to ~/.config/tchelvis/fallback_api_key (0600)
    Key,
    /// List models from the configured fallback endpoint (/v1/models)
    Models {
        /// Output JSON
        #[arg(long)]
        json: bool,
        /// Case-insensitive substring filter
        #[arg(long)]
        filter: Option<String>,
    },
    /// Try to return to the primary local model (clears fallback only on success)
    Off,
    /// Remove [fallback] from supervisor.toml and deactivate fallback
    Clear,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// List local GGUF models
    List {
        /// Output JSON
        #[arg(long)]
        json: bool,
        /// Override max parameter filter (`2b|4b|8b|all`)
        #[arg(long, value_name = "2b|4b|8b|all")]
        max_params: Option<String>,
        /// Include embeddings, projectors, and models above the filter
        #[arg(long)]
        all: bool,
    },
    /// Select a model by id (`ollama:<name>`, `file:<path>`, `hf:<repo>`)
    Set {
        /// Model id
        id: String,
    },
    /// Persist the max parameter filter (`2b|4b|8b|all`)
    Filter {
        /// Filter bucket (`2b|4b|8b|all`)
        max_params: String,
    },
    /// Clear `llm_container.model_id` from supervisor.toml
    Reset,
    /// Hide a model id from `model list`
    Hide {
        /// Model id
        id: String,
        /// Return all models (including hidden/filtered) in the JSON output
        #[arg(long)]
        all: bool,
    },
    /// Unhide a previously hidden model id
    Unhide {
        /// Model id
        id: String,
        /// Return all models (including hidden/filtered) in the JSON output
        #[arg(long)]
        all: bool,
    },
    /// Delete a local model from disk or Ollama (requires confirmation)
    Rm {
        /// Model id
        id: String,
        /// Skip interactive confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Output DeleteOutcome JSON
        #[arg(long)]
        json: bool,
    },
}

const VALID_LABEL_KEYS: [&str; 12] = [
    "cockpit.issue",
    "cockpit.pr",
    "cockpit.slack",
    "cockpit.runbook",
    "cockpit.agent",
    "cockpit.supervisor_state",
    "cockpit.supervisor_decision",
    "cockpit.parent",
    "cockpit.orch.role",
    "cockpit.orch.run",
    "cockpit.orch.task",
    "cockpit.orch.dispatch",
];

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = CockpitPaths::resolve()?;
    let _ = paths.ensure_dirs();
    let _ = init_logging(&paths.log_file());
    let cfg = CockpitConfig::load_or_default(&paths)?;

    match cli.command {
        Commands::Daemon { once, socket } => {
            if once {
                let store = CacheStore::open(&paths.db_file())?;
                let herdr = HerdrClient::default();
                let coord = ProviderCoordinator::new();
                let snap = collect_once(&cfg, &paths, &store, &herdr, &coord, false, false).await?;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                return Ok(());
            }
            run_daemon(cfg, paths, socket).await
        }
        Commands::Status {
            json,
            refresh,
            all,
            no_spawn,
        } => {
            let snap = obtain_snapshot(&cfg, &paths, refresh, all, no_spawn).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                print_human_status(&snap);
            }
            Ok(())
        }
        Commands::Watch { json } => {
            let sock = paths.socket_file();
            if !sock.exists() {
                let _ = tchelvis::daemon::ensure_daemon_running(&paths).await;
            }
            if let Ok(stream) = UnixStream::connect(&sock).await {
                let (read_half, mut write_half) = stream.into_split();
                let req = IpcRequest {
                    op: IpcOp::Watch,
                    terminal: None,
                    include_all: false,
                };
                let mut line = serde_json::to_string(&req)?;
                line.push('\n');
                write_half.write_all(line.as_bytes()).await?;

                let mut reader = BufReader::new(read_half);
                let mut incoming = String::new();
                loop {
                    incoming.clear();
                    let n = reader.read_line(&mut incoming).await?;
                    if n == 0 {
                        break;
                    }
                    if json {
                        print!("{incoming}");
                    } else if let Ok(snap) =
                        serde_json::from_str::<CockpitSnapshot>(incoming.trim())
                    {
                        print!("\x1b[2J\x1b[H");
                        print_human_status(&snap);
                    }
                }
                Ok(())
            } else {
                let snap = obtain_snapshot(&cfg, &paths, false, false, true).await?;
                if json {
                    println!("{}", serde_json::to_string(&snap)?);
                } else {
                    print_human_status(&snap);
                }
                Ok(())
            }
        }
        Commands::Inspect { terminal, json } => {
            let sock = paths.socket_file();
            let req = IpcRequest {
                op: IpcOp::Inspect,
                terminal: Some(terminal.clone()),
                include_all: true,
            };
            let resp: InspectResponse = match query_socket_once(&sock, &req).await {
                Ok(r) => r,
                Err(_) => {
                    let store = CacheStore::open(&paths.db_file())?;
                    let herdr = HerdrClient::default();
                    let coord = ProviderCoordinator::new();
                    let snap =
                        collect_once(&cfg, &paths, &store, &herdr, &coord, true, false).await?;
                    let agent = snap
                        .agents
                        .into_iter()
                        .find(|a| a.address == terminal || a.name == terminal)
                        .with_context(|| format!("terminal '{}' not found", terminal))?;
                    InspectResponse {
                        agent,
                        providers: snap.providers,
                    }
                }
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_human_inspect(&resp);
            }
            Ok(())
        }
        Commands::Label { terminal, pairs } => {
            let herdr = HerdrClient::default();
            if pairs.is_empty() {
                let mut rpc = None;
                let term = herdr.get_terminal(&mut rpc, &terminal).await?;
                for (k, v) in term
                    .labels
                    .iter()
                    .filter(|(k, _)| k.starts_with("cockpit."))
                {
                    println!("{k}={v}");
                }
                return Ok(());
            }
            let mut normalized_args = vec!["label".to_string(), terminal];
            for pair in pairs {
                let Some((raw_k, v)) = pair.split_once('=') else {
                    bail!(
                        "Invalid label syntax '{}'; expected key=value or key=",
                        pair
                    );
                };
                let full_k = if raw_k.starts_with("cockpit.") {
                    raw_k.to_string()
                } else {
                    format!("cockpit.{raw_k}")
                };
                if !VALID_LABEL_KEYS.contains(&full_k.as_str()) {
                    bail!(
                        "Unsupported Cockpit label key '{}'. Valid keys: {}",
                        full_k,
                        VALID_LABEL_KEYS.join(", ")
                    );
                }
                normalized_args.push(format!("{full_k}={v}"));
            }
            let status = Command::new(herdr.hc_bin())
                .args(&normalized_args)
                .status()
                .await
                .context("failed invoking `hc label`")?;
            if !status.success() {
                bail!("`hc label` exited with {}", status);
            }
            Ok(())
        }
        Commands::Doctor { json, bundle } => {
            let rep = run_doctor(&cfg, &paths, bundle.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                print_human_doctor(&rep);
            }
            Ok(())
        }
        Commands::Setup {
            dry_run,
            only,
            skip,
            verify,
            undo,
            set_default_layout,
            systemd,
            purge,
        } => {
            let opts = SetupOptions {
                dry_run,
                only,
                skip,
                verify_only: verify,
                undo,
                set_default_layout,
                systemd,
                purge,
            };
            let rep = run_setup_with(&paths, &opts, &mut |path| {
                use std::io::IsTerminal;
                if !std::io::stdin().is_terminal() {
                    return false;
                }
                let mut err = std::io::stderr();
                let mut stdin = std::io::stdin().lock();
                confirm_replace_skill_io(path, &mut stdin, &mut err)
            })?;
            println!("Cockpit {} (dry_run={}):", rep.action, rep.dry_run);
            for line in &rep.planned_or_applied {
                println!("  - {line}");
            }
            if !rep.verified_ok {
                bail!("Setup verification reported mismatches");
            }
            Ok(())
        }
        Commands::Uninstall {
            dry_run,
            force: _,
            purge,
        } => {
            let rep = run_uninstall(&paths, dry_run, purge, None)?;
            println!("Cockpit {} (dry_run={}):", rep.action, rep.dry_run);
            for line in &rep.planned_or_applied {
                println!("  - {line}");
            }
            Ok(())
        }
        Commands::Update {
            channel,
            check,
            json,
            no_setup,
            force,
        } => {
            if json && !check {
                println!(
                    "{}",
                    serde_json::json!({ "error": "--json is only supported with --check" })
                );
                std::process::exit(1);
            }
            let parsed_channel = match channel {
                Some(ref raw) => match tchelvis::update::Channel::parse(raw) {
                    Some(ch) => Some(ch),
                    None => {
                        let msg = format!("unknown channel '{raw}' (expected stable or nightly)");
                        if json {
                            println!("{}", serde_json::json!({ "error": msg }));
                            std::process::exit(1);
                        }
                        bail!("{msg}");
                    }
                },
                None => None,
            };
            tchelvis::update::run_update(parsed_channel, check, json, no_setup, force)
        }
        Commands::Logs {
            agent,
            provider,
            lines,
        } => {
            let entries = read_filtered_logs(
                &paths.log_file(),
                agent.as_deref(),
                provider.as_deref(),
                lines,
            )?;
            for line in entries {
                println!("{line}");
            }
            Ok(())
        }
        Commands::Tui => {
            tokio::task::spawn_blocking(run_tui)
                .await
                .context("supervisor TUI task panicked")??;
            Ok(())
        }
        Commands::Supervisor { action } => match action {
            SupervisorCommands::Status { json: _ } => {
                let snap = tokio::task::spawn_blocking(current_status)
                    .await
                    .context("supervisor status task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Tick { json: _ } => {
                let out = tokio::task::spawn_blocking(run_tick_once)
                    .await
                    .context("supervisor tick task panicked")??;
                println!("{}", serde_json::to_string_pretty(&out)?);
                Ok(())
            }
            SupervisorCommands::Watch => {
                tokio::task::spawn_blocking(run_supervisor_daemon)
                    .await
                    .context("supervisor watch task panicked")??;
                Ok(())
            }
            SupervisorCommands::Approve { agent_id, payload } => {
                let payload_opt = if payload.is_empty() {
                    None
                } else {
                    Some(payload.join(" "))
                };
                let snap = tokio::task::spawn_blocking(move || {
                    approve_action(&agent_id, payload_opt.as_deref())
                })
                .await
                .context("supervisor approve task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Ignore { agent_id } => {
                let snap = tokio::task::spawn_blocking(move || ignore_action(&agent_id))
                    .await
                    .context("supervisor ignore task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Respond { terminal, action } => {
                let parsed = tchelvis::supervisor::RespondAction::parse(&action)?;
                let snap = tokio::task::spawn_blocking(move || {
                    tchelvis::supervisor::respond_pending_prompt(&terminal, parsed)
                })
                .await
                .context("supervisor respond task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Mode { mode } => {
                let parsed = match mode.to_ascii_lowercase().as_str() {
                    "semi-auto" | "semi_auto" | "semiauto" => Some(SupervisorMode::SemiAuto),
                    "alert-only" | "alert_only" | "alertonly" => Some(SupervisorMode::AlertOnly),
                    "paused" => Some(SupervisorMode::Paused),
                    "cycle" => None,
                    other => bail!(
                        "Invalid supervisor mode '{}'; expected semi-auto, alert-only, paused, or cycle",
                        other
                    ),
                };
                let snap = tokio::task::spawn_blocking(move || set_or_cycle_mode(parsed))
                    .await
                    .context("supervisor mode task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Gpu { state, no_warm } => {
                let allow = match state.to_ascii_lowercase().as_str() {
                    "on" | "true" | "1" | "gpu" => true,
                    "off" | "false" | "0" | "cpu" => false,
                    other => bail!("Invalid gpu state '{}'; expected on or off", other),
                };
                let snap = tokio::task::spawn_blocking(move || set_gpu(allow, !no_warm))
                    .await
                    .context("supervisor gpu task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Load => {
                let snap = tokio::task::spawn_blocking(load_model)
                    .await
                    .context("supervisor load task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Unload => {
                let snap = tokio::task::spawn_blocking(unload_model)
                    .await
                    .context("supervisor unload task panicked")??;
                println!("{}", serde_json::to_string_pretty(&snap)?);
                Ok(())
            }
            SupervisorCommands::Model { action } => match action {
                ModelCommand::List {
                    json,
                    max_params,
                    all,
                } => {
                    let parsed_filter = match max_params {
                        Some(raw) => Some(
                            tchelvis::supervisor::models::MaxParams::parse(&raw).ok_or_else(
                                || {
                                    anyhow::anyhow!(
                                        "Invalid --max-params '{}'; expected 2b, 4b, 8b, or all",
                                        raw
                                    )
                                },
                            )?,
                        ),
                        None => None,
                    };
                    let out = tokio::task::spawn_blocking(move || {
                        tchelvis::supervisor::list_models(parsed_filter, all)
                    })
                    .await
                    .context("supervisor model list task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else {
                        print_model_table(&out);
                    }
                    Ok(())
                }
                ModelCommand::Set { id } => {
                    let snap =
                        tokio::task::spawn_blocking(move || tchelvis::supervisor::set_model(&id))
                            .await
                            .context("supervisor model set task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&snap)?);
                    Ok(())
                }
                ModelCommand::Filter { max_params } => {
                    let filter = tchelvis::supervisor::models::MaxParams::parse(&max_params)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Invalid filter '{}'; expected 2b, 4b, 8b, or all",
                                max_params
                            )
                        })?;
                    let out = tokio::task::spawn_blocking(move || {
                        tchelvis::supervisor::set_model_filter(filter)
                    })
                    .await
                    .context("supervisor model filter task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&out)?);
                    Ok(())
                }
                ModelCommand::Reset => {
                    let snap = tokio::task::spawn_blocking(tchelvis::supervisor::reset_model)
                        .await
                        .context("supervisor model reset task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&snap)?);
                    Ok(())
                }
                ModelCommand::Hide { id, all } => {
                    let out = tokio::task::spawn_blocking(move || {
                        let out = tchelvis::supervisor::hide_model(&id)?;
                        if all {
                            tchelvis::supervisor::list_models(None, true)
                        } else {
                            Ok(out)
                        }
                    })
                    .await
                    .context("supervisor model hide task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&out)?);
                    Ok(())
                }
                ModelCommand::Unhide { id, all } => {
                    let out = tokio::task::spawn_blocking(move || {
                        let out = tchelvis::supervisor::unhide_model(&id)?;
                        if all {
                            tchelvis::supervisor::list_models(None, true)
                        } else {
                            Ok(out)
                        }
                    })
                    .await
                    .context("supervisor model unhide task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&out)?);
                    Ok(())
                }
                ModelCommand::Rm { id, yes, json } => {
                    use std::io::{IsTerminal, Write};
                    if !yes && !std::io::stdin().is_terminal() {
                        bail!("refusing to delete without --yes (stdin is not a terminal)");
                    }
                    let outcome = tokio::task::spawn_blocking(move || {
                        tchelvis::supervisor::delete_model(&id, &mut |model, plan| {
                            if yes {
                                return true;
                            }
                            eprintln!("Source: {}", model.source);
                            match plan {
                                tchelvis::supervisor::model_delete::DeletePlan::Files {
                                    remove,
                                    prune_dir,
                                    bytes,
                                } => {
                                    for p in remove {
                                        eprintln!("  Path: {}", p.display());
                                    }
                                    if let Some(dir) = prune_dir {
                                        eprintln!("  Prune dir: {}", dir.display());
                                    }
                                    eprintln!("Total size: {}", format_bytes(*bytes));
                                }
                                tchelvis::supervisor::model_delete::DeletePlan::Ollama { name } => {
                                    eprintln!("  Ollama model: {name}");
                                    eprintln!("  Path: {}", model.path.display());
                                    eprintln!("Total size: {}", format_bytes(model.size_bytes));
                                    eprintln!("This also removes it from Ollama for every app.");
                                }
                            }
                            eprint!("Delete? [y/N]: ");
                            let _ = std::io::stderr().flush();
                            let mut input = String::new();
                            if std::io::stdin().read_line(&mut input).is_err() {
                                return false;
                            }
                            matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                        })
                    })
                    .await
                    .context("supervisor model rm task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&outcome)?);
                    } else if outcome.deleted {
                        println!("Deleted {}", outcome.id);
                    } else {
                        println!("Aborted; {} was not deleted.", outcome.id);
                    }
                    Ok(())
                }
            },
            SupervisorCommands::Fallback { action } => match action {
                FallbackCommand::Status { json } => {
                    let info = tokio::task::spawn_blocking(tchelvis::supervisor::fallback_status)
                        .await
                        .context("supervisor fallback status task panicked")?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&info)?);
                    } else {
                        let source = tchelvis::supervisor::fallback_key_source(&info.host)
                            .unwrap_or("nenhuma");
                        println!("configured: {}", info.configured);
                        println!("active: {}", info.active);
                        if let Some(reason) = info.reason {
                            let r = match reason {
                                tchelvis::supervisor::fallback::FallbackReason::LoadFailed => {
                                    "load_failed"
                                }
                                tchelvis::supervisor::fallback::FallbackReason::Health => "health",
                            };
                            println!("reason: {r}");
                        }
                        if let Some(detail) = &info.detail {
                            println!("detail: {detail}");
                        }
                        println!(
                            "host: {}",
                            if info.host.is_empty() {
                                "(não definido)"
                            } else {
                                &info.host
                            }
                        );
                        println!(
                            "model: {}",
                            if info.model.is_empty() {
                                "(não definido)"
                            } else {
                                &info.model
                            }
                        );
                        println!("key_source: {source}");
                    }
                    Ok(())
                }
                FallbackCommand::Set { host, model } => {
                    if host.is_none() && model.is_none() {
                        bail!("informe --host e/ou --model");
                    }
                    let info = tokio::task::spawn_blocking(move || {
                        tchelvis::supervisor::fallback_set(host.as_deref(), model.as_deref())
                    })
                    .await
                    .context("supervisor fallback set task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                    Ok(())
                }
                FallbackCommand::Key => {
                    use std::io::{IsTerminal, Read, Write};
                    let mut buf = String::new();
                    if std::io::stdin().is_terminal() {
                        eprint!("Cole a chave da API de fallback e pressione Enter: ");
                        let _ = std::io::stderr().flush();
                        std::io::stdin()
                            .read_line(&mut buf)
                            .context("Falha ao ler chave de stdin")?;
                    } else {
                        std::io::stdin()
                            .read_to_string(&mut buf)
                            .context("Falha ao ler chave de stdin")?;
                    }
                    if buf.trim().is_empty() {
                        bail!("nenhuma chave fornecida em stdin");
                    }
                    tokio::task::spawn_blocking(move || {
                        tchelvis::supervisor::fallback_key_from_reader(std::io::Cursor::new(buf))
                    })
                    .await
                    .context("supervisor fallback key task panicked")??;
                    println!(
                        "Chave salva em {} (0600)",
                        tchelvis::supervisor::fallback::fallback_key_path().display()
                    );
                    Ok(())
                }
                FallbackCommand::Models { json, filter } => {
                    let models = tokio::task::spawn_blocking(move || {
                        tchelvis::supervisor::fallback_models(filter.as_deref())
                    })
                    .await
                    .context("supervisor fallback models task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&models)?);
                    } else if models.is_empty() {
                        println!("(nenhum modelo encontrado)");
                    } else {
                        for id in models {
                            println!("{id}");
                        }
                    }
                    Ok(())
                }
                FallbackCommand::Off => {
                    let snap = tokio::task::spawn_blocking(tchelvis::supervisor::fallback_off)
                        .await
                        .context("supervisor fallback off task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&snap)?);
                    Ok(())
                }
                FallbackCommand::Clear => {
                    let info = tokio::task::spawn_blocking(tchelvis::supervisor::fallback_clear)
                        .await
                        .context("supervisor fallback clear task panicked")??;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                    Ok(())
                }
            },
            SupervisorCommands::Bench {
                runs,
                json,
                last,
                progress,
            } => {
                if progress {
                    println!("{}", tchelvis::supervisor::bench::load_bench_progress());
                    return Ok(());
                }
                if last {
                    let res = tchelvis::supervisor::bench::load_last_bench()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&res)?);
                    } else {
                        println!("{}", tchelvis::supervisor::bench::format_bench_table(&res));
                    }
                    return Ok(());
                }
                let runs = runs.unwrap_or(3).max(1);
                let res = tokio::task::spawn_blocking(move || {
                    tchelvis::supervisor::bench::run_bench(runs, |p| {
                        eprintln!("[bench] {} {} ({}/{})", p.phase, p.mode, p.run, p.runs);
                    })
                })
                .await
                .context("supervisor bench task panicked")??;
                if json {
                    println!("{}", serde_json::to_string_pretty(&res)?);
                } else {
                    println!("{}", tchelvis::supervisor::bench::format_bench_table(&res));
                }
                Ok(())
            }
            SupervisorCommands::Log {
                json,
                limit,
                kind,
                agent,
            } => {
                let mut kinds = Vec::new();
                for item in kind {
                    for part in item.split(',') {
                        let trimmed = part.trim();
                        if !trimmed.is_empty() {
                            kinds.push(tchelvis::supervisor::events::SupervisorEventKind::parse(
                                trimmed,
                            )?);
                        }
                    }
                }
                let query = tchelvis::supervisor::events::EventLogQuery {
                    limit,
                    kinds,
                    agent_id: agent,
                };
                let events = tokio::task::spawn_blocking(move || {
                    tchelvis::supervisor::read_supervisor_log(&query)
                })
                .await
                .context("supervisor log task panicked")??;
                if json {
                    println!("{}", serde_json::to_string_pretty(&events)?);
                } else if events.is_empty() {
                    println!("(nenhum evento registrado)");
                } else {
                    for ev in &events {
                        println!("{}", tchelvis::supervisor::events::format_event_line(ev));
                    }
                }
                Ok(())
            }
            SupervisorCommands::Rules {
                json: rules_json,
                action,
            } => {
                use tchelvis::supervisor::rules::load_default_guardrail_rules;
                use tchelvis::supervisor::rules_cli::{
                    format_rules_human, format_rules_test_human, test_command, test_screen,
                    RulesReport,
                };
                match action {
                    None => {
                        let rules = load_default_guardrail_rules();
                        if rules_json {
                            let report = RulesReport::from_rules(&rules);
                            println!("{}", serde_json::to_string(&report)?);
                        } else {
                            print!("{}", format_rules_human(&rules));
                        }
                        Ok(())
                    }
                    Some(RulesCommand::Test {
                        command,
                        tool,
                        screen,
                        msg,
                        kind,
                        json: test_json,
                    }) => {
                        let use_json = rules_json || test_json;
                        let rules = load_default_guardrail_rules();
                        let report = match (command, screen) {
                            (Some(cmd), None) => test_command(&rules, &cmd, tool.as_deref()),
                            (None, Some(path)) => {
                                let screen_text = match std::fs::read_to_string(&path) {
                                    Ok(s) => s,
                                    Err(err) => {
                                        eprintln!(
                                            "Erro ao ler arquivo de tela {}: {err}",
                                            path.display()
                                        );
                                        std::process::exit(2);
                                    }
                                };
                                test_screen(&rules, &screen_text, msg.as_deref(), kind.as_deref())
                            }
                            _ => {
                                eprintln!("Informe <COMMAND> ou --screen <FILE>.");
                                std::process::exit(2);
                            }
                        };
                        if use_json {
                            println!("{}", serde_json::to_string_pretty(&report)?);
                        } else {
                            print!("{}", format_rules_test_human(&report));
                        }
                        if !report.approved {
                            std::process::exit(1);
                        }
                        Ok(())
                    }
                }
            }
        },
        Commands::Chat { action } => {
            match action {
                ChatCommand::Ask {
                    json,
                    request,
                    question,
                } => {
                    use tchelvis::supervisor::chat::{
                        parse_chat_request, run_chat, ChatDeps, ChatError, ChatRequest,
                        QUESTION_MAX_CHARS,
                    };
                    use tchelvis::supervisor::hc::HcBackend;

                    let req = match (json, request) {
                        (true, None) => {
                            let err = ChatError {
                                code: "bad_input".to_string(),
                                error: "--json requer --request".to_string(),
                            };
                            println!("{}", serde_json::to_string(&err)?);
                            std::process::exit(1);
                        }
                        (_, Some(raw)) => match parse_chat_request(&raw) {
                            Ok(r) => r,
                            Err(err) => {
                                if json {
                                    println!("{}", serde_json::to_string(&err)?);
                                } else {
                                    eprintln!("{}", err.error);
                                }
                                std::process::exit(1);
                            }
                        },
                        (false, None) => {
                            let q = question.join(" ");
                            if q.trim().is_empty() {
                                eprintln!("Pergunta vazia");
                                std::process::exit(1);
                            }
                            if q.chars().count() > QUESTION_MAX_CHARS {
                                eprintln!(
                                    "Pergunta excede o limite de {QUESTION_MAX_CHARS} caracteres"
                                );
                                std::process::exit(1);
                            }
                            ChatRequest {
                                question: q,
                                history: Vec::new(),
                            }
                        }
                    };

                    let paths_clone = paths.clone();
                    let res = tokio::task::spawn_blocking(move || -> Result<Result<_, ChatError>> {
                    let mut cfg = tchelvis::supervisor::config::SupervisorConfig::load();
                    cfg.ollama_timeout_secs = 120;
                    let container_mode = cfg.container_mode_active();
                    let ollama_roots =
                        tchelvis::supervisor::container::default_ollama_roots();
                    let primary_label =
                        tchelvis::supervisor::models::effective_model(&cfg, &ollama_roots)
                            .map(|m| m.display_name)
                            .unwrap_or_else(|| cfg.ollama_model.clone());
                    let primary = tchelvis::supervisor::ollama::OllamaClient::new(cfg.clone());
                    let fb_host = cfg.fallback.host.trim().to_string();
                    let fb = tchelvis::supervisor::fallback::LiveFallback {
                        state_path: tchelvis::supervisor::fallback::fallback_state_path(),
                        primary: cfg.clone(),
                    };
                    let hc = tchelvis::supervisor::hc::HcCliAdapter::new(cfg.hc.clone());
                    let sessions = hc.list_sessions().ok();
                    let store = tchelvis::supervisor::state::DurableStateStore::load_or_init_with_fallback(
                        &cfg.state_file_path,
                        tchelvis::supervisor::config::SupervisorConfig::legacy_state_file_path(),
                    )?;
                    let tasks = DailyTaskStore::load_or_init(&DailyTaskStore::default_path(&paths_clone))
                        .ok()
                        .map(|s| s.tasks);
                    let slot_lock = tchelvis::supervisor::slot_lock::slot_lock_path(&cfg);
                    let deps = ChatDeps {
                        container_mode,
                        sessions,
                        agents: &store.agents,
                        tasks,
                        primary: &primary,
                        primary_label,
                        fallback: Some(&fb),
                        fallback_host: fb_host,
                        slot_lock,
                        slot_wait: Duration::from_secs(60),
                    };
                    Ok(run_chat(deps, &req, &chrono::Local::now()))
                })
                .await
                .context("chat task panicked");

                    match res {
                        Ok(Ok(Ok(answer))) => {
                            if json {
                                println!("{}", serde_json::to_string(&answer)?);
                            } else {
                                println!("{}", answer.answer);
                            }
                            Ok(())
                        }
                        Ok(Ok(Err(chat_err))) => {
                            if json {
                                println!("{}", serde_json::to_string(&chat_err)?);
                            } else {
                                eprintln!("{}", chat_err.error);
                            }
                            std::process::exit(1);
                        }
                        Ok(Err(err)) | Err(err) => {
                            let msg: String = format!("{err:#}").chars().take(300).collect();
                            if json {
                                let chat_err = ChatError {
                                    code: "llm_error".to_string(),
                                    error: msg,
                                };
                                println!("{}", serde_json::to_string(&chat_err)?);
                            } else {
                                eprintln!("{msg}");
                            }
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Commands::Home { json } => {
            let paths_clone = paths.clone();
            let snap = tokio::task::spawn_blocking(move || home::build_home_snapshot(&paths_clone))
                .await
                .context("home snapshot task panicked")??;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                print_human_home(&snap);
            }
            Ok(())
        }
        Commands::Task { action } => {
            let paths_clone = paths.clone();
            let task_path = DailyTaskStore::default_path(&paths);
            match action {
                TaskCommand::List { json } => {
                    let snap = tokio::task::spawn_blocking(move || {
                        home::build_home_snapshot(&paths_clone)
                    })
                    .await
                    .context("task list task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&snap)?);
                    } else {
                        print_human_tasks(&snap);
                    }
                    Ok(())
                }
                TaskCommand::Add {
                    project,
                    priority,
                    json,
                    edit,
                    title,
                } => {
                    let initial = title.join(" ");
                    let final_title = if edit {
                        let visual = std::env::var("VISUAL").ok();
                        let editor_env = std::env::var("EDITOR").ok();
                        let nvi_available = std::env::var_os("PATH").is_some_and(|paths| {
                            use std::os::unix::fs::PermissionsExt;
                            std::env::split_paths(&paths).any(|dir| {
                                let candidate = dir.join("nvi");
                                candidate.metadata().is_ok_and(|m| {
                                    m.is_file() && (m.permissions().mode() & 0o111 != 0)
                                })
                            })
                        });
                        let editor = home::resolve_editor(
                            visual.as_deref(),
                            editor_env.as_deref(),
                            nvi_available,
                        );
                        let temp_dir = std::env::temp_dir();
                        let edited = tokio::task::spawn_blocking(move || {
                            home::edit_text_with_editor(&editor, &initial, &temp_dir)
                        })
                        .await
                        .context("task add --edit task panicked")??;
                        let Some(text) = edited else {
                            println!("Cancelado: texto vazio");
                            return Ok(());
                        };
                        text
                    } else {
                        if initial.trim().is_empty() {
                            bail!("Task title cannot be empty");
                        }
                        initial
                    };
                    let prio = TaskPriority::from_str_loose(&priority);
                    let (task, snap) = tokio::task::spawn_blocking(move || -> Result<_> {
                        let mut store = DailyTaskStore::load_or_init(&task_path)?;
                        let task = store.add_task(&final_title, project, prio, unix_now_secs());
                        store.save(&task_path)?;
                        let snap = home::build_home_snapshot(&paths_clone)?;
                        Ok((task, snap))
                    })
                    .await
                    .context("task add task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&snap)?);
                    } else {
                        let proj = task
                            .project
                            .as_deref()
                            .map(|p| format!(" [{p}]"))
                            .unwrap_or_default();
                        println!(
                            "Added {} ({}){} {}",
                            task.id,
                            task.priority.as_str(),
                            proj,
                            home::table_title(&task.title)
                        );
                    }
                    Ok(())
                }
                TaskCommand::Toggle { id, json } => {
                    let (task, snap) = tokio::task::spawn_blocking(move || -> Result<_> {
                        let mut store = DailyTaskStore::load_or_init(&task_path)?;
                        let Some(task) = store.toggle_task(&id, unix_now_secs()) else {
                            bail!("Task '{}' not found", id);
                        };
                        store.save(&task_path)?;
                        let snap = home::build_home_snapshot(&paths_clone)?;
                        Ok((task, snap))
                    })
                    .await
                    .context("task toggle task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&snap)?);
                    } else {
                        let mark = if task.done { "x" } else { " " };
                        println!("[{mark}] {} {}", task.id, task.title);
                    }
                    Ok(())
                }
                TaskCommand::Remove { id, json } => {
                    let id_clone = id.clone();
                    let snap = tokio::task::spawn_blocking(move || -> Result<_> {
                        let mut store = DailyTaskStore::load_or_init(&task_path)?;
                        if !store.remove_task(&id_clone) {
                            bail!("Task '{}' not found", id_clone);
                        }
                        store.save(&task_path)?;
                        home::build_home_snapshot(&paths_clone)
                    })
                    .await
                    .context("task remove task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&snap)?);
                    } else {
                        println!("Removed task {id}");
                    }
                    Ok(())
                }
                TaskCommand::ClearDone { json } => {
                    let (cleared, snap) = tokio::task::spawn_blocking(move || -> Result<_> {
                        let mut store = DailyTaskStore::load_or_init(&task_path)?;
                        let cleared = store.clear_done();
                        if cleared > 0 {
                            store.save(&task_path)?;
                        }
                        let snap = home::build_home_snapshot(&paths_clone)?;
                        Ok((cleared, snap))
                    })
                    .await
                    .context("task clear-done task panicked")??;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&snap)?);
                    } else {
                        println!("Cleared {cleared} completed task(s).");
                    }
                    Ok(())
                }
            }
        }
        Commands::Usage { json, detect } => {
            let snap = tokio::task::spawn_blocking(move || {
                tchelvis::usage::collect_usage_snapshot(detect)
            })
            .await
            .context("usage snapshot task panicked")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                println!("{}", tchelvis::usage::format_human_usage(&snap));
            }
            Ok(())
        }
        Commands::Orch { action } => {
            tchelvis::orch::cli::execute_orch_command(action, &cfg, &paths).await
        }
        Commands::Loop { action } => {
            let code = tchelvis::loops::cli::execute_loop_command(action, &paths).await;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

async fn obtain_snapshot(
    cfg: &CockpitConfig,
    paths: &CockpitPaths,
    refresh: bool,
    include_all: bool,
    no_spawn: bool,
) -> Result<CockpitSnapshot> {
    let sock = paths.socket_file();
    let req = IpcRequest {
        op: if refresh {
            IpcOp::Refresh
        } else {
            IpcOp::Status
        },
        terminal: None,
        include_all,
    };

    if let Ok(snap) = query_socket_once::<CockpitSnapshot>(&sock, &req).await {
        return Ok(snap);
    }

    if !no_spawn {
        let _ = tchelvis::daemon::ensure_daemon_running(paths).await;
        if let Ok(snap) = query_socket_once::<CockpitSnapshot>(&sock, &req).await {
            return Ok(snap);
        }
    }

    let store = CacheStore::open(&paths.db_file())?;
    let herdr = HerdrClient::default();
    let coord = ProviderCoordinator::new();
    collect_once(cfg, paths, &store, &herdr, &coord, include_all, refresh).await
}

fn print_human_status(snap: &CockpitSnapshot) {
    println!(
        "COCKPIT v{} | herdr={} | live={} (blocked={} working={} done={} idle={}) | recent_finished={}",
        snap.cockpit_version,
        if snap.herdr_connected { "connected" } else { "offline" },
        snap.summary.total_live,
        snap.summary.blocked,
        snap.summary.working,
        snap.summary.done,
        snap.summary.idle,
        snap.summary.recent_finished
    );
    println!(
        "{:<8} {:<14} {:<10} {:<18} {:<22} {:<12} {:<14}",
        "ADDR", "NAME", "AGENT", "STATE", "BRANCH", "TICKET", "PR / CI"
    );
    println!("{}", "-".repeat(102));

    for row in &snap.agents {
        println!("{}", format_human_status_row(row));
    }
}

fn format_human_status_row(row: &AgentRow) -> String {
    let state_str = match &row.attention_state {
        AttentionState::Blocked { kind, .. } => format!("BLOCKED({})", kind.as_str()),
        other => {
            if !row.lifecycle.is_live() {
                "finished".to_string()
            } else if matches!(
                row.attention_state,
                AttentionState::Idle | AttentionState::Done { .. }
            ) && !row.active_children.is_empty()
            {
                format!("Waiting on children ({})", row.active_children.len())
            } else {
                other.label().to_string()
            }
        }
    };
    let branch = row
        .workspace
        .git_branch
        .as_deref()
        .unwrap_or("Local / No PR");
    let ticket = row
        .context
        .issue
        .as_ref()
        .map(|i| format!("{} ({})", i.key, i.status))
        .or_else(|| row.context.candidate_issues.first().cloned())
        .unwrap_or_else(|| "-".into());
    let pr_ci = match (&row.context.pull_request, &row.context.ci) {
        (Some(pr), Some(ci)) => format!("#{} [{}]", pr.number, ci.overall_state),
        (Some(pr), None) => format!("#{} ({})", pr.number, pr.state),
        _ => "-".into(),
    };
    let name_col = if row
        .labels
        .get("cockpit.loop_run")
        .is_some_and(|v| !v.trim().is_empty())
    {
        row.name.clone()
    } else {
        truncate_col(&row.name, 14)
    };
    format!(
        "{:<8} {:<14} {:<10} {:<18} {:<22} {:<12} {:<14}",
        row.address,
        name_col,
        row.agent_kind.as_str(),
        truncate_col(&state_str, 18),
        truncate_col(branch, 22),
        truncate_col(&ticket, 12),
        truncate_col(&pr_ci, 14),
    )
}

fn print_human_inspect(resp: &InspectResponse) {
    let a = &resp.agent;
    println!("Terminal: {} ({})", a.name, a.address);
    println!("Agent Kind: {}", a.agent_kind.as_str());
    println!("Lifecycle: {:?}", a.lifecycle);
    println!("Attention State: {:?}", a.attention_state);
    if !a.blocked_children.is_empty() {
        println!("Blocked Children: {}", a.blocked_children.join(", "));
    }
    if !a.active_children.is_empty() {
        println!("Active Children: {}", a.active_children.join(", "));
    }
    println!("Workspace CWD: {}", a.workspace.cwd);
    println!(
        "Git Branch: {} (HEAD: {})",
        a.workspace.git_branch.as_deref().unwrap_or("none"),
        a.workspace.git_head_sha.as_deref().unwrap_or("none")
    );
    println!("Correlation Tier: {}", a.correlation_tier.as_str());
    if !a.provenance.is_empty() {
        println!("Provenance:");
        for edge in &a.provenance {
            println!(
                "  - [{}] {} -> {} ({})",
                edge.confidence.as_str(),
                edge.source,
                edge.target,
                edge.detail
            );
        }
    }
    if let Some(pr) = &a.context.pull_request {
        println!(
            "Pull Request: #{} {} ({}) - {}",
            pr.number, pr.title, pr.state, pr.url
        );
    } else {
        println!("Pull Request: Local / No PR");
    }
    if let Some(ci) = &a.context.ci {
        println!(
            "CI Checks: {} (passed={}, failed={}, pending={})",
            ci.overall_state, ci.passed, ci.failed, ci.pending
        );
    }
    if let Some(iss) = &a.context.issue {
        println!(
            "Ticket: {} [{}] {} ({})",
            iss.key, iss.status, iss.title, iss.url
        );
    } else if !a.context.candidate_issues.is_empty() {
        println!(
            "Candidate Ticket Keys: {}",
            a.context.candidate_issues.join(", ")
        );
    }
    if !a.context.runbooks.is_empty() {
        println!("Matched Local Runbooks:");
        for rb in &a.context.runbooks {
            println!("  - {} ({}) [{}]", rb.title, rb.path, rb.matched_reason);
        }
    }
}

fn print_human_doctor(rep: &tchelvis::doctor::DoctorReport) {
    println!(
        "Cockpit Doctor v{} — overall: {}",
        rep.cockpit_version, rep.overall_status
    );
    println!("\nSystem Checks:");
    for c in &rep.checks {
        println!("  [{:<13}] {:<14} {}", c.status, c.name, c.detail);
    }
    println!("\nProviders:");
    for (id, h) in &rep.providers {
        println!(
            "  [{:?}] {:<10} {}",
            h.status,
            id.as_str(),
            h.detail.as_deref().unwrap_or("")
        );
    }
}

fn print_human_home(snap: &HomeSnapshot) {
    println!(
        "Cockpit Home — {} projects ({} active), {} running agents ({} blocked), {} pending / {} done tasks",
        snap.summary.total_projects,
        snap.summary.active_projects,
        snap.summary.running_agents,
        snap.summary.blocked_agents,
        snap.summary.tasks_pending,
        snap.summary.tasks_done,
    );
    println!("\nProjects in Development:");
    if snap.projects.is_empty() {
        println!("  (none discovered)");
    } else {
        for p in &snap.projects {
            let branch = p.git_branch.as_deref().unwrap_or("-");
            let dirty = if p.git_dirty_count > 0 {
                format!(" *{}", p.git_dirty_count)
            } else {
                String::new()
            };
            println!(
                "  [{:<10}] {:<20} branch={}{} agents={}/{} tasks={}",
                p.status_label,
                truncate_col(&p.name, 20),
                branch,
                dirty,
                p.active_agents,
                p.blocked_agents,
                p.open_tasks,
            );
        }
    }
    println!();
    print_human_tasks(snap);
}

fn print_human_tasks(snap: &HomeSnapshot) {
    println!(
        "Daily Tasks ({} pending, {} done):",
        snap.summary.tasks_pending, snap.summary.tasks_done
    );
    if snap.tasks.is_empty() {
        println!("  (no tasks)");
        return;
    }
    for t in &snap.tasks {
        let mark = if t.done { "x" } else { " " };
        let proj = t
            .project
            .as_deref()
            .map(|p| format!(" [{p}]"))
            .unwrap_or_default();
        println!(
            "  [{}] {:<5} ({}){:<14} {}",
            mark,
            t.id,
            t.priority.as_str(),
            proj,
            home::table_title(&t.title)
        );
    }
}

fn truncate_col(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn format_size_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "-".to_string();
    }
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else {
        format!("{:.0} MiB", b / MIB)
    }
}

fn print_model_table(out: &tchelvis::supervisor::models::ModelListOutput) {
    if !out.managed {
        println!("Container mode is off or no runtime was found.");
        return;
    }
    if let Some(ref note) = out.env_override_ignored {
        println!("Note: {note} is set in the environment and ignored in container mode.");
    }
    if out.models.is_empty() {
        println!("No local GGUF models found (filter: {}).", out.filter);
        if out.user_hidden_count > 0 {
            println!("({} hidden by you)", out.user_hidden_count);
        }
        return;
    }
    println!(
        "{:<2} {:<28} {:<7} {:<15} {:<9} {:<8} ID",
        "·", "NAME", "PARAMS", "SOURCE", "SIZE", "QUANT"
    );
    for m in &out.models {
        let dot = if m.in_use { "●" } else { " " };
        let quant = m.quant.as_deref().unwrap_or("-");
        println!(
            "{:<2} {:<28} {:<7} {:<15} {:<9} {:<8} {}",
            dot,
            truncate_col(&m.display_name, 28),
            m.params_label,
            truncate_col(&m.source, 15),
            format_size_bytes(m.size_bytes),
            quant,
            m.id,
        );
    }
    if out.hidden_count > 0 {
        println!("({} hidden by filter '{}')", out.hidden_count, out.filter);
    }
    if out.user_hidden_count > 0 {
        println!("({} hidden by you)", out.user_hidden_count);
    }
}

fn confirm_replace_skill_io(
    path: &std::path::Path,
    reader: &mut dyn std::io::BufRead,
    writer: &mut dyn std::io::Write,
) -> bool {
    let _ = write!(
        writer,
        "'{}' já existe e não é do tchelvis. Sobrescrever? [y/N]: ",
        path.display()
    );
    let _ = writer.flush();
    let mut input = String::new();
    match reader.read_line(&mut input) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(
            input.trim().to_ascii_lowercase().as_str(),
            "y" | "yes" | "s" | "sim"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn confirm_replace_skill_io_matches_contract() {
        let p = std::path::Path::new("/tmp/to-tickets/SKILL.md");
        for yes in ["y\n", "Y\n", "yes\n", "s\n", "sim\n", "  SIM  \n"] {
            let mut out = Vec::new();
            let mut cursor = std::io::Cursor::new(yes.as_bytes());
            assert!(
                confirm_replace_skill_io(p, &mut cursor, &mut out),
                "expected true for {yes:?}"
            );
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "'/tmp/to-tickets/SKILL.md' já existe e não é do tchelvis. Sobrescrever? [y/N]: "
            );
        }
        for no in ["\n", "n\n", "no\n", "nao\n", ""] {
            let mut out = Vec::new();
            let mut cursor = std::io::Cursor::new(no.as_bytes());
            assert!(
                !confirm_replace_skill_io(p, &mut cursor, &mut out),
                "expected false for {no:?}"
            );
        }
    }

    #[test]
    fn task_add_requires_title_without_edit() {
        assert!(Cli::try_parse_from(["tchelvis", "task", "add"]).is_err());
    }

    #[test]
    fn task_add_edit_makes_title_optional() {
        let cli = Cli::try_parse_from(["tchelvis", "task", "add", "--edit"]).unwrap();
        match cli.command {
            Commands::Task {
                action: TaskCommand::Add { edit, title, .. },
            } => {
                assert!(edit);
                assert!(title.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn task_add_edit_accepts_prefill_text() {
        let cli = Cli::try_parse_from([
            "tchelvis", "task", "add", "--edit", "--", "rascunho", "longo",
        ])
        .unwrap();
        match cli.command {
            Commands::Task {
                action: TaskCommand::Add { edit, title, .. },
            } => {
                assert!(edit);
                assert_eq!(title, vec!["rascunho", "longo"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn supervisor_rules_test_enforces_flag_exclusivity() {
        assert!(Cli::try_parse_from(["tchelvis", "supervisor", "rules"]).is_ok());
        assert!(Cli::try_parse_from(["tchelvis", "supervisor", "rules", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["tchelvis", "supervisor", "rules", "test", "ls"]).is_ok());
        assert!(Cli::try_parse_from([
            "tchelvis",
            "supervisor",
            "rules",
            "test",
            "a.rs",
            "--tool",
            "Read"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "tchelvis",
            "supervisor",
            "rules",
            "test",
            "--screen",
            "s.txt",
            "--msg",
            "approve Bash: ls",
            "--kind",
            "permission"
        ])
        .is_ok());

        // Missing both command and --screen
        assert!(Cli::try_parse_from(["tchelvis", "supervisor", "rules", "test"]).is_err());
        // Command and --screen are mutually exclusive
        assert!(Cli::try_parse_from([
            "tchelvis",
            "supervisor",
            "rules",
            "test",
            "ls",
            "--screen",
            "s.txt"
        ])
        .is_err());
        // --tool only with command
        assert!(Cli::try_parse_from([
            "tchelvis",
            "supervisor",
            "rules",
            "test",
            "--screen",
            "s.txt",
            "--tool",
            "Bash"
        ])
        .is_err());
        // --msg and --kind only with --screen
        assert!(Cli::try_parse_from([
            "tchelvis",
            "supervisor",
            "rules",
            "test",
            "ls",
            "--msg",
            "x"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "tchelvis",
            "supervisor",
            "rules",
            "test",
            "ls",
            "--kind",
            "permission"
        ])
        .is_err());
    }

    #[test]
    fn format_human_status_row_preserves_loop_run_id() {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert(
            "cockpit.loop_run".to_string(),
            "20261124-193000-cafe".to_string(),
        );
        let row = AgentRow {
            address: "t-loop".to_string(),
            name: "loop-20261124-193000-cafe".to_string(),
            agent_kind: tchelvis::model::AgentKind::PlainShell,
            lifecycle: tchelvis::model::AgentLifecycle::Live,
            attention_state: AttentionState::Working {
                message: Some("loop 20261124-193000-cafe".to_string()),
            },
            workspace: Default::default(),
            correlation_tier: tchelvis::model::CorrelationTier::LocalOnly,
            context: Default::default(),
            provenance: vec![],
            labels,
            blocked_children: vec![],
            active_children: vec![],
            parent: None,
            prompt_destructive: false,
            turn: 0,
            reported: false,
            updated_at: 0,
        };
        let line = format_human_status_row(&row);
        assert!(line.contains("loop-20261124-193000-cafe"), "{line}");
        assert!(line.contains("working"), "{line}");
    }
}
