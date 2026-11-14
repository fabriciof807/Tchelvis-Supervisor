pub mod config;
pub mod config_write;
pub mod container;
pub mod gguf;
pub mod guardrails;
pub mod hc;
pub mod models;
pub mod ollama;
pub mod state;
pub mod tui;
pub mod watcher;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use config::SupervisorConfig;
use hc::{HcBackend, HcCliAdapter};
use ollama::{CognitiveBackend, OllamaClient};
use state::{unix_now_secs, DurableStateStore, SupervisorMode};
use std::path::PathBuf;
use std::time::Duration;
use tui::{build_desktop_mod_snapshot, enrich_snapshot_with_llm_health, DesktopModSnapshot};
use watcher::{SupervisorRuntimeStatus, TickOutcome, WatcherEngine};

/// Combined output for single-tick / `--once` supervisor invocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnceOutput {
    pub snapshot: DesktopModSnapshot,
    pub tick_outcome: TickOutcome,
}

/// Instantiates a `WatcherEngine` from the provided `SupervisorConfig`.
pub fn build_engine_from_config(
    config: SupervisorConfig,
) -> Result<WatcherEngine<HcCliAdapter, OllamaClient>> {
    let legacy_path = SupervisorConfig::legacy_state_file_path();
    let store =
        DurableStateStore::load_or_init_with_fallback(&config.state_file_path, &legacy_path)?;
    let hc = HcCliAdapter::new(config.hc.clone());
    hc.register_self_terminal();
    let cognitive = OllamaClient::new(config.clone());
    Ok(WatcherEngine::new(config, store, hc, cognitive))
}

/// Instantiates a `WatcherEngine` using `SupervisorConfig::load()`.
pub fn build_default_engine() -> Result<WatcherEngine<HcCliAdapter, OllamaClient>> {
    build_engine_from_config(SupervisorConfig::load())
}

/// Runs the interactive Ratatui TUI for the Cockpit Durable Supervisor.
pub fn run_tui() -> Result<()> {
    let mut engine = build_default_engine()?;
    tui::run_tui(&mut engine)
}

/// Executes a single deterministic watcher tick and returns the updated snapshot and tick outcome.
pub fn run_tick_once() -> Result<OnceOutput> {
    let mut engine = build_default_engine()?;
    let mut tick_outcome = engine.tick(unix_now_secs())?;
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    if tick_outcome.active_sessions == 0 {
        if !engine.config.container_mode_active() && snapshot.model_loaded {
            let _ = engine.cognitive.unload_model();
            engine.model_loaded = false;
            engine.store.model_loaded = false;
            let _ = engine.store.save();
            snapshot.model_loaded = false;
            tick_outcome.model_unloaded = true;
            enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
        }
        snapshot.runtime_status = SupervisorRuntimeStatus::Idle;
        snapshot.pill_text = SupervisorRuntimeStatus::Idle.pill_text();
    }
    Ok(OnceOutput {
        snapshot,
        tick_outcome,
    })
}

fn sync_runtime_status<H: HcBackend, C: CognitiveBackend>(engine: &mut WatcherEngine<H, C>) {
    if let Ok(sessions) = engine.hc.list_sessions() {
        let active_ids: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.agent_id.as_str()).collect();
        let before = engine.store.agents.len();
        engine
            .store
            .agents
            .retain(|id, _| active_ids.contains(id.as_str()));
        if engine.store.agents.len() != before {
            let _ = engine.store.save();
        }
        engine.runtime_status = if sessions.is_empty() {
            SupervisorRuntimeStatus::Idle
        } else {
            SupervisorRuntimeStatus::Watching(sessions.len())
        };
    }
}

/// Returns the current `DesktopModSnapshot` without running an evaluation tick.
pub fn current_status() -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Approves a pending operator action for `agent_id` and returns the updated `DesktopModSnapshot`.
/// If `command_payload` is `None`, uses the pending Action Card's `hc_command_payload` if present.
pub fn approve_action(agent_id: &str, command_payload: Option<&str>) -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    let payload = match command_payload {
        Some(p) => p.to_string(),
        None => engine
            .store
            .agents
            .get(agent_id)
            .and_then(|a| a.pending_operator_action.as_ref())
            .map(|c| c.hc_command_payload.clone())
            .unwrap_or_default(),
    };
    engine.approve_operator_action(agent_id, &payload, unix_now_secs())?;
    sync_runtime_status(&mut engine);
    Ok(build_desktop_mod_snapshot(&engine))
}

/// Ignores a pending operator alert for `agent_id` and returns the updated `DesktopModSnapshot`.
pub fn ignore_action(agent_id: &str) -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    engine.ignore_operator_action(agent_id)?;
    sync_runtime_status(&mut engine);
    Ok(build_desktop_mod_snapshot(&engine))
}

/// Sets or cycles the supervisor operating mode and returns the updated `DesktopModSnapshot`.
pub fn set_or_cycle_mode(mode: Option<SupervisorMode>) -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    match mode {
        Some(m) => {
            engine.set_mode(m)?;
        }
        None => {
            engine.cycle_mode()?;
        }
    }
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Explicitly starts/loads the managed `llama.cpp` container (or warms Ollama) and waits until healthy.
pub fn load_model() -> Result<DesktopModSnapshot> {
    let config = SupervisorConfig::load();
    if config.container_mode_active() {
        let candidates = config.runtime_candidates();
        if candidates.is_empty() {
            anyhow::bail!("Nenhum runtime de container encontrado (kiri/podman/docker)");
        }
        let ollama_roots = container::default_ollama_roots();
        let model = container::resolve_model_source(
            &config.llm_container,
            &config.ollama_model,
            &ollama_roots,
        )?;
        let model_id = models::effective_model(&config, &ollama_roots)
            .map(|m| m.id)
            .unwrap_or_default();
        let host = config.effective_primary_host();
        let pref_path = config::runtime_pref_path();
        container::start_with_fallback(
            &candidates,
            config.effective_num_gpu(),
            &model,
            &model_id,
            Some(&pref_path),
            |rt| {
                container::LlmContainer::new(
                    rt,
                    config.llm_container.clone(),
                    container::SystemContainerCli,
                )
            },
            || container::wait_healthy(&host, Duration::from_secs(90)),
        )?;
    } else {
        let client = OllamaClient::new(config.clone());
        client.warm_model()?;
    }

    let mut engine = build_engine_from_config(config)?;
    engine.model_loaded = true;
    engine.zero_active_since = None;
    engine.store.model_loaded = true;
    engine.store.zero_active_since = None;
    let _ = engine.store.save();
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Explicitly stops the managed container (or unloads Ollama with `keep_alive: 0`) and returns the updated snapshot.
pub fn unload_model() -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    engine.cognitive.unload_model()?;
    engine.model_loaded = false;
    engine.store.model_loaded = false;
    let _ = engine.store.save();
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Persists the GPU-offload toggle and applies it to the active LLM backend:
/// - In container mode: if Running, recreates with the new `-ngl` and waits healthy (or stops if `--no-warm`); if stopped, only persists.
/// - Outside container mode: unloads the Ollama model (`keep_alive: 0`) and optionally warm-loads it.
pub fn set_gpu(allow_gpu: bool, warm: bool) -> Result<DesktopModSnapshot> {
    config::write_gpu_override(&config::gpu_override_path(), allow_gpu)?;
    let mut config = SupervisorConfig::load();
    config.allow_gpu = allow_gpu;
    let mut loaded_after = false;

    if config.container_mode_active() {
        if let Some(rt) = config.detected_container_runtime() {
            let mgr = container::LlmContainer::new(
                rt,
                config.llm_container.clone(),
                container::SystemContainerCli,
            );
            if matches!(mgr.state(), Ok(container::ContainerState::Running)) {
                if warm {
                    let ollama_roots = container::default_ollama_roots();
                    let model = container::resolve_model_source(
                        &config.llm_container,
                        &config.ollama_model,
                        &ollama_roots,
                    )?;
                    let model_id = models::effective_model(&config, &ollama_roots)
                        .map(|m| m.id)
                        .unwrap_or_default();
                    mgr.start(config.effective_num_gpu(), &model, &model_id)?;
                    let host = config.effective_primary_host();
                    container::wait_healthy(&host, Duration::from_secs(90))?;
                    loaded_after = true;
                } else {
                    let _ = mgr.stop();
                }
            }
        }
    } else {
        let client = OllamaClient::new(config.clone());
        let _ = client.unload_model();
        if warm {
            let _ = client.warm_model();
        }
    }

    let mut engine = build_engine_from_config(config)?;
    engine.model_loaded = loaded_after;
    engine.store.model_loaded = loaded_after;
    let _ = engine.store.save();
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Lists local GGUF models with parameter filtering and effective-model marking.
pub fn list_models(
    filter_override: Option<models::MaxParams>,
    show_all: bool,
) -> Result<models::ModelListOutput> {
    let cfg = SupervisorConfig::load();
    let filter = filter_override
        .or_else(|| models::MaxParams::parse(&cfg.llm_container.model_filter))
        .unwrap_or(models::MaxParams::B4);

    if !cfg.container_mode_active() {
        return Ok(models::ModelListOutput {
            models: Vec::new(),
            hidden_count: 0,
            filter: filter.as_str().to_string(),
            selected_id: cfg.llm_container.model_id,
            managed: false,
            env_override_ignored: None,
        });
    }

    let roots = models::ScanRoots::from_env_and_config(&cfg.llm_container);
    let scanned = models::scan_local_models(&roots);
    let eff = models::effective_model(&cfg, &roots.ollama);
    let (model_list, hidden_count) =
        models::build_model_list(scanned, filter, eff.as_ref(), show_all);

    let model_id_set = cfg
        .llm_container
        .model_id
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let eff_display = eff.as_ref().map(|e| e.display_name.as_str()).unwrap_or("");
    let cockpit_env = std::env::var("COCKPIT_LLM_MODEL").ok();
    let ollama_env = std::env::var("OLLAMA_MODEL").ok();
    let env_note = models::env_override_note(
        model_id_set,
        eff_display,
        cockpit_env.as_deref(),
        ollama_env.as_deref(),
    );

    Ok(models::ModelListOutput {
        models: model_list,
        hidden_count,
        filter: filter.as_str().to_string(),
        selected_id: cfg.llm_container.model_id,
        managed: true,
        env_override_ignored: env_note,
    })
}

fn recreate_container_if_running(cfg: &SupervisorConfig) -> Result<()> {
    let Some(rt) = cfg.detected_container_runtime() else {
        return Ok(());
    };
    let mgr =
        container::LlmContainer::new(rt, cfg.llm_container.clone(), container::SystemContainerCli);
    if !matches!(mgr.state(), Ok(container::ContainerState::Running)) {
        return Ok(());
    }
    let ollama_roots = container::default_ollama_roots();
    let model =
        container::resolve_model_source(&cfg.llm_container, &cfg.ollama_model, &ollama_roots)?;
    let eff = models::effective_model(cfg, &ollama_roots);
    let display = eff
        .as_ref()
        .map(|e| e.display_name.as_str())
        .unwrap_or("modelo selecionado");
    let eff_id = eff.as_ref().map(|e| e.id.as_str()).unwrap_or("");

    let start_and_wait = mgr
        .start(cfg.effective_num_gpu(), &model, eff_id)
        .and_then(|_| {
            container::wait_healthy(&cfg.effective_primary_host(), Duration::from_secs(90))
        });
    if let Err(err) = start_and_wait {
        let _ = mgr.stop();
        anyhow::bail!("Falha ao carregar modelo {display}: {err}");
    }
    Ok(())
}

fn snapshot_from_config(cfg: SupervisorConfig) -> Result<DesktopModSnapshot> {
    let mut engine = build_engine_from_config(cfg)?;
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Persists a selected model id into `supervisor.toml` and recreates the container if running.
pub fn set_model(id: &str) -> Result<DesktopModSnapshot> {
    let cfg = SupervisorConfig::load();
    if !cfg.container_mode_active() {
        anyhow::bail!(
            "Model selection applies to the managed llama.cpp container (container mode is off or no runtime was found)"
        );
    }

    let roots = models::ScanRoots::from_env_and_config(&cfg.llm_container);
    let scanned = models::scan_local_models(&roots);
    let scanned_ids: Vec<String> = scanned.iter().map(|m| m.id.clone()).collect();
    let eff = models::effective_model(&cfg, &roots.ollama);
    models::validate_model_choice(id, &scanned_ids, eff.as_ref().map(|e| e.id.as_str()))?;

    if cfg.llm_container.model_id.as_deref() == Some(id) {
        return snapshot_from_config(cfg);
    }

    let ollama_name = match container::ModelId::parse(id)? {
        container::ModelId::Ollama(name) => Some(name),
        _ => None,
    };
    config_write::write_model_selection(
        &config::supervisor_config_path(),
        id,
        ollama_name.as_deref(),
    )?;

    let new_cfg = SupervisorConfig::load();
    recreate_container_if_running(&new_cfg)?;
    snapshot_from_config(new_cfg)
}

/// Persists the `max_params` filter into `supervisor.toml` and returns the updated model list.
pub fn set_model_filter(filter: models::MaxParams) -> Result<models::ModelListOutput> {
    config_write::write_model_filter(&config::supervisor_config_path(), filter.as_str())?;
    list_models(None, false)
}

/// Clears `llm_container.model_id` from `supervisor.toml` and recreates the container if running.
pub fn reset_model() -> Result<DesktopModSnapshot> {
    config_write::clear_model_selection(&config::supervisor_config_path())?;
    let new_cfg = SupervisorConfig::load();
    if new_cfg.container_mode_active() {
        recreate_container_if_running(&new_cfg)?;
    }
    snapshot_from_config(new_cfg)
}

/// Runs the supervisor deterministic watcher loop, persisting `DesktopModSnapshot` JSON
/// to `~/.local/state/cockpit/supervisor_mod_snapshot.json` on each tick.
pub fn run_supervisor_daemon() -> Result<()> {
    let mut engine = build_default_engine()?;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
    let snap_path = PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("cockpit")
        .join("supervisor_mod_snapshot.json");
    if let Some(parent) = snap_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    loop {
        let _ = engine.tick(unix_now_secs());
        let snapshot = build_desktop_mod_snapshot(&engine);
        if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
            let tmp_path = snap_path.with_extension("json.tmp");
            if std::fs::write(&tmp_path, json).is_ok() {
                let _ = std::fs::rename(&tmp_path, &snap_path);
            }
        }
        let interval = engine.config.poll_interval_secs.max(1);
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// Operator response to a deterministic pending prompt (no LLM needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespondAction {
    Approve,
    Deny,
}

impl RespondAction {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "approve" => Ok(Self::Approve),
            "deny" => Ok(Self::Deny),
            other => anyhow::bail!("Invalid respond action '{other}'; expected approve or deny"),
        }
    }
}

/// Maps a pending-prompt response action to the `hc keys` key name.
pub fn respond_keys(action: RespondAction) -> &'static str {
    match action {
        RespondAction::Approve => "enter",
        RespondAction::Deny => "esc",
    }
}

/// Server-side guard for `cockpit supervisor respond`.
pub fn validate_respond_guard(
    agent_id: &str,
    action: RespondAction,
    is_blocked: bool,
    pending_msg: &str,
    recent_output: &str,
) -> Result<&'static str> {
    if !is_blocked {
        anyhow::bail!("Nenhum prompt pendente em {agent_id}");
    }
    if action == RespondAction::Approve
        && (guardrails::detect_destructive(pending_msg, "").is_some()
            || guardrails::contains_forbidden_payload(pending_msg).is_some()
            || guardrails::detect_destructive(recent_output, "").is_some())
    {
        anyhow::bail!("Comando destrutivo: aprove manualmente no terminal (Open)");
    }
    Ok(respond_keys(action))
}

/// Sends a deterministic Enter/Esc response to a blocked terminal after server-side guardrail checks.
pub fn respond_pending_prompt(terminal: &str, action: RespondAction) -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    let (is_blocked, pending_msg) = engine.hc.pending_prompt_info(terminal)?;
    let recent_output = if action == RespondAction::Approve {
        engine.hc.capture_output(terminal, 25).unwrap_or_default()
    } else {
        String::new()
    };
    let key = validate_respond_guard(terminal, action, is_blocked, &pending_msg, &recent_output)?;
    engine.hc.send_keys(terminal, key)?;
    let _ = engine.hc.set_labels(
        terminal,
        &[
            ("cockpit.state", "working"),
            ("cockpit.kind", ""),
            ("cockpit.msg", ""),
        ],
    );
    if let Some(agent) = engine.store.agents.get_mut(terminal) {
        agent.pending_operator_action = None;
        let _ = engine.store.save();
    }
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_maps_to_enter_deny_to_esc() {
        assert_eq!(respond_keys(RespondAction::Approve), "enter");
        assert_eq!(respond_keys(RespondAction::Deny), "esc");
    }

    #[test]
    fn approve_refused_for_destructive_prompt() {
        let err = validate_respond_guard(
            "t1",
            RespondAction::Approve,
            true,
            "Approve Bash: rm -rf build",
            "",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("Comando destrutivo: aprove manualmente no terminal (Open)"));

        // Deny is still allowed even when the prompt is destructive.
        assert_eq!(
            validate_respond_guard(
                "t1",
                RespondAction::Deny,
                true,
                "Approve Bash: rm -rf build",
                "",
            )
            .unwrap(),
            "esc"
        );
    }

    #[test]
    fn respond_requires_blocked() {
        let err = validate_respond_guard("t1", RespondAction::Approve, false, "", "").unwrap_err();
        assert!(err.to_string().contains("Nenhum prompt pendente em t1"));
    }
}
