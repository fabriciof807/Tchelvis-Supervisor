use super::config::SupervisorConfig;
use super::fallback::{
    apply_health_probe, primary_stopped, FallbackProvider, FallbackState, NoFallback, ProbeCtx,
};
use super::guardrails::{
    enforce_blocked_policy, has_error_or_loop_signature, readonly_fast_path_with_msg,
    sanitize_auto_reply, tidy_operator_reason, validate_evaluation,
};
use super::hc::{HcBackend, HcSession};
use super::ollama::{CognitiveBackend, CognitiveEvaluation, SupervisorDecision};
use super::state::{
    compute_sha256, AgentDurableState, AutoReplyAuditEntry, AutoReplySource, DurableAutoReplyEntry,
    DurableStateStore, LifecycleState, PendingOperatorAction, SupervisorMode, Urgency,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::process::Command;

/// Runtime status of the deterministic watcher for the Top-Bar Status Pill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorRuntimeStatus {
    Idle,
    Watching(usize),
    Evaluating(String),
}

impl SupervisorRuntimeStatus {
    pub fn pill_text(&self) -> String {
        match self {
            Self::Idle => "Supervisor: Idle".to_string(),
            Self::Watching(n) => format!("Supervisor: Watching ({n})"),
            Self::Evaluating(_) => "Supervisor: Evaluating".to_string(),
        }
    }
}

/// Summary of actions performed during a single deterministic watcher tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickOutcome {
    pub active_sessions: usize,
    pub evaluated_agents: Vec<String>,
    pub auto_replied_agents: Vec<String>,
    pub alerted_agents: Vec<String>,
    pub model_unloaded: bool,
}

/// Deterministic Watcher & Guardrail Orchestrator.
pub struct WatcherEngine<H: HcBackend, C: CognitiveBackend> {
    pub config: SupervisorConfig,
    pub store: DurableStateStore,
    pub hc: H,
    pub cognitive: C,
    pub zero_active_since: Option<u64>,
    pub model_loaded: bool,
    pub runtime_status: SupervisorRuntimeStatus,
    pub fallback: Box<dyn FallbackProvider>,
    last_fallback_state: FallbackState,
    last_eval_model: Option<String>,
}

impl<H: HcBackend, C: CognitiveBackend> WatcherEngine<H, C> {
    pub fn new(config: SupervisorConfig, store: DurableStateStore, hc: H, cognitive: C) -> Self {
        let zero_active_since = store.zero_active_since;
        let model_loaded = store.model_loaded;
        Self {
            config,
            store,
            hc,
            cognitive,
            zero_active_since,
            model_loaded,
            runtime_status: SupervisorRuntimeStatus::Idle,
            fallback: Box::new(NoFallback),
            last_fallback_state: FallbackState::default(),
            last_eval_model: None,
        }
    }

    pub fn with_fallback(mut self, p: Box<dyn FallbackProvider>) -> Self {
        self.last_fallback_state = p.load_state();
        self.fallback = p;
        self
    }

    pub fn fallback_state(&self) -> FallbackState {
        self.last_fallback_state.clone()
    }

    pub fn refresh_fallback_state(&mut self) {
        self.last_fallback_state = self.fallback.load_state();
    }

    /// Executes one deterministic poll/diff/evaluate cycle at Unix timestamp `now`.
    pub fn tick(&mut self, now: u64) -> Result<TickOutcome> {
        let sessions = self.hc.list_sessions()?;
        let active_ids: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.agent_id.as_str()).collect();
        self.store
            .agents
            .retain(|id, _| active_ids.contains(id.as_str()));

        let mut outcome = TickOutcome {
            active_sessions: sessions.len(),
            ..Default::default()
        };

        if sessions.is_empty() {
            let zero_since = *self.zero_active_since.get_or_insert(now);
            let mut fb = self.fallback.load_state();
            let idle_expired = now.saturating_sub(zero_since) >= self.config.idle_unload_secs;
            if !self.model_loaded || idle_expired {
                if self.model_loaded {
                    let _ = self.cognitive.unload_model();
                    self.model_loaded = false;
                    outcome.model_unloaded = true;
                    if let Ok(updated) = self.fallback.update_state(&mut primary_stopped) {
                        fb = updated;
                    }
                } else if fb.active && idle_expired {
                    if let Ok(updated) = self.fallback.update_state(&mut primary_stopped) {
                        fb = updated;
                    }
                }
                self.runtime_status = SupervisorRuntimeStatus::Idle;
            } else {
                self.runtime_status = SupervisorRuntimeStatus::Watching(0);
            }
            self.last_fallback_state = fb;
            self.store.zero_active_since = self.zero_active_since;
            self.store.model_loaded = self.model_loaded;
            self.store.save()?;
            return Ok(outcome);
        }

        self.zero_active_since = None;
        self.runtime_status = SupervisorRuntimeStatus::Watching(sessions.len());

        if self.store.mode == SupervisorMode::Paused {
            self.last_fallback_state = self.fallback.load_state();
            for session in &sessions {
                let entry = self
                    .store
                    .agents
                    .entry(session.agent_id.clone())
                    .or_insert_with(|| {
                        AgentDurableState::new(
                            &session.agent_id,
                            &session.session_name,
                            self.config.max_auto_replies,
                            now,
                        )
                    });
                entry.session_name = session.session_name.clone();
                if !session.is_blocked() {
                    entry.pending_operator_action = None;
                }
            }
            self.store.zero_active_since = self.zero_active_since;
            self.store.model_loaded = self.model_loaded;
            self.store.save()?;
            return Ok(outcome);
        }

        let mut fb = self.fallback.load_state();
        if !fb.active && fb.primary_intended {
            let (healthy, detail) = self.fallback.probe_primary();
            let configured = self.fallback.backend().is_some();
            match self.fallback.update_state(&mut |s| {
                apply_health_probe(
                    s,
                    &ProbeCtx {
                        healthy,
                        paused: false,
                        sessions: sessions.len(),
                        configured,
                        now,
                        detail: detail.clone(),
                        min_gap_secs: (self.config.poll_interval_secs * 3 / 4).max(1),
                    },
                );
            }) {
                Ok(updated) => fb = updated,
                Err(err) => {
                    tracing::warn!(
                        error = %format!("{err:#}"),
                        "Failed to update fallback state"
                    );
                }
            }
        }
        self.last_fallback_state = fb.clone();

        let fb_backend = if fb.active {
            self.fallback.backend()
        } else {
            None
        };
        let llm_available = if fb.active {
            fb_backend.is_some()
        } else {
            self.cognitive.is_available()
        };
        if !fb.active && !llm_available {
            self.model_loaded = false;
        }

        // Blocked sessions first (longest-waiting first), then the rest; at most
        // `max_evals_per_tick` LLM evaluations per tick so a new prompt never waits a full round.
        let mut ordered: Vec<&HcSession> = sessions.iter().collect();
        ordered.sort_by_key(|s| {
            let since = self
                .store
                .agents
                .get(&s.agent_id)
                .map(|a| a.last_change_timestamp)
                .unwrap_or(now);
            (!s.is_blocked(), since)
        });
        let eval_cap = self.config.max_evals_per_tick.max(1);
        let mut evals_started = 0usize;

        for session in ordered {
            let output = match self
                .hc
                .capture_output(&session.agent_id, self.config.capture_lines)
            {
                Ok(out) => out,
                Err(_) => continue,
            };

            let current_hash = compute_sha256(&output);
            let state = self
                .store
                .agents
                .entry(session.agent_id.clone())
                .or_insert_with(|| {
                    AgentDurableState::new(
                        &session.agent_id,
                        &session.session_name,
                        self.config.max_auto_replies,
                        now,
                    )
                });
            state.session_name = session.session_name.clone();
            state.last_output_preview = output.clone();

            // Stale Action Cards: an agent that is no longer blocked needs no operator attention.
            if !session.is_blocked() && state.pending_operator_action.is_some() {
                state.pending_operator_action = None;
            }

            let hash_changed = current_hash != state.last_output_hash;
            if hash_changed {
                state.last_output_hash = current_hash.clone();
                state.last_change_timestamp = now;
            }

            // Read-only fast path: trivial read-only requests are approved by a fixed rule,
            // without the LLM and outside the per-tick evaluation cap.
            if session.is_blocked() && current_hash != state.last_evaluated_hash {
                if let Some((payload, cmd)) = readonly_fast_path_with_msg(
                    &output,
                    session.kind.as_deref(),
                    session.msg.as_deref(),
                ) {
                    state.last_hc_status = session.status.clone();
                    let raw = CognitiveEvaluation {
                        updated_summary: format!(
                            "Comando somente leitura aprovado pela regra fixa: {cmd}"
                        ),
                        detected_state: LifecycleState::WaitingConfirmation,
                        decision: SupervisorDecision::AutoReply,
                        urgency: Urgency::Low,
                        hc_command_payload: payload,
                        reason_for_operator: String::new(),
                    };
                    let eval = validate_evaluation(state, self.store.mode, raw);
                    self.apply_decision(
                        session,
                        &current_hash,
                        eval,
                        AutoReplySource::FixedRule,
                        now,
                        &mut outcome,
                    );
                    continue;
                }
            }

            if !llm_available {
                continue;
            }

            let status_transition =
                session.status != state.last_hc_status && session.has_notable_hc_status();

            // Already evaluated this exact output and waiting on operator
            if current_hash == state.last_evaluated_hash
                && state.pending_operator_action.is_some()
                && !status_transition
            {
                state.last_hc_status = session.status.clone();
                continue;
            }

            let inactivity_trigger = !hash_changed
                && now.saturating_sub(state.last_change_timestamp)
                    >= self.config.inactivity_trigger_secs
                && current_hash != state.last_evaluated_hash;
            let signature_trigger =
                has_error_or_loop_signature(&output) && current_hash != state.last_evaluated_hash;
            let status_trigger = status_transition
                && (current_hash != state.last_evaluated_hash
                    || state.pending_operator_action.is_none());

            if !inactivity_trigger && !signature_trigger && !status_trigger {
                state.last_hc_status = session.status.clone();
                continue;
            }
            if evals_started >= eval_cap {
                // Deferred to a later tick; keep the status transition pending.
                continue;
            }
            evals_started += 1;
            state.last_hc_status = session.status.clone();

            let state_snapshot = state.clone();
            self.runtime_status = SupervisorRuntimeStatus::Evaluating(session.agent_id.clone());

            let eval_result = match &fb_backend {
                Some(b) => b.evaluate(&state_snapshot, &output),
                None => self.cognitive.evaluate(&state_snapshot, &output),
            };
            let raw_eval = match eval_result {
                Ok(eval) => eval,
                Err(e) => {
                    let msg: String = format!("{e:#}").chars().take(300).collect();
                    tracing::warn!(
                        agent_id = %session.agent_id,
                        error = %msg,
                        "LLM evaluation failed"
                    );
                    if let Some(state) = self.store.agents.get_mut(&session.agent_id) {
                        state.last_eval_error = Some(msg);
                        state.last_eval_error_at = Some(now);
                    }
                    continue;
                }
            };
            if fb_backend.is_none() {
                self.model_loaded = true;
            }
            self.last_eval_model = Some(
                fb_backend
                    .as_ref()
                    .map(|b| b.model_label())
                    .unwrap_or_else(|| self.cognitive.model_label()),
            );
            let llm_decision = raw_eval.decision;
            let mut eval = validate_evaluation(&state_snapshot, self.store.mode, raw_eval);
            eval = enforce_blocked_policy(session.is_blocked(), &output, eval);
            eval = sanitize_auto_reply(
                &output,
                session.kind.as_deref(),
                session.msg.as_deref(),
                eval,
            );
            eval = tidy_operator_reason(llm_decision, eval);
            outcome.evaluated_agents.push(session.agent_id.clone());

            if let Some(state) = self.store.agents.get_mut(&session.agent_id) {
                state.last_eval_error = None;
                state.last_eval_error_at = None;
            }
            self.apply_decision(
                session,
                &current_hash,
                eval,
                AutoReplySource::Llm,
                now,
                &mut outcome,
            );
        }

        self.runtime_status = SupervisorRuntimeStatus::Watching(sessions.len());
        self.store.zero_active_since = self.zero_active_since;
        self.store.model_loaded = self.model_loaded;
        self.store.save()?;
        Ok(outcome)
    }

    /// Records an evaluation (LLM or fixed rule) for a session and performs its side effects:
    /// AUTO_REPLY types the payload, ALERT_OPERATOR raises an Action Card, then hc labels are set.
    fn apply_decision(
        &mut self,
        session: &HcSession,
        current_hash: &str,
        eval: CognitiveEvaluation,
        source: AutoReplySource,
        now: u64,
        outcome: &mut TickOutcome,
    ) {
        let mut decision_label = eval.decision.as_str();
        let mut audit_entry: Option<DurableAutoReplyEntry> = None;

        let lifecycle_state_str = {
            let state = self
                .store
                .agents
                .get_mut(&session.agent_id)
                .expect("agent state must exist");
            state.rolling_summary = eval.updated_summary.clone();
            state.lifecycle_state = eval.detected_state;
            state.last_evaluated_hash = current_hash.to_string();

            match eval.decision {
                SupervisorDecision::NoOp => {}
                SupervisorDecision::AutoReply => {
                    match self
                        .hc
                        .send_input(&session.agent_id, &eval.hc_command_payload, true)
                    {
                        Ok(()) => {
                            let _ = self.hc.set_labels(
                                &session.agent_id,
                                &[
                                    ("cockpit.state", "working"),
                                    ("cockpit.kind", ""),
                                    ("cockpit.msg", ""),
                                ],
                            );
                            state.consecutive_auto_replies += 1;
                            state.last_auto_action = Some(eval.hc_command_payload.clone());
                            state.pending_operator_action = None;
                            state.last_eval_error = None;
                            state.last_eval_error_at = None;
                            state.auto_reply_log.push(AutoReplyAuditEntry {
                                timestamp: now,
                                payload: eval.hc_command_payload.clone(),
                                summary: eval.updated_summary.clone(),
                            });
                            audit_entry = Some(DurableAutoReplyEntry {
                                timestamp: now,
                                agent_id: session.agent_id.clone(),
                                session_name: session.session_name.clone(),
                                payload: eval.hc_command_payload.clone(),
                                source,
                                summary: eval.updated_summary.clone(),
                                model: match source {
                                    AutoReplySource::Llm => {
                                        self.last_eval_model.clone().filter(|m| !m.is_empty())
                                    }
                                    AutoReplySource::FixedRule => None,
                                },
                            });
                            outcome.auto_replied_agents.push(session.agent_id.clone());
                        }
                        Err(e) => {
                            let err_msg: String = format!("{e:#}").chars().take(300).collect();
                            tracing::warn!(
                                agent_id = %session.agent_id,
                                error = %err_msg,
                                "hc send_input failed"
                            );
                            state.last_eval_error = Some(format!("hc send falhou: {err_msg}"));
                            state.last_eval_error_at = Some(now);
                            decision_label = "AUTO_REPLY_FAILED";
                        }
                    }
                }
                SupervisorDecision::AlertOperator => {
                    state.pending_operator_action = Some(PendingOperatorAction {
                        urgency: eval.urgency,
                        detected_state: eval.detected_state,
                        reason_for_operator: eval.reason_for_operator.clone(),
                        updated_summary: eval.updated_summary.clone(),
                        hc_command_payload: eval.hc_command_payload.clone(),
                        triggered_at: now,
                    });
                    outcome.alerted_agents.push(session.agent_id.clone());
                    if self.config.enable_desktop_notifications
                        && matches!(eval.urgency, Urgency::Normal | Urgency::Critical)
                    {
                        send_desktop_notification(&session.agent_id, &eval.reason_for_operator);
                    }
                }
            }

            state.lifecycle_state.as_str()
        };

        if let Some(entry) = audit_entry {
            if let Err(e) = self.store.append_auto_reply_log(&entry) {
                tracing::warn!(
                    agent_id = %session.agent_id,
                    error = %format!("{e:#}"),
                    "Failed to append auto-reply audit log"
                );
            }
        }

        let _ = self.hc.set_labels(
            &session.agent_id,
            &[
                ("cockpit.supervisor_state", lifecycle_state_str),
                ("cockpit.supervisor_decision", decision_label),
                ("supervisor.state", lifecycle_state_str),
                ("supervisor.decision", decision_label),
            ],
        );
    }

    /// Operator clicked `[Aprovar e Enviar]` on an Action Card.
    pub fn approve_operator_action(
        &mut self,
        agent_id: &str,
        command_payload: &str,
        now: u64,
    ) -> Result<()> {
        if !command_payload.trim().is_empty() {
            self.hc.send_input(agent_id, command_payload, true)?;
        }
        let _ = self.hc.set_labels(
            agent_id,
            &[
                ("cockpit.state", "working"),
                ("cockpit.kind", ""),
                ("cockpit.msg", ""),
            ],
        );
        if let Some(agent) = self.store.agents.get_mut(agent_id) {
            agent.reset_operator_counter();
            agent.last_change_timestamp = now;
        }
        self.store.save()?;
        Ok(())
    }

    /// Operator clicked `[Ignorar]` on an Action Card.
    pub fn ignore_operator_action(&mut self, agent_id: &str) -> Result<()> {
        let _ = self.hc.set_labels(
            agent_id,
            &[
                ("cockpit.state", "working"),
                ("cockpit.kind", ""),
                ("cockpit.msg", ""),
            ],
        );
        if let Some(agent) = self.store.agents.get_mut(agent_id) {
            agent.ignore_alert();
        }
        self.store.save()?;
        Ok(())
    }

    /// Cycles supervisor operating mode (`SemiAuto` -> `AlertOnly` -> `Paused`) and persists.
    pub fn cycle_mode(&mut self) -> Result<SupervisorMode> {
        self.store.mode = self.store.mode.next();
        self.store.save()?;
        Ok(self.store.mode)
    }

    /// Sets supervisor operating mode explicitly and persists.
    pub fn set_mode(&mut self, mode: SupervisorMode) -> Result<()> {
        self.store.mode = mode;
        self.store.save()?;
        Ok(())
    }

    /// Explicitly unloads the local LLM on shutdown.
    pub fn shutdown_unload(&mut self) -> Result<()> {
        self.cognitive.unload_model()?;
        self.model_loaded = false;
        self.runtime_status = SupervisorRuntimeStatus::Idle;
        self.store.zero_active_since = self.zero_active_since;
        self.store.model_loaded = self.model_loaded;
        let _ = self.store.save();
        Ok(())
    }
}

fn send_desktop_notification(agent_id: &str, reason: &str) {
    let summary = format!("Cockpit Supervisor [{agent_id}]");
    let _ = Command::new("notify-send")
        .args([summary.as_str(), reason])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::hc::HcSession;
    use crate::supervisor::ollama::CognitiveEvaluation;
    use crate::supervisor::state::LifecycleState;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tempfile::tempdir;

    struct MockHc {
        sessions: Mutex<Vec<HcSession>>,
        outputs: Mutex<VecDeque<String>>,
        sent_inputs: Mutex<Vec<(String, String, bool)>>,
        fail_send_input: Mutex<Option<String>>,
        labels: Mutex<Vec<(String, Vec<(String, String)>)>>,
    }

    impl MockHc {
        fn new(sessions: Vec<HcSession>, outputs: Vec<&str>) -> Self {
            Self {
                sessions: Mutex::new(sessions),
                outputs: Mutex::new(outputs.into_iter().map(String::from).collect()),
                sent_inputs: Mutex::new(Vec::new()),
                fail_send_input: Mutex::new(None),
                labels: Mutex::new(Vec::new()),
            }
        }
    }

    impl HcBackend for MockHc {
        fn list_sessions(&self) -> Result<Vec<HcSession>> {
            Ok(self.sessions.lock().unwrap().clone())
        }

        fn capture_output(&self, _agent_id: &str, _lines: usize) -> Result<String> {
            let mut outputs = self.outputs.lock().unwrap();
            if outputs.len() > 1 {
                Ok(outputs.pop_front().unwrap())
            } else {
                Ok(outputs.front().cloned().unwrap_or_default())
            }
        }

        fn send_input(&self, agent_id: &str, payload: &str, force: bool) -> Result<()> {
            if let Some(err) = self.fail_send_input.lock().unwrap().as_ref() {
                anyhow::bail!("{err}");
            }
            self.sent_inputs.lock().unwrap().push((
                agent_id.to_string(),
                payload.to_string(),
                force,
            ));
            Ok(())
        }

        fn send_keys(&self, _agent_id: &str, _key: &str) -> Result<()> {
            Ok(())
        }

        fn set_labels(&self, agent_id: &str, labels: &[(&str, &str)]) -> Result<()> {
            let pairs = labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            self.labels
                .lock()
                .unwrap()
                .push((agent_id.to_string(), pairs));
            Ok(())
        }
    }

    struct MockCognitive {
        eval_calls: Mutex<usize>,
        unload_calls: Mutex<usize>,
        available: Mutex<bool>,
        next_eval: Mutex<CognitiveEvaluation>,
        next_error: Mutex<Option<String>>,
    }

    impl MockCognitive {
        fn new(next_eval: CognitiveEvaluation) -> Self {
            Self {
                eval_calls: Mutex::new(0),
                unload_calls: Mutex::new(0),
                available: Mutex::new(true),
                next_eval: Mutex::new(next_eval),
                next_error: Mutex::new(None),
            }
        }
    }

    impl CognitiveBackend for MockCognitive {
        fn evaluate(
            &self,
            _state: &AgentDurableState,
            _recent_output: &str,
        ) -> Result<CognitiveEvaluation> {
            *self.eval_calls.lock().unwrap() += 1;
            if let Some(err) = self.next_error.lock().unwrap().as_ref() {
                anyhow::bail!("{err}");
            }
            Ok(self.next_eval.lock().unwrap().clone())
        }

        fn unload_model(&self) -> Result<()> {
            *self.unload_calls.lock().unwrap() += 1;
            Ok(())
        }

        fn is_available(&self) -> bool {
            *self.available.lock().unwrap()
        }
    }

    fn test_session(id: &str) -> HcSession {
        HcSession {
            agent_id: id.to_string(),
            session_name: "refactor-auth".to_string(),
            agent_program: Some("pi".to_string()),
            cwd: Some("/tmp".to_string()),
            state: "live".to_string(),
            status: Some("blocked".to_string()),
            turn: Some(1),
            kind: None,
            msg: None,
        }
    }

    fn working_session(id: &str) -> HcSession {
        let mut s = test_session(id);
        s.status = Some("working".to_string());
        s
    }

    fn test_config(dir: &tempfile::TempDir) -> SupervisorConfig {
        let mut cfg = SupervisorConfig::default();
        cfg.state_file_path = dir.path().join("supervisor_state.json");
        cfg.enable_desktop_notifications = false;
        cfg
    }

    #[test]
    fn test_normal_streaming_output_uses_zero_llm_calls() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![working_session("worker-01")],
            vec![
                "Compiling step 1...",
                "Compiling step 2...",
                "Compiling step 3...",
            ],
        );
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            updated_summary: "Compilando".to_string(),
            detected_state: LifecycleState::Working,
            decision: SupervisorDecision::NoOp,
            urgency: Urgency::Low,
            hc_command_payload: String::new(),
            reason_for_operator: String::new(),
        });

        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let o1 = engine.tick(1000).unwrap();
        let o2 = engine.tick(1012).unwrap();
        let o3 = engine.tick(1024).unwrap();

        assert!(o1.evaluated_agents.is_empty());
        assert!(o2.evaluated_agents.is_empty());
        assert!(o3.evaluated_agents.is_empty());
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
        assert_eq!(engine.runtime_status, SupervisorRuntimeStatus::Watching(1));
    }

    #[test]
    fn test_stalled_prompt_triggers_auto_reply_and_increments_counter() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let mut store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let mut agent = AgentDurableState::new("worker-01", "refactor-auth", 2, 1000);
        agent.last_hc_status = Some("blocked".to_string());
        store.agents.insert("worker-01".to_string(), agent);
        let hc = MockHc::new(
            vec![test_session("worker-01")],
            vec!["Waiting for input...", "Waiting for input..."],
        );
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            updated_summary: "Agente aguardando comando para rodar testes.".to_string(),
            detected_state: LifecycleState::WaitingConfirmation,
            decision: SupervisorDecision::AutoReply,
            urgency: Urgency::Low,
            hc_command_payload: "cargo test".to_string(),
            reason_for_operator: String::new(),
        });

        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let o1 = engine.tick(1000).unwrap();
        assert!(o1.evaluated_agents.is_empty());

        // > 20 seconds unchanged triggers inactivity evaluation. Free text ("cargo test") is never
        // typed by AUTO_REPLY: it becomes an operator alert without payload.
        let o2 = engine.tick(1021).unwrap();
        assert_eq!(o2.evaluated_agents, vec!["worker-01"]);
        assert!(o2.auto_replied_agents.is_empty());
        assert_eq!(o2.alerted_agents, vec!["worker-01"]);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 1);
        assert!(engine.hc.sent_inputs.lock().unwrap().is_empty());
        let labels = engine.hc.labels.lock().unwrap();
        assert!(labels.last().unwrap().1.contains(&(
            "supervisor.state".to_string(),
            "WAITING_CONFIRMATION".to_string()
        )));

        let agent = engine.store.agents.get("worker-01").unwrap();
        assert_eq!(agent.consecutive_auto_replies, 0);
        let card = agent.pending_operator_action.as_ref().unwrap();
        assert!(card.hc_command_payload.is_empty());
    }

    #[test]
    fn test_blocked_yn_prompt_echoed_hint_sends_y() {
        // Test 2 / safe-ls: lfm3 answered AUTO_REPLY with the literal "[y/N]".
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t11m")],
            vec!["Bash command\n  ls -la src/\nAllow? [y/N]"],
        );
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            decision: SupervisorDecision::AutoReply,
            ..alert_eval(Urgency::Low, "[y/N]")
        });
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let o = engine.tick(1000).unwrap();
        assert_eq!(o.auto_replied_agents, vec!["t11m"]);
        assert_eq!(
            engine.hc.sent_inputs.lock().unwrap().as_slice(),
            &[("t11m".to_string(), "y".to_string(), true)]
        );
    }

    #[test]
    fn test_loop_breaker_downgrades_to_alert_and_operator_approve_resets() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let mut store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let mut agent = AgentDurableState::new("worker-01", "refactor-auth", 2, 900);
        agent.consecutive_auto_replies = 2;
        store.agents.insert("worker-01".to_string(), agent);

        let hc = MockHc::new(
            vec![test_session("worker-01")],
            vec!["Do you want to continue? [y/N]"],
        );
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            updated_summary: "Agente em loop de confirmação.".to_string(),
            detected_state: LifecycleState::StuckLoop,
            decision: SupervisorDecision::AutoReply,
            urgency: Urgency::Normal,
            hc_command_payload: "y".to_string(),
            reason_for_operator: "Solicitou confirmação novamente.".to_string(),
        });

        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let outcome = engine.tick(1000).unwrap();
        assert_eq!(outcome.evaluated_agents, vec!["worker-01"]);
        assert!(outcome.auto_replied_agents.is_empty());
        assert_eq!(outcome.alerted_agents, vec!["worker-01"]);

        let agent = engine.store.agents.get("worker-01").unwrap();
        assert!(agent.pending_operator_action.is_some());
        assert!(agent
            .pending_operator_action
            .as_ref()
            .unwrap()
            .reason_for_operator
            .contains("[Limite de auto-resposta atingido]"));

        // Operator approves the action card
        engine
            .approve_operator_action("worker-01", "y", 1005)
            .unwrap();
        let agent = engine.store.agents.get("worker-01").unwrap();
        assert_eq!(agent.consecutive_auto_replies, 0);
        assert!(agent.pending_operator_action.is_none());
        assert_eq!(
            engine.hc.sent_inputs.lock().unwrap().last().unwrap(),
            &("worker-01".to_string(), "y".to_string(), true)
        );
    }

    #[test]
    fn test_zero_idle_unloads_model_after_180_seconds() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(vec![], vec![]);
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            updated_summary: String::new(),
            detected_state: LifecycleState::Working,
            decision: SupervisorDecision::NoOp,
            urgency: Urgency::Low,
            hc_command_payload: String::new(),
            reason_for_operator: String::new(),
        });

        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        engine.model_loaded = true;

        let o1 = engine.tick(1000).unwrap();
        assert!(!o1.model_unloaded);
        assert_eq!(engine.runtime_status, SupervisorRuntimeStatus::Watching(0));

        let o2 = engine.tick(1180).unwrap();
        assert!(o2.model_unloaded);
        assert!(!engine.model_loaded);
        assert_eq!(*engine.cognitive.unload_calls.lock().unwrap(), 1);
        assert_eq!(engine.runtime_status, SupervisorRuntimeStatus::Idle);
    }

    fn alert_eval(urgency: Urgency, payload: &str) -> CognitiveEvaluation {
        CognitiveEvaluation {
            updated_summary: "Agente t10b aguardando background agent.".to_string(),
            detected_state: LifecycleState::Error,
            decision: SupervisorDecision::AlertOperator,
            urgency,
            hc_command_payload: payload.to_string(),
            reason_for_operator: "HooksError repetido.".to_string(),
        }
    }

    #[test]
    fn test_working_agent_with_hooks_error_creates_no_card() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![working_session("t10b")],
            vec!["HooksError: git process aborted after 2000ms\nerror[E0308]: noise\nWaiting for background agent"],
        );
        let cognitive = MockCognitive::new(alert_eval(Urgency::Critical, "n"));
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let o = engine.tick(1000).unwrap();
        assert_eq!(o.evaluated_agents, vec!["t10b"]);
        assert!(o.alerted_agents.is_empty());
        let agent = engine.store.agents.get("t10b").unwrap();
        assert!(agent.pending_operator_action.is_none());
        assert_eq!(
            agent.rolling_summary,
            "Agente t10b aguardando background agent."
        );
    }

    #[test]
    fn test_blocked_force_push_prompt_raises_critical_card() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Bash: git push --force origin main\nProceed? (y/n)"],
        );
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            decision: SupervisorDecision::AutoReply,
            urgency: Urgency::Low,
            hc_command_payload: "y".to_string(),
            ..alert_eval(Urgency::Low, "y")
        });
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let o = engine.tick(1000).unwrap();
        assert!(o.auto_replied_agents.is_empty());
        let card = engine.store.agents["t1"]
            .pending_operator_action
            .clone()
            .unwrap();
        assert_eq!(card.urgency, Urgency::Critical);
        assert_eq!(card.hc_command_payload, "n");
        assert!(engine.hc.sent_inputs.lock().unwrap().is_empty());
    }

    #[test]
    fn test_blocked_safe_prompt_auto_replies_y() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Run cargo test?\nProceed? (y/n)"],
        );
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            decision: SupervisorDecision::AutoReply,
            urgency: Urgency::Low,
            hc_command_payload: "y".to_string(),
            ..alert_eval(Urgency::Low, "y")
        });
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let o = engine.tick(1000).unwrap();
        assert_eq!(o.auto_replied_agents, vec!["t1"]);
        assert_eq!(
            engine.hc.sent_inputs.lock().unwrap().as_slice(),
            &[("t1".to_string(), "y".to_string(), true)]
        );
    }

    #[test]
    fn test_stale_action_card_cleared_when_agent_no_longer_blocked() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let mut store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let mut agent = AgentDurableState::new("t1", "refactor-auth", 2, 900);
        agent.pending_operator_action = Some(PendingOperatorAction {
            urgency: Urgency::Critical,
            detected_state: LifecycleState::Error,
            reason_for_operator: "x".into(),
            updated_summary: "x".into(),
            hc_command_payload: "n".into(),
            triggered_at: 900,
        });
        store.agents.insert("t1".to_string(), agent);
        let hc = MockHc::new(vec![working_session("t1")], vec!["Compiling..."]);
        let cognitive = MockCognitive::new(alert_eval(Urgency::Low, ""));
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        engine.tick(1000).unwrap();
        assert!(engine.store.agents["t1"].pending_operator_action.is_none());
    }

    #[test]
    fn test_watcher_skips_evaluate_when_llm_unavailable() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Allow read-only inspection? (y/n)"],
        );
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            decision: SupervisorDecision::AutoReply,
            ..alert_eval(Urgency::Low, "y")
        });
        *cognitive.available.lock().unwrap() = false;
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);

        // Tick 1 while unavailable: no evaluate, preview refreshed, last_evaluated_hash stays empty
        let o1 = engine.tick(1000).unwrap();
        assert_eq!(o1.active_sessions, 1);
        assert!(o1.evaluated_agents.is_empty());
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
        assert_eq!(
            engine.store.agents["t1"].last_output_preview,
            "Allow read-only inspection? (y/n)"
        );
        assert_eq!(
            engine.store.agents["t1"].last_evaluated_hash,
            compute_sha256("")
        );

        // User loads LLM -> available becomes true -> next tick evaluates immediately
        *engine.cognitive.available.lock().unwrap() = true;
        let o2 = engine.tick(1005).unwrap();
        assert_eq!(o2.evaluated_agents, vec!["t1"]);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 1);
    }

    #[test]
    fn eval_error_is_recorded_and_cleared() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Allow read-only inspection? (y/n)"],
        );
        let cognitive = MockCognitive::new(alert_eval(Urgency::Normal, "y"));
        *cognitive.next_error.lock().unwrap() = Some("kv cache full".to_string());
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);

        // Tick 1: evaluate fails -> error recorded, hash not marked evaluated, runtime_status not Evaluating
        let o1 = engine.tick(1000).unwrap();
        assert!(o1.evaluated_agents.is_empty());
        assert_eq!(
            engine.store.agents["t1"].last_eval_error.as_deref(),
            Some("kv cache full")
        );
        assert_eq!(engine.store.agents["t1"].last_eval_error_at, Some(1000));
        assert_eq!(
            engine.store.agents["t1"].last_evaluated_hash,
            compute_sha256("")
        );
        assert_eq!(engine.runtime_status, SupervisorRuntimeStatus::Watching(1));

        // Tick 2: evaluate succeeds -> error cleared
        *engine.cognitive.next_error.lock().unwrap() = None;
        let o2 = engine.tick(1005).unwrap();
        assert_eq!(o2.evaluated_agents, vec!["t1"]);
        assert_eq!(engine.store.agents["t1"].last_eval_error, None);
        assert_eq!(engine.store.agents["t1"].last_eval_error_at, None);
    }

    fn noop_eval() -> CognitiveEvaluation {
        CognitiveEvaluation {
            decision: SupervisorDecision::NoOp,
            ..alert_eval(Urgency::Low, "")
        }
    }

    #[test]
    fn blocked_sessions_are_evaluated_before_working_ones() {
        // Test 2: 9 blocked terminals waited behind working agents in `hc ls` order.
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir); // max_evals_per_tick defaults to 1
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![working_session("w1"), test_session("b1")],
            vec!["thread 'main' panicked at x\nAllow? [y/N]"],
        );
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));
        assert_eq!(engine.tick(1000).unwrap().evaluated_agents, vec!["b1"]);
        assert_eq!(engine.tick(1012).unwrap().evaluated_agents, vec!["w1"]);
    }

    #[test]
    fn eval_cap_defers_without_losing_the_blocked_transition() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("b1"), test_session("b2"), test_session("b3")],
            vec!["Read README.md? (y/n)"],
        );
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));
        assert_eq!(engine.tick(1000).unwrap().evaluated_agents, vec!["b1"]);
        assert_eq!(engine.tick(1012).unwrap().evaluated_agents, vec!["b2"]);
        assert_eq!(engine.tick(1024).unwrap().evaluated_agents, vec!["b3"]);
        assert!(engine.tick(1036).unwrap().evaluated_agents.is_empty());
    }

    #[test]
    fn eval_cap_is_configurable() {
        let dir = tempdir().unwrap();
        let mut cfg = test_config(&dir);
        cfg.max_evals_per_tick = 2;
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("b1"), test_session("b2"), test_session("b3")],
            vec!["Read README.md? (y/n)"],
        );
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));
        assert_eq!(
            engine.tick(1000).unwrap().evaluated_agents,
            vec!["b1", "b2"]
        );
        assert_eq!(engine.tick(1012).unwrap().evaluated_agents, vec!["b3"]);
    }

    const SAFE_LS: &str = "Bash command\n  ls -la src/\nAllow? [y/N]";

    #[test]
    fn readonly_fast_path_replies_without_the_llm() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(vec![test_session("t1")], vec![SAFE_LS]);
        let cognitive = MockCognitive::new(noop_eval());
        *cognitive.available.lock().unwrap() = false; // works even with the LLM down
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);

        let o = engine.tick(1000).unwrap();
        assert_eq!(o.auto_replied_agents, vec!["t1"]);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
        assert_eq!(
            engine.hc.sent_inputs.lock().unwrap().as_slice(),
            &[("t1".to_string(), "y".to_string(), true)]
        );
        // Same screen on the next tick: not answered twice.
        engine.tick(1012).unwrap();
        assert_eq!(engine.hc.sent_inputs.lock().unwrap().len(), 1);
    }

    #[test]
    fn readonly_fast_path_respects_alert_only_mode() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let mut store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        store.mode = SupervisorMode::AlertOnly;
        let hc = MockHc::new(vec![test_session("t1")], vec![SAFE_LS]);
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));
        let o = engine.tick(1000).unwrap();
        assert!(engine.hc.sent_inputs.lock().unwrap().is_empty());
        assert_eq!(o.alerted_agents, vec!["t1"]);
    }

    #[test]
    fn readonly_fast_path_is_not_limited_by_the_eval_cap() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir); // max_evals_per_tick = 1
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(vec![test_session("a"), test_session("b")], vec![SAFE_LS]);
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));
        let o = engine.tick(1000).unwrap();
        assert_eq!(o.auto_replied_agents, vec!["a", "b"]);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
    }

    #[test]
    fn readonly_fast_path_uses_hc_status_msg_on_a_claude_menu() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let mut session = test_session("t14c");
        session.kind = Some("permission".to_string());
        session.msg = Some("approve Bash: git diff --stat".to_string());
        let screen = " Bash command\n\n git diff --stat\n Show changes\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and always allow access\n   3. Yes, and switch to auto mode\n   4. No\n\n Esc to cancel · Tab to amend";
        let hc = MockHc::new(vec![session], vec![screen]);
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));
        let o = engine.tick(1000).unwrap();
        assert_eq!(o.auto_replied_agents, vec!["t14c"]);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
        assert_eq!(
            engine.hc.sent_inputs.lock().unwrap().as_slice(),
            &[("t14c".to_string(), "1".to_string(), true)]
        );
    }

    #[test]
    fn llm_cannot_auto_reply_to_claude_permission_menu() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let mut session = test_session("t11g");
        session.kind = Some("permission".to_string());
        session.msg = Some("approve Bash: ls -la src && find . -name '*.rs'".to_string());
        let screen = " Bash command\n\n  ls -la src && find . -name '*.rs'\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again\n   3. No";
        let hc = MockHc::new(vec![session], vec![screen]);
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            decision: SupervisorDecision::AutoReply,
            urgency: Urgency::Low,
            hc_command_payload: "y".to_string(),
            ..alert_eval(Urgency::Low, "y")
        });
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);

        let o = engine.tick(1000).unwrap();
        assert_eq!(o.evaluated_agents, vec!["t11g"]);
        assert!(o.auto_replied_agents.is_empty());
        assert_eq!(o.alerted_agents, vec!["t11g"]);
        assert!(engine.hc.sent_inputs.lock().unwrap().is_empty());
        assert!(!dir.path().join("auto_replies.jsonl").exists());

        let card = engine.store.agents["t11g"]
            .pending_operator_action
            .as_ref()
            .unwrap();
        assert!(card.hc_command_payload.is_empty());
        assert_eq!(card.urgency, Urgency::Normal);
    }

    #[test]
    fn auto_replies_jsonl_records_both_paths_and_survives_agent_pruning() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();

        // Tick 1: fast path replies to t1
        let hc = MockHc::new(vec![test_session("t1")], vec![SAFE_LS]);
        let cognitive = MockCognitive::new(CognitiveEvaluation {
            updated_summary: "Continuando execução.".to_string(),
            detected_state: LifecycleState::WaitingConfirmation,
            decision: SupervisorDecision::AutoReply,
            urgency: Urgency::Low,
            hc_command_payload: "y".to_string(),
            reason_for_operator: String::new(),
        });
        let mut engine = WatcherEngine::new(cfg, store, hc, cognitive);
        let o1 = engine.tick(1000).unwrap();
        assert_eq!(o1.auto_replied_agents, vec!["t1"]);

        // Tick 2: LLM path replies to t2
        *engine.hc.sessions.lock().unwrap() = vec![test_session("t2")];
        *engine.hc.outputs.lock().unwrap() = VecDeque::from(vec!["Continue? [y/N]".to_string()]);
        let o2 = engine.tick(1010).unwrap();
        assert_eq!(o2.auto_replied_agents, vec!["t2"]);

        // Tick 3: all terminals closed -> agents pruned from state store
        *engine.hc.sessions.lock().unwrap() = vec![];
        engine.tick(1020).unwrap();
        assert!(engine.store.agents.is_empty());

        let log_path = dir.path().join("auto_replies.jsonl");
        let raw = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["timestamp"], 1000);
        assert_eq!(lines[0]["agent_id"], "t1");
        assert_eq!(lines[0]["session_name"], "refactor-auth");
        assert_eq!(lines[0]["payload"], "y");
        assert_eq!(lines[0]["source"], "fixed_rule");
        assert!(lines[0]["summary"]
            .as_str()
            .unwrap()
            .contains("ls -la src/"));

        assert_eq!(lines[1]["timestamp"], 1010);
        assert_eq!(lines[1]["agent_id"], "t2");
        assert_eq!(lines[1]["session_name"], "refactor-auth");
        assert_eq!(lines[1]["payload"], "y");
        assert_eq!(lines[1]["source"], "llm");
        assert_eq!(lines[1]["summary"], "Continuando execução.");
    }

    #[test]
    fn failed_hc_send_records_error_and_sets_auto_reply_failed_label() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(vec![test_session("t1")], vec![SAFE_LS]);
        *hc.fail_send_input.lock().unwrap() = Some("tmux pane not found".to_string());
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));

        let o = engine.tick(1000).unwrap();
        assert!(o.auto_replied_agents.is_empty());
        assert!(!dir.path().join("auto_replies.jsonl").exists());

        let agent = &engine.store.agents["t1"];
        assert_eq!(agent.consecutive_auto_replies, 0);
        assert!(agent.auto_reply_log.is_empty());
        assert!(agent.last_auto_action.is_none());
        assert_eq!(
            agent.last_eval_error.as_deref(),
            Some("hc send falhou: tmux pane not found")
        );
        assert_eq!(agent.last_eval_error_at, Some(1000));

        let labels = engine.hc.labels.lock().unwrap();
        let last_labels = &labels.last().unwrap().1;
        assert!(last_labels.contains(&(
            "cockpit.supervisor_decision".to_string(),
            "AUTO_REPLY_FAILED".to_string()
        )));
        assert!(last_labels.contains(&(
            "supervisor.decision".to_string(),
            "AUTO_REPLY_FAILED".to_string()
        )));
        assert!(!labels.iter().any(|(_, pairs)| pairs
            .iter()
            .any(|(k, v)| k == "cockpit.state" && v == "working")));
        drop(labels);

        // Error messages longer than 300 chars are truncated in last_eval_error.
        *engine.hc.fail_send_input.lock().unwrap() = Some("x".repeat(400));
        *engine.hc.outputs.lock().unwrap() =
            VecDeque::from(vec!["Bash command\n  pwd\nAllow? [y/N]".to_string()]);
        engine.tick(1005).unwrap();
        let err = engine.store.agents["t1"]
            .last_eval_error
            .as_deref()
            .unwrap();
        assert_eq!(
            err.strip_prefix("hc send falhou: ")
                .unwrap()
                .chars()
                .count(),
            300
        );
    }

    #[test]
    fn audit_log_write_failure_does_not_stop_reply() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        // Make auto_replies.jsonl a directory so opening it for append fails.
        std::fs::create_dir_all(dir.path().join("auto_replies.jsonl")).unwrap();

        let hc = MockHc::new(vec![test_session("t1")], vec![SAFE_LS]);
        let mut engine = WatcherEngine::new(cfg, store, hc, MockCognitive::new(noop_eval()));

        let o = engine.tick(1000).unwrap();
        assert_eq!(o.auto_replied_agents, vec!["t1"]);
        assert_eq!(
            engine.hc.sent_inputs.lock().unwrap().as_slice(),
            &[("t1".to_string(), "y".to_string(), true)]
        );
        assert_eq!(engine.store.agents["t1"].consecutive_auto_replies, 1);
    }

    struct SharedFakeCognitive {
        eval_calls: std::sync::Arc<Mutex<usize>>,
        model: String,
        next_eval: CognitiveEvaluation,
        next_error: Option<String>,
    }

    impl CognitiveBackend for SharedFakeCognitive {
        fn evaluate(
            &self,
            _state: &AgentDurableState,
            _recent_output: &str,
        ) -> Result<CognitiveEvaluation> {
            *self.eval_calls.lock().unwrap() += 1;
            if let Some(err) = &self.next_error {
                anyhow::bail!("{err}");
            }
            Ok(self.next_eval.clone())
        }

        fn unload_model(&self) -> Result<()> {
            Ok(())
        }

        fn model_label(&self) -> String {
            self.model.clone()
        }
    }

    struct FakeFallback {
        state_path: Option<std::path::PathBuf>,
        state: Mutex<crate::supervisor::fallback::FallbackState>,
        healthy: Mutex<bool>,
        probe_calls: std::sync::Arc<Mutex<usize>>,
        fb_eval_calls: std::sync::Arc<Mutex<usize>>,
        backend_enabled: Mutex<bool>,
        backend_model: String,
        backend_eval: CognitiveEvaluation,
        backend_error: Option<String>,
        redact_secret: Option<String>,
    }

    impl FakeFallback {
        fn new(
            state: crate::supervisor::fallback::FallbackState,
            healthy: bool,
            eval: CognitiveEvaluation,
        ) -> Self {
            Self {
                state_path: None,
                state: Mutex::new(state),
                healthy: Mutex::new(healthy),
                probe_calls: std::sync::Arc::new(Mutex::new(0)),
                fb_eval_calls: std::sync::Arc::new(Mutex::new(0)),
                backend_enabled: Mutex::new(true),
                backend_model: "openai/gpt-4.1-mini".to_string(),
                backend_eval: eval,
                backend_error: None,
                redact_secret: None,
            }
        }
    }

    impl crate::supervisor::fallback::FallbackProvider for FakeFallback {
        fn load_state(&self) -> crate::supervisor::fallback::FallbackState {
            if let Some(p) = &self.state_path {
                crate::supervisor::fallback::load_state(p)
            } else {
                self.state.lock().unwrap().clone()
            }
        }

        fn update_state(
            &self,
            f: &mut dyn FnMut(&mut crate::supervisor::fallback::FallbackState),
        ) -> Result<crate::supervisor::fallback::FallbackState> {
            if let Some(p) = &self.state_path {
                crate::supervisor::fallback::update_state(p, f)
            } else {
                let mut guard = self.state.lock().unwrap();
                f(&mut guard);
                Ok(guard.clone())
            }
        }

        fn probe_primary(&self) -> (bool, String) {
            *self.probe_calls.lock().unwrap() += 1;
            let ok = *self.healthy.lock().unwrap();
            if ok {
                (true, String::new())
            } else {
                (false, "/health sem resposta".to_string())
            }
        }

        fn backend(&self) -> Option<Box<dyn CognitiveBackend>> {
            if !*self.backend_enabled.lock().unwrap() {
                return None;
            }
            let inner: Box<dyn CognitiveBackend> = Box::new(SharedFakeCognitive {
                eval_calls: self.fb_eval_calls.clone(),
                model: self.backend_model.clone(),
                next_eval: self.backend_eval.clone(),
                next_error: self.backend_error.clone(),
            });
            if let Some(secret) = &self.redact_secret {
                Some(Box::new(crate::supervisor::fallback::RedactingBackend {
                    inner,
                    key: secret.clone(),
                }))
            } else {
                Some(inner)
            }
        }
    }

    fn auto_reply_eval() -> CognitiveEvaluation {
        CognitiveEvaluation {
            updated_summary: "Autorizando operação.".to_string(),
            detected_state: LifecycleState::WaitingConfirmation,
            decision: SupervisorDecision::AutoReply,
            urgency: Urgency::Low,
            hc_command_payload: "y".to_string(),
            reason_for_operator: String::new(),
        }
    }

    #[test]
    fn health_failures_switch_evaluation_to_fallback_after_two_ticks() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Delete production db? [y/N]"],
        );
        let primary = MockCognitive::new(noop_eval());
        *primary.available.lock().unwrap() = false;
        let fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState {
                primary_intended: true,
                ..Default::default()
            },
            false,
            auto_reply_eval(),
        );
        let fb_eval_calls = fb.fb_eval_calls.clone();

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));

        engine.tick(1000).unwrap();
        assert!(!engine.fallback_state().active);
        assert_eq!(*fb_eval_calls.lock().unwrap(), 0);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);

        engine.tick(1012).unwrap();
        let st = engine.fallback_state();
        assert!(st.active);
        assert_eq!(
            st.reason,
            Some(crate::supervisor::fallback::FallbackReason::Health)
        );
        assert_eq!(*fb_eval_calls.lock().unwrap(), 1);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
    }

    #[test]
    fn active_fallback_skips_probe_and_primary() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Delete production db? [y/N]"],
        );
        let primary = MockCognitive::new(noop_eval());
        let fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState {
                primary_intended: true,
                active: true,
                reason: Some(crate::supervisor::fallback::FallbackReason::Health),
                since: Some(900),
                ..Default::default()
            },
            false,
            auto_reply_eval(),
        );
        let fb_eval_calls = fb.fb_eval_calls.clone();
        let probe_calls = fb.probe_calls.clone();

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));
        engine.tick(1000).unwrap();

        assert_eq!(*fb_eval_calls.lock().unwrap(), 1);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
        assert_eq!(*probe_calls.lock().unwrap(), 0);
        let snap = engine.fallback_state();
        assert!(snap.active);
    }

    #[test]
    fn engine_reads_state_written_by_other_writer() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let state_path = dir.path().join("supervisor_fallback.json");
        crate::supervisor::fallback::update_state(&state_path, |s| {
            s.primary_intended = true;
            s.active = true;
            s.reason = Some(crate::supervisor::fallback::FallbackReason::Health);
            s.since = Some(900);
        })
        .unwrap();

        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Delete production db? [y/N]", "Another risky step? [y/N]"],
        );
        let primary = MockCognitive::new(auto_reply_eval());
        let mut fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState::default(),
            true,
            auto_reply_eval(),
        );
        fb.state_path = Some(state_path.clone());
        let fb_eval_calls = fb.fb_eval_calls.clone();

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));
        engine.tick(1000).unwrap();
        assert_eq!(*fb_eval_calls.lock().unwrap(), 1);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);

        // External process returns to primary.
        crate::supervisor::fallback::update_state(
            &state_path,
            crate::supervisor::fallback::primary_started,
        )
        .unwrap();

        engine.tick(1012).unwrap();
        assert!(!engine.fallback_state().active);
        assert_eq!(*fb_eval_calls.lock().unwrap(), 1);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 1);
    }

    #[test]
    fn active_without_key_skips_evaluation() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Delete production db? [y/N]"],
        );
        let primary = MockCognitive::new(auto_reply_eval());
        let fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState {
                primary_intended: true,
                active: true,
                reason: Some(crate::supervisor::fallback::FallbackReason::LoadFailed),
                since: Some(900),
                ..Default::default()
            },
            false,
            auto_reply_eval(),
        );
        *fb.backend_enabled.lock().unwrap() = false;
        let fb_eval_calls = fb.fb_eval_calls.clone();

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));
        engine.tick(1000).unwrap();
        assert_eq!(*fb_eval_calls.lock().unwrap(), 0);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
        assert!(engine.fallback_state().active);
    }

    #[test]
    fn zero_idle_ends_fallback() {
        let dir = tempdir().unwrap();
        let mut cfg = test_config(&dir);
        cfg.idle_unload_secs = 60;
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(vec![], vec![]);
        let primary = MockCognitive::new(noop_eval());
        let fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState {
                primary_intended: true,
                active: true,
                reason: Some(crate::supervisor::fallback::FallbackReason::Health),
                since: Some(900),
                ..Default::default()
            },
            false,
            noop_eval(),
        );

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));
        engine.tick(1000).unwrap();
        assert!(engine.fallback_state().active);

        engine.tick(1060).unwrap();
        let st = engine.fallback_state();
        assert!(!st.active);
        assert!(!st.primary_intended);
    }

    #[test]
    fn paused_keeps_fallback_active_and_evaluates_nothing() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let mut store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        store.mode = SupervisorMode::Paused;
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Delete production db? [y/N]"],
        );
        let primary = MockCognitive::new(auto_reply_eval());
        let fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState {
                primary_intended: true,
                active: true,
                reason: Some(crate::supervisor::fallback::FallbackReason::Health),
                since: Some(900),
                ..Default::default()
            },
            false,
            auto_reply_eval(),
        );
        let fb_eval_calls = fb.fb_eval_calls.clone();

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));
        engine.tick(1000).unwrap();
        assert_eq!(*fb_eval_calls.lock().unwrap(), 0);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 0);
        assert!(engine.fallback_state().active);
    }

    #[test]
    fn fallback_eval_error_keeps_fallback_and_is_redacted() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Delete production db? [y/N]"],
        );
        let primary = MockCognitive::new(noop_eval());
        let mut fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState {
                primary_intended: true,
                active: true,
                reason: Some(crate::supervisor::fallback::FallbackReason::Health),
                since: Some(900),
                ..Default::default()
            },
            false,
            noop_eval(),
        );
        fb.backend_error = Some("401 unauthorized Bearer sk-secret".to_string());
        fb.redact_secret = Some("sk-secret".to_string());

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));
        engine.tick(1000).unwrap();
        assert!(engine.fallback_state().active);
        let err = engine.store.agents["t1"]
            .last_eval_error
            .as_deref()
            .unwrap();
        assert!(err.contains("***"));
        assert!(!err.contains("sk-secret"));

        let snap = crate::supervisor::tui::build_desktop_mod_snapshot(&engine);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("sk-secret"));
    }

    #[test]
    fn audit_entry_records_answering_model() {
        let dir = tempdir().unwrap();
        let cfg = test_config(&dir);
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
        let hc = MockHc::new(
            vec![test_session("t1")],
            vec!["Delete production db? [y/N]"],
        );
        let primary = MockCognitive::new(noop_eval());
        let fb = FakeFallback::new(
            crate::supervisor::fallback::FallbackState {
                primary_intended: true,
                active: true,
                reason: Some(crate::supervisor::fallback::FallbackReason::Health),
                since: Some(900),
                ..Default::default()
            },
            false,
            auto_reply_eval(),
        );

        let mut engine = WatcherEngine::new(cfg, store, hc, primary).with_fallback(Box::new(fb));
        engine.tick(1000).unwrap();

        let log_path = dir.path().join("auto_replies.jsonl");
        let raw = std::fs::read_to_string(log_path).unwrap();
        let entry: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(entry["model"], "openai/gpt-4.1-mini");
    }
}
