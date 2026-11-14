pub mod config;
pub mod guardrails;
pub mod hc;
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
        if snapshot.model_loaded {
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

/// Explicitly unloads the configured Ollama model (`keep_alive: 0`) and returns the updated `DesktopModSnapshot`.
pub fn unload_model() -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    engine.cognitive.unload_model()?;
    engine.model_loaded = false;
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Persists the GPU-offload toggle, unloads the model (`keep_alive: 0`) so the next evaluation
/// reloads with the new `num_gpu`, optionally warm-loads it, and returns the updated snapshot.
pub fn set_gpu(allow_gpu: bool, warm: bool) -> Result<DesktopModSnapshot> {
    config::write_gpu_override(&config::gpu_override_path(), allow_gpu)?;
    let mut config = SupervisorConfig::load();
    config.allow_gpu = allow_gpu;
    let client = OllamaClient::new(config.clone());
    let _ = client.unload_model();
    if warm {
        let _ = client.warm_model();
    }
    let mut engine = build_engine_from_config(config)?;
    engine.model_loaded = false;
    engine.store.model_loaded = false;
    let _ = engine.store.save();
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
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
