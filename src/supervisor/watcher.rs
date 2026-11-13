use super::config::SupervisorConfig;
use super::guardrails::{has_error_or_loop_signature, validate_evaluation};
use super::hc::HcBackend;
use super::ollama::{CognitiveBackend, SupervisorDecision};
use super::state::{
    compute_sha256, AgentDurableState, AutoReplyAuditEntry, DurableStateStore,
    PendingOperatorAction, SupervisorMode, Urgency,
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
            Self::Idle => "Supervisor: Idle (0 MB)".to_string(),
            Self::Watching(n) => format!("Supervisor: Watching ({n})"),
            Self::Evaluating(_) => "Supervisor: Evaluating...".to_string(),
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
}

impl<H: HcBackend, C: CognitiveBackend> WatcherEngine<H, C> {
    pub fn new(config: SupervisorConfig, store: DurableStateStore, hc: H, cognitive: C) -> Self {
        Self {
            config,
            store,
            hc,
            cognitive,
            zero_active_since: None,
            model_loaded: false,
            runtime_status: SupervisorRuntimeStatus::Idle,
        }
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
            if now.saturating_sub(zero_since) >= self.config.idle_unload_secs {
                if self.model_loaded {
                    let _ = self.cognitive.unload_model();
                    self.model_loaded = false;
                    outcome.model_unloaded = true;
                }
                self.runtime_status = SupervisorRuntimeStatus::Idle;
            } else {
                self.runtime_status = SupervisorRuntimeStatus::Watching(0);
            }
            self.store.save()?;
            return Ok(outcome);
        }

        self.zero_active_since = None;
        self.runtime_status = SupervisorRuntimeStatus::Watching(sessions.len());

        if self.store.mode == SupervisorMode::Paused {
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
            }
            self.store.save()?;
            return Ok(outcome);
        }

        for session in &sessions {
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

            let status_transition =
                session.status != state.last_hc_status && session.has_notable_hc_status();
            state.last_hc_status = session.status.clone();

            // Already evaluated this exact output and waiting on operator
            if current_hash == state.last_evaluated_hash
                && state.pending_operator_action.is_some()
                && !status_transition
            {
                continue;
            }

            let hash_changed = current_hash != state.last_output_hash;
            if hash_changed {
                state.last_output_hash = current_hash.clone();
                state.last_change_timestamp = now;
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
                continue;
            }

            let state_snapshot = state.clone();
            self.runtime_status = SupervisorRuntimeStatus::Evaluating(session.agent_id.clone());

            let raw_eval = self.cognitive.evaluate(&state_snapshot, &output)?;
            self.model_loaded = true;
            let eval = validate_evaluation(&state_snapshot, self.store.mode, raw_eval);
            outcome.evaluated_agents.push(session.agent_id.clone());

            let state = self
                .store
                .agents
                .get_mut(&session.agent_id)
                .expect("agent state must exist");
            state.rolling_summary = eval.updated_summary.clone();
            state.lifecycle_state = eval.detected_state;
            state.last_evaluated_hash = current_hash.clone();

            match eval.decision {
                SupervisorDecision::NoOp => {}
                SupervisorDecision::AutoReply => {
                    self.hc
                        .send_input(&session.agent_id, &eval.hc_command_payload, false)?;
                    state.consecutive_auto_replies += 1;
                    state.last_auto_action = Some(eval.hc_command_payload.clone());
                    state.pending_operator_action = None;
                    state.auto_reply_log.push(AutoReplyAuditEntry {
                        timestamp: now,
                        payload: eval.hc_command_payload.clone(),
                        summary: eval.updated_summary.clone(),
                    });
                    outcome.auto_replied_agents.push(session.agent_id.clone());
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

            let _ = self.hc.set_labels(
                &session.agent_id,
                &[
                    ("cockpit.supervisor_state", state.lifecycle_state.as_str()),
                    ("cockpit.supervisor_decision", eval.decision.as_str()),
                    ("supervisor.state", state.lifecycle_state.as_str()),
                    ("supervisor.decision", eval.decision.as_str()),
                ],
            );
        }

        self.runtime_status = SupervisorRuntimeStatus::Watching(sessions.len());
        self.store.save()?;
        Ok(outcome)
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
        if let Some(agent) = self.store.agents.get_mut(agent_id) {
            agent.reset_operator_counter();
            agent.last_change_timestamp = now;
        }
        self.store.save()?;
        Ok(())
    }

    /// Operator clicked `[Ignorar]` on an Action Card.
    pub fn ignore_operator_action(&mut self, agent_id: &str) -> Result<()> {
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
        labels: Mutex<Vec<(String, Vec<(String, String)>)>>,
    }

    impl MockHc {
        fn new(sessions: Vec<HcSession>, outputs: Vec<&str>) -> Self {
            Self {
                sessions: Mutex::new(sessions),
                outputs: Mutex::new(outputs.into_iter().map(String::from).collect()),
                sent_inputs: Mutex::new(Vec::new()),
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
            self.sent_inputs
                .lock()
                .unwrap()
                .push((agent_id.to_string(), payload.to_string(), force));
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
        next_eval: Mutex<CognitiveEvaluation>,
    }

    impl MockCognitive {
        fn new(next_eval: CognitiveEvaluation) -> Self {
            Self {
                eval_calls: Mutex::new(0),
                unload_calls: Mutex::new(0),
                next_eval: Mutex::new(next_eval),
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
            Ok(self.next_eval.lock().unwrap().clone())
        }

        fn unload_model(&self) -> Result<()> {
            *self.unload_calls.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn test_session(id: &str) -> HcSession {
        HcSession {
            agent_id: id.to_string(),
            session_name: "refactor-auth".to_string(),
            agent_program: Some("pi".to_string()),
            cwd: Some("/tmp".to_string()),
            state: "live".to_string(),
            status: Some("working".to_string()),
            turn: Some(1),
        }
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
            vec![test_session("worker-01")],
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
        let store = DurableStateStore::load_or_init(&cfg.state_file_path).unwrap();
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

        // > 20 seconds unchanged triggers inactivity evaluation
        let o2 = engine.tick(1021).unwrap();
        assert_eq!(o2.evaluated_agents, vec!["worker-01"]);
        assert_eq!(o2.auto_replied_agents, vec!["worker-01"]);
        assert_eq!(*engine.cognitive.eval_calls.lock().unwrap(), 1);
        assert_eq!(
            engine.hc.sent_inputs.lock().unwrap().as_slice(),
            &[("worker-01".to_string(), "cargo test".to_string(), false)]
        );
        let labels = engine.hc.labels.lock().unwrap();
        assert_eq!(labels.len(), 1);
        assert!(labels[0]
            .1
            .contains(&("cockpit.supervisor_state".to_string(), "WAITING_CONFIRMATION".to_string())));
        assert!(labels[0]
            .1
            .contains(&("supervisor.state".to_string(), "WAITING_CONFIRMATION".to_string())));

        let agent = engine.store.agents.get("worker-01").unwrap();
        assert_eq!(agent.consecutive_auto_replies, 1);
        assert_eq!(agent.auto_reply_log.len(), 1);
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
}
