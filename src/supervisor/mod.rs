pub mod bench;
pub mod chat;
pub mod config;
pub mod config_write;
pub mod container;
pub mod events;
pub mod fallback;
pub mod gguf;
pub mod guardrails;
pub mod hc;
pub mod model_delete;
pub mod models;
pub mod ollama;
pub mod rules;
pub mod slot_lock;
pub mod state;
pub mod tui;
pub mod watcher;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use config::SupervisorConfig;
use events::{read_events, EventLogQuery, SupervisorEvent};
use hc::{HcBackend, HcCliAdapter};
use ollama::{CognitiveBackend, OllamaClient};
use state::{unix_now_ms, unix_now_secs, DurableStateStore, SupervisorMode};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tui::{build_desktop_mod_snapshot, enrich_snapshot_with_llm_health, DesktopModSnapshot};
use watcher::{SupervisorRuntimeStatus, TickOutcome, WatcherEngine};

/// Resolves the path to `supervisor_events.jsonl` in the active state directory.
pub fn events_log_path() -> PathBuf {
    let cfg = SupervisorConfig::load();
    events_log_path_for_state(&cfg.state_file_path)
}

/// Resolves the `supervisor_events.jsonl` path adjacent to a given `state_file_path`.
pub fn events_log_path_for_state(state_file_path: &Path) -> PathBuf {
    state::sibling_path(state_file_path, "supervisor_events.jsonl").unwrap_or_else(|| {
        config::supervisor_state_dir_in(&config::home_or_fallback_dir())
            .join("supervisor_events.jsonl")
    })
}

/// Reads supervisor activity events newest-first from the active state directory.
pub fn read_supervisor_log(query: &EventLogQuery) -> Result<Vec<SupervisorEvent>> {
    read_events(&events_log_path(), query)
}

/// Reads supervisor activity events newest-first from the state directory of `state_file_path`.
pub fn read_supervisor_log_from(
    state_file_path: &Path,
    query: &EventLogQuery,
) -> Result<Vec<SupervisorEvent>> {
    read_events(&events_log_path_for_state(state_file_path), query)
}

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
    let mut engine = WatcherEngine::new(config.clone(), store, hc, cognitive)
        .with_rules_source(Box::new(rules::load_default_guardrail_rules));
    if config.container_mode_active() {
        engine = engine.with_fallback(Box::new(fallback::LiveFallback {
            state_path: fallback::fallback_state_path(),
            primary: config,
        }));
    }
    Ok(engine)
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
            engine
                .store
                .record_supervisor_event(&SupervisorEvent::llm_unload(
                    unix_now_ms(),
                    Some(&engine.config.ollama_model),
                    "LLM descarregado por inatividade (0 sessões ativas)",
                    None,
                ));
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
    let loaded_model: String;
    let load_detail: String;
    if config.container_mode_active() {
        fallback::mark_primary_starting(&fallback::fallback_state_path(), unix_now_secs());
        let ollama_roots = container::default_ollama_roots();
        let eff_model = models::effective_model(&config, &ollama_roots);
        loaded_model = eff_model
            .as_ref()
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| config.ollama_model.clone());
        load_detail = format!(
            "backend=llama-cpp; gpu={}",
            if config.allow_gpu { "on" } else { "off" }
        );
        let start_res = (|| -> Result<()> {
            let candidates = config.runtime_candidates();
            if candidates.is_empty() {
                anyhow::bail!("Nenhum runtime de container encontrado (kiri/podman/docker)");
            }
            let model = container::resolve_model_source(
                &config.llm_container,
                &config.ollama_model,
                &ollama_roots,
            )?;
            let model_id = eff_model.map(|m| m.id).unwrap_or_default();
            let host = config.effective_primary_host();
            let pref_path = config::runtime_pref_path();
            let (gpu, _) = container::resolve_gpu_backend(&config.llm_container);
            container::start_with_fallback(
                &candidates,
                config.effective_num_gpu(),
                &model,
                &model_id,
                gpu,
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
            Ok(())
        })();
        let provider = fallback::LiveFallback {
            state_path: fallback::fallback_state_path(),
            primary: config.clone(),
        };
        fallback::finish_primary_start(&provider, start_res, unix_now_secs())?;
    } else {
        loaded_model = config.ollama_model.clone();
        load_detail = format!(
            "backend={}; gpu={}",
            config.effective_backend().as_str(),
            if config.allow_gpu { "on" } else { "off" }
        );
        let client = OllamaClient::new(config.clone());
        client.warm_model()?;
    }

    let mut engine = build_engine_from_config(config)?;
    engine.model_loaded = true;
    engine.zero_active_since = None;
    engine.store.model_loaded = true;
    engine.store.zero_active_since = None;
    let _ = engine.store.save();
    engine
        .store
        .record_supervisor_event(&SupervisorEvent::llm_load(
            unix_now_ms(),
            Some(&loaded_model),
            Some(&load_detail),
        ));
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Explicitly stops the managed container (or unloads Ollama with `keep_alive: 0`) and returns the updated snapshot.
pub fn unload_model() -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    // Record the intentional stop before stopping, so a concurrent tick never reads the
    // stopping container as a failure and switches to the fallback.
    if engine.config.container_mode_active() {
        let _ = engine.fallback.update_state(&mut fallback::primary_stopped);
        engine.refresh_fallback_state();
    }
    engine.cognitive.unload_model()?;
    engine.model_loaded = false;
    engine.store.model_loaded = false;
    let _ = engine.store.save();
    let unloaded_model = Some(engine.cognitive.model_label()).filter(|m| !m.is_empty());
    engine
        .store
        .record_supervisor_event(&SupervisorEvent::llm_unload(
            unix_now_ms(),
            unloaded_model.as_deref(),
            "LLM descarregado pelo operador",
            None,
        ));
    sync_runtime_status(&mut engine);
    let mut snapshot = build_desktop_mod_snapshot(&engine);
    enrich_snapshot_with_llm_health(&mut snapshot, &engine.config);
    Ok(snapshot)
}

/// Persists the GPU-offload toggle and applies it to the active LLM backend:
/// - In container mode: if Running, recreates with the new `-ngl` and waits healthy (or stops if `--no-warm`); if stopped, only persists.
/// - Outside container mode: unloads the Ollama model (`keep_alive: 0`) and optionally warm-loads it.
pub fn set_gpu(allow_gpu: bool, warm: bool) -> Result<DesktopModSnapshot> {
    let prev_allow_gpu = SupervisorConfig::load().allow_gpu;
    config::write_gpu_override(&config::try_gpu_override_path()?, allow_gpu)?;
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
                let provider = fallback::LiveFallback {
                    state_path: fallback::fallback_state_path(),
                    primary: config.clone(),
                };
                if warm {
                    fallback::mark_primary_starting(
                        &fallback::fallback_state_path(),
                        unix_now_secs(),
                    );
                    let res = (|| -> Result<()> {
                        let ollama_roots = container::default_ollama_roots();
                        let model = container::resolve_model_source(
                            &config.llm_container,
                            &config.ollama_model,
                            &ollama_roots,
                        )?;
                        let model_id = models::effective_model(&config, &ollama_roots)
                            .map(|m| m.id)
                            .unwrap_or_default();
                        let (gpu, _) = container::resolve_gpu_backend(&config.llm_container);
                        mgr.start(config.effective_num_gpu(), &model, &model_id, gpu)?;
                        let host = config.effective_primary_host();
                        container::wait_healthy(&host, Duration::from_secs(90))?;
                        Ok(())
                    })();
                    fallback::finish_primary_start(&provider, res, unix_now_secs())?;
                    loaded_after = true;
                } else {
                    // State first (see `unload_model`), then stop.
                    let _ = fallback::update_state(
                        &fallback::fallback_state_path(),
                        fallback::primary_stopped,
                    );
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
    engine
        .store
        .record_supervisor_event(&SupervisorEvent::gpu_toggle(
            unix_now_ms(),
            prev_allow_gpu,
            allow_gpu,
            Some(&format!("warm={warm}; loaded={loaded_after}")),
        ));
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
            user_hidden_count: 0,
            filter: filter.as_str().to_string(),
            selected_id: cfg.llm_container.model_id,
            managed: false,
            env_override_ignored: None,
        });
    }

    let roots = models::ScanRoots::from_env_and_config(&cfg.llm_container);
    let scanned = models::scan_local_models(&roots);
    let eff = models::effective_model(&cfg, &roots.ollama);
    let (model_list, hidden_count, user_hidden_count) = models::build_model_list(
        scanned,
        filter,
        eff.as_ref(),
        show_all,
        &cfg.llm_container.hidden_models,
    );

    let model_id_set = cfg
        .llm_container
        .model_id
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let eff_display = eff.as_ref().map(|e| e.display_name.as_str()).unwrap_or("");
    let tchelvis_env = crate::envvars::var("LLM_MODEL");
    let ollama_env = std::env::var("OLLAMA_MODEL").ok();
    let env_note = models::env_override_note(
        model_id_set,
        eff_display,
        tchelvis_env.as_deref(),
        ollama_env.as_deref(),
    );

    Ok(models::ModelListOutput {
        models: model_list,
        hidden_count,
        user_hidden_count,
        filter: filter.as_str().to_string(),
        selected_id: cfg.llm_container.model_id,
        managed: true,
        env_override_ignored: env_note,
    })
}

fn hide_model_in(
    id: &str,
    cfg_path: &std::path::Path,
    cfg: &SupervisorConfig,
    ollama_roots: &[PathBuf],
) -> Result<()> {
    let eff = models::effective_model(cfg, ollama_roots);
    if eff.as_ref().map(|e| e.id.as_str()) == Some(id) {
        anyhow::bail!("cannot hide the model in use: {id}");
    }
    if !cfg.llm_container.hidden_models.iter().any(|h| h == id) {
        let mut next = cfg.llm_container.hidden_models.clone();
        next.push(id.to_string());
        config_write::write_hidden_models(cfg_path, &next)?;
    }
    Ok(())
}

fn unhide_model_in(id: &str, cfg_path: &std::path::Path, cfg: &SupervisorConfig) -> Result<()> {
    if cfg.llm_container.hidden_models.iter().any(|h| h == id) {
        let next: Vec<String> = cfg
            .llm_container
            .hidden_models
            .iter()
            .filter(|h| h.as_str() != id)
            .cloned()
            .collect();
        config_write::write_hidden_models(cfg_path, &next)?;
    }
    Ok(())
}

/// Persists `id` into `[llm_container] hidden_models` and returns the updated model list.
/// Idempotent; errors if `id` is the currently effective model.
pub fn hide_model(id: &str) -> Result<models::ModelListOutput> {
    let cfg = SupervisorConfig::load();
    let roots = models::ScanRoots::from_env_and_config(&cfg.llm_container);
    hide_model_in(id, &config::supervisor_config_path(), &cfg, &roots.ollama)?;
    list_models(None, false)
}

/// Removes `id` from `[llm_container] hidden_models` and returns the updated model list.
/// Idempotent.
pub fn unhide_model(id: &str) -> Result<models::ModelListOutput> {
    let cfg = SupervisorConfig::load();
    unhide_model_in(id, &config::supervisor_config_path(), &cfg)?;
    list_models(None, false)
}

pub use model_delete::DeleteOutcome;

#[allow(clippy::too_many_arguments)]
pub(crate) fn delete_model_with(
    id: &str,
    cfg_path: &std::path::Path,
    cfg: &SupervisorConfig,
    roots: &models::ScanRoots,
    running_id: Option<&str>,
    ollama: &dyn model_delete::OllamaRemover,
    confirm: &mut dyn FnMut(&models::LocalModel, &model_delete::DeletePlan) -> bool,
) -> Result<DeleteOutcome> {
    let scanned = models::scan_local_models(roots);
    let eff = models::effective_model(cfg, &roots.ollama);
    let filter =
        models::MaxParams::parse(&cfg.llm_container.model_filter).unwrap_or(models::MaxParams::B4);
    let (all_models, _, _) = models::build_model_list(
        scanned,
        filter,
        eff.as_ref(),
        true,
        &cfg.llm_container.hidden_models,
    );

    let Some(model) = all_models.iter().find(|m| m.id == id) else {
        anyhow::bail!("model not found: {id}");
    };

    if let Some(reason) = model_delete::protected_reason(
        id,
        eff.as_ref().map(|e| e.id.as_str()),
        cfg.llm_container.fallback_model_id.as_deref(),
        running_id,
    ) {
        anyhow::bail!("cannot delete {id}: {reason}");
    }

    let plan = model_delete::plan_delete(model)?;
    if !confirm(model, &plan) {
        return Ok(DeleteOutcome {
            id: id.to_string(),
            plan,
            deleted: false,
        });
    }

    model_delete::execute_plan(&plan, ollama)?;
    unhide_model_in(id, cfg_path, cfg)?;

    Ok(DeleteOutcome {
        id: id.to_string(),
        plan,
        deleted: true,
    })
}

/// Deletes a model from disk or Ollama after checking protection and calling `confirm`.
pub fn delete_model(
    id: &str,
    confirm: &mut dyn FnMut(&models::LocalModel, &model_delete::DeletePlan) -> bool,
) -> Result<DeleteOutcome> {
    let cfg = SupervisorConfig::load();
    let roots = models::ScanRoots::from_env_and_config(&cfg.llm_container);
    let running_id = cfg.detected_container_runtime().and_then(|rt| {
        let mgr = container::LlmContainer::new(
            rt,
            cfg.llm_container.clone(),
            container::SystemContainerCli,
        );
        if matches!(mgr.state(), Ok(container::ContainerState::Running)) {
            mgr.container_model_id()
        } else {
            None
        }
    });
    let ollama = model_delete::SystemOllamaRemover {
        host: cfg.ollama_host.clone(),
    };
    delete_model_with(
        id,
        &config::supervisor_config_path(),
        &cfg,
        &roots,
        running_id.as_deref(),
        &ollama,
        confirm,
    )
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
    if cfg.container_mode_active() {
        fallback::mark_primary_starting(&fallback::fallback_state_path(), unix_now_secs());
    }
    let res = (|| -> Result<()> {
        let ollama_roots = container::default_ollama_roots();
        let model =
            container::resolve_model_source(&cfg.llm_container, &cfg.ollama_model, &ollama_roots)?;
        let eff = models::effective_model(cfg, &ollama_roots);
        let display = eff
            .as_ref()
            .map(|e| e.display_name.as_str())
            .unwrap_or("modelo selecionado");
        let eff_id = eff.as_ref().map(|e| e.id.as_str()).unwrap_or("");
        let (gpu, _) = container::resolve_gpu_backend(&cfg.llm_container);

        let start_and_wait = mgr
            .start(cfg.effective_num_gpu(), &model, eff_id, gpu)
            .and_then(|_| {
                container::wait_healthy(&cfg.effective_primary_host(), Duration::from_secs(90))
            });
        if let Err(err) = start_and_wait {
            let _ = mgr.stop();
            anyhow::bail!("Falha ao carregar modelo {display}: {err}");
        }
        Ok(())
    })();
    if cfg.container_mode_active() {
        let provider = fallback::LiveFallback {
            state_path: fallback::fallback_state_path(),
            primary: cfg.clone(),
        };
        fallback::finish_primary_start(&provider, res, unix_now_secs())
    } else {
        res
    }
}

/// Attempts to return to the primary local model (`load_model`). Clears fallback only on success.
pub fn fallback_off() -> Result<DesktopModSnapshot> {
    load_model()
}

/// Returns the fallback configuration and runtime state (never includes the API key).
pub fn fallback_status() -> fallback::FallbackInfo {
    let cfg = SupervisorConfig::load();
    let key_present = fallback::resolve_fallback_key(
        &cfg.fallback.host,
        fallback::read_key_file(&fallback::fallback_key_path()),
        |k| std::env::var(k).ok(),
    )
    .is_some();
    let state = fallback::load_state(&fallback::fallback_state_path());
    fallback::fallback_snapshot_info(&cfg.fallback, key_present, &state)
}

/// Returns the name of the active fallback key source (never the key value itself).
pub fn fallback_key_source(host: &str) -> Option<&'static str> {
    fallback::resolve_fallback_key_with_source(
        host,
        fallback::read_key_file(&fallback::fallback_key_path()),
        |k| std::env::var(k).ok(),
    )
    .map(|(_, src)| src)
}

/// Updates `[fallback] host` and/or `model` in `supervisor.toml` and returns updated status.
pub fn fallback_set(host: Option<&str>, model: Option<&str>) -> Result<fallback::FallbackInfo> {
    if host.is_none() && model.is_none() {
        anyhow::bail!("informe --host e/ou --model");
    }
    let host = host.map(fallback::normalize_fallback_host);
    config_write::write_fallback(&config::supervisor_config_path(), host.as_deref(), model)?;
    Ok(fallback_status())
}

/// Saves the fallback API key from `r` into `~/.config/tchelvis/fallback_api_key` (0600).
pub fn fallback_key_from_reader(r: impl std::io::Read) -> Result<()> {
    fallback::write_key_from_reader(&fallback::fallback_key_path(), r)
}

/// Queries `GET {host}/v1/models` on the configured fallback endpoint using the resolved key.
pub fn fallback_models(filter: Option<&str>) -> Result<Vec<String>> {
    let cfg = SupervisorConfig::load();
    let host = cfg.fallback.host.trim();
    if host.is_empty() {
        anyhow::bail!("nenhum host de fallback configurado (use `tchelvis supervisor fallback set --host <url>`)");
    }
    let key = fallback::resolve_fallback_key(
        host,
        fallback::read_key_file(&fallback::fallback_key_path()),
        |k| std::env::var(k).ok(),
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "nenhuma chave de API encontrada para o fallback (rode `tchelvis supervisor fallback key`)"
        )
    })?;
    let url = fallback::models_url(host);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let resp = match agent
        .get(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(status, _)) => {
            anyhow::bail!("{}", fallback::models_http_error(status));
        }
        Err(ureq::Error::Transport(t)) => {
            anyhow::bail!("{}", fallback::redact_key(&t.to_string(), &key));
        }
    };
    let val = resp
        .into_json::<serde_json::Value>()
        .map_err(|e| anyhow::anyhow!("resposta inválida de /models: {e}"))?;
    let ids = fallback::parse_models_list(&val);
    Ok(fallback::filter_models(&ids, filter))
}

/// Removes `[fallback]` from `supervisor.toml` and clears any active fallback state.
pub fn fallback_clear() -> Result<fallback::FallbackInfo> {
    config_write::clear_fallback(&config::supervisor_config_path())?;
    fallback::update_state(&fallback::fallback_state_path(), |s| {
        s.active = false;
        s.reason = None;
        s.detail = None;
        s.since = None;
    })?;
    Ok(fallback_status())
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
/// to `~/.local/state/tchelvis/supervisor_mod_snapshot.json` on each tick.
pub fn run_supervisor_daemon() -> Result<()> {
    let mut engine = build_default_engine()?;
    let home = config::require_home_dir()?;
    let snap_path = config::supervisor_state_dir_in(&home).join("supervisor_mod_snapshot.json");
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

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
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

/// Server-side guard for `tchelvis supervisor respond`.
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

pub(crate) fn respond_pending_prompt_with<H: HcBackend, C: CognitiveBackend>(
    engine: &mut WatcherEngine<H, C>,
    terminal: &str,
    action: RespondAction,
) -> Result<()> {
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
    let session_name = engine
        .store
        .agents
        .get(terminal)
        .map(|a| a.session_name.clone());
    if let Some(agent) = engine.store.agents.get_mut(terminal) {
        agent.pending_operator_action = None;
        let _ = engine.store.save();
    }
    let verb = match action {
        RespondAction::Approve => "aprovou",
        RespondAction::Deny => "negou",
    };
    let summary = format!("Operador {verb} prompt em {terminal} (tecla {key})");
    let detail = (!pending_msg.trim().is_empty()).then_some(pending_msg.as_str());
    engine
        .store
        .record_supervisor_event(&SupervisorEvent::respond(
            unix_now_ms(),
            terminal,
            session_name.as_deref(),
            action.as_str(),
            Some(key),
            &summary,
            detail,
        ));
    Ok(())
}

/// Sends a deterministic Enter/Esc response to a blocked terminal after server-side guardrail checks.
pub fn respond_pending_prompt(terminal: &str, action: RespondAction) -> Result<DesktopModSnapshot> {
    let mut engine = build_default_engine()?;
    respond_pending_prompt_with(&mut engine, terminal, action)?;
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

    #[test]
    fn hide_and_unhide_roundtrip_and_refuse_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("supervisor.toml");
        let mut cfg = SupervisorConfig::default();
        cfg.llm_container.model_id = Some("file:/m/active.gguf".to_string());

        let err = hide_model_in("file:/m/active.gguf", &cfg_path, &cfg, &[]).unwrap_err();
        assert!(err.to_string().contains("cannot hide the model in use"));

        hide_model_in("file:/m/other.gguf", &cfg_path, &cfg, &[]).unwrap();
        let parsed1: SupervisorConfig =
            toml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            parsed1.llm_container.hidden_models,
            vec!["file:/m/other.gguf".to_string()]
        );

        // Idempotent hide
        hide_model_in("file:/m/other.gguf", &cfg_path, &parsed1, &[]).unwrap();
        let parsed2: SupervisorConfig =
            toml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            parsed2.llm_container.hidden_models,
            vec!["file:/m/other.gguf".to_string()]
        );

        unhide_model_in("file:/m/other.gguf", &cfg_path, &parsed2).unwrap();
        let raw3 = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(!raw3.contains("hidden_models"));

        // Idempotent unhide
        let parsed3: SupervisorConfig = toml::from_str(&raw3).unwrap();
        unhide_model_in("file:/m/other.gguf", &cfg_path, &parsed3).unwrap();
    }

    struct FakeOllama {
        removed: std::sync::Mutex<Vec<String>>,
    }

    impl FakeOllama {
        fn new() -> Self {
            Self {
                removed: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl model_delete::OllamaRemover for FakeOllama {
        fn remove(&self, name: &str) -> Result<()> {
            self.removed.lock().unwrap().push(name.to_string());
            Ok(())
        }
    }

    fn test_roots(folder: PathBuf) -> models::ScanRoots {
        models::ScanRoots {
            ollama: vec![],
            hf_hub: vec![],
            llama_cpp: vec![],
            lm_studio: vec![],
            folders: vec![folder],
        }
    }

    #[test]
    fn rm_unknown_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("supervisor.toml");
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let cfg = SupervisorConfig::default();
        let roots = test_roots(models_dir);
        let fake = FakeOllama::new();

        let err = delete_model_with(
            "file:/does/not/exist.gguf",
            &cfg_path,
            &cfg,
            &roots,
            None,
            &fake,
            &mut |_, _| true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("model not found"));
    }

    #[test]
    fn rm_protected_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("supervisor.toml");
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let model_file = std::fs::canonicalize(&models_dir)
            .unwrap()
            .join("active.gguf");
        std::fs::write(&model_file, b"gguf").unwrap();
        let id = format!("file:{}", model_file.display());

        let mut cfg = SupervisorConfig::default();
        cfg.llm_container.model_id = Some(id.clone());
        let roots = test_roots(models_dir);
        let fake = FakeOllama::new();

        let err = delete_model_with(&id, &cfg_path, &cfg, &roots, None, &fake, &mut |_, _| true)
            .unwrap_err();
        assert!(err.to_string().contains("in use by the supervisor"));
        assert!(model_file.exists());
    }

    #[test]
    fn rm_declined_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("supervisor.toml");
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let model_file = std::fs::canonicalize(&models_dir)
            .unwrap()
            .join("keep.gguf");
        std::fs::write(&model_file, b"gguf").unwrap();
        let id = format!("file:{}", model_file.display());

        let cfg = SupervisorConfig::default();
        let roots = test_roots(models_dir);
        let fake = FakeOllama::new();

        let outcome =
            delete_model_with(&id, &cfg_path, &cfg, &roots, None, &fake, &mut |_, _| false)
                .unwrap();
        assert!(!outcome.deleted);
        assert!(model_file.exists());
    }

    #[test]
    fn rm_removes_hidden_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("supervisor.toml");
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let model_file = std::fs::canonicalize(&models_dir)
            .unwrap()
            .join("hidden.gguf");
        std::fs::write(&model_file, b"gguf").unwrap();
        let id = format!("file:{}", model_file.display());

        config_write::write_hidden_models(&cfg_path, std::slice::from_ref(&id)).unwrap();
        let mut cfg = SupervisorConfig::default();
        cfg.llm_container.hidden_models = vec![id.clone()];
        let roots = test_roots(models_dir);
        let fake = FakeOllama::new();

        let outcome =
            delete_model_with(&id, &cfg_path, &cfg, &roots, None, &fake, &mut |_, _| true).unwrap();
        assert!(outcome.deleted);
        assert!(!model_file.exists());
        let raw = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed: SupervisorConfig = toml::from_str(&raw).unwrap();
        assert!(parsed.llm_container.hidden_models.is_empty());
    }
}
