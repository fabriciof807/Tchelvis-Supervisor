use crate::herdr::{
    classify_terminal_for_send, FleetTracker, GuardedSendOutcome, HcTerminal, HerdrClient,
    DEFAULT_HC_SEND_TIMEOUT,
};
use crate::orch::store::OrchStore;
use crate::orch::DeliveryOutcome;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;

pub const DELIVERY_BACKOFF_BASE_MS: i64 = 250;
pub const DELIVERY_BACKOFF_MAX_MS: i64 = 5_000;

pub fn pointer_deferral_reason(
    tracker: &FleetTracker,
    target_terminal: &str,
    now_ms: i64,
) -> Option<&'static str> {
    let term = tracker.terminals.get(target_terminal);
    let att = term.map(|_| tracker.effective_attention(target_terminal, now_ms));
    classify_terminal_for_send(term, att).1.err()
}

pub fn is_terminal_safe_for_pointer(
    tracker: &FleetTracker,
    target_terminal: &str,
    now_ms: i64,
) -> bool {
    pointer_deferral_reason(tracker, target_terminal, now_ms).is_none()
}

pub fn format_pointer_line(run_id: &str, count: usize) -> String {
    format!(
        "[tchelvis] You have {count} orchestration message(s) in run {run_id}. Run: tchelvis orch check --run {run_id}"
    )
}

fn compute_backoff_ms(consecutive_failures: u32) -> i64 {
    if consecutive_failures == 0 {
        return 0;
    }
    let shift = consecutive_failures.saturating_sub(1).min(5);
    (DELIVERY_BACKOFF_BASE_MS << shift).min(DELIVERY_BACKOFF_MAX_MS)
}

fn tracker_status_label(
    tracker: &FleetTracker,
    term: Option<&HcTerminal>,
    target_terminal: &str,
    now_ms: i64,
) -> String {
    let att = term.map(|_| tracker.effective_attention(target_terminal, now_ms));
    classify_terminal_for_send(term, att).0
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoggedDeliveryKey {
    outcome: &'static str,
    reason: String,
    detail: Option<String>,
    tracker_status: String,
}

#[derive(Debug, Clone, Default)]
struct TargetDeliveryState {
    last_tracker_status: Option<String>,
    consecutive_failures: u32,
    attempts: u32,
    next_attempt_after_ms: i64,
    last_logged: Option<LoggedDeliveryKey>,
}

#[derive(Debug, Clone)]
pub struct CompletedSend {
    pub target_terminal: String,
    pub run_id: String,
    pub pending_count: usize,
    pub tracker_status: String,
    pub result: GuardedSendOutcome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleOutcome {
    pub spawned_count: usize,
    pub deferred_count: usize,
    pub stalled_count: usize,
}

pub struct DeliveryScheduler {
    herdr: HerdrClient,
    send_timeout: Duration,
    in_flight: HashSet<String>,
    target_states: HashMap<(String, String), TargetDeliveryState>,
    done_tx: mpsc::UnboundedSender<CompletedSend>,
    done_rx: mpsc::UnboundedReceiver<CompletedSend>,
}

impl DeliveryScheduler {
    pub fn new(herdr: HerdrClient) -> Self {
        Self::with_timeout(herdr, DEFAULT_HC_SEND_TIMEOUT)
    }

    pub fn with_timeout(herdr: HerdrClient, send_timeout: Duration) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            herdr,
            send_timeout,
            in_flight: HashSet::new(),
            target_states: HashMap::new(),
            done_tx,
            done_rx,
        }
    }

    pub fn is_in_flight(&self, target_terminal: &str) -> bool {
        self.in_flight.contains(target_terminal)
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub fn target_attempts(&self, target_terminal: &str, run_id: &str) -> u32 {
        self.target_states
            .get(&(target_terminal.to_string(), run_id.to_string()))
            .map(|s| s.attempts)
            .unwrap_or(0)
    }

    pub fn target_next_attempt_after_ms(&self, target_terminal: &str, run_id: &str) -> Option<i64> {
        self.target_states
            .get(&(target_terminal.to_string(), run_id.to_string()))
            .map(|s| s.next_attempt_after_ms)
    }

    pub async fn recv_completed(&mut self) -> Option<CompletedSend> {
        self.done_rx.recv().await
    }

    pub fn try_recv_completed(&mut self) -> Option<CompletedSend> {
        self.done_rx.try_recv().ok()
    }

    pub fn schedule_pending(
        &mut self,
        store: &OrchStore,
        tracker: &FleetTracker,
        now_ms: i64,
    ) -> Result<ScheduleOutcome> {
        let (requeued, stalled) = store.requeue_expired_pointers_detailed(now_ms)?;
        if requeued > 0 || stalled > 0 {
            tracing::info!(requeued, stalled, "requeued expired orchestration pointers");
        }

        let targets = store.pending_pointer_targets(now_ms)?;
        let active_keys: HashSet<(String, String)> = targets
            .iter()
            .filter(|t| t.pending_count > 0)
            .map(|t| (t.target_terminal.clone(), t.run_id.clone()))
            .collect();
        self.target_states
            .retain(|k, _| active_keys.contains(k) || self.in_flight.contains(&k.0));

        let mut outcome = ScheduleOutcome {
            spawned_count: 0,
            deferred_count: 0,
            stalled_count: stalled,
        };

        for target in targets {
            if target.pending_count == 0 {
                continue;
            }
            let key = (target.target_terminal.clone(), target.run_id.clone());
            let term = tracker.terminals.get(&target.target_terminal);
            let tracker_status =
                tracker_status_label(tracker, term, &target.target_terminal, now_ms);

            if target.has_active_unexpired_pointer {
                let state = self.target_states.entry(key).or_default();
                let log_key = LoggedDeliveryKey {
                    outcome: "deferred",
                    reason: "active_unexpired_pointer".to_string(),
                    detail: None,
                    tracker_status: tracker_status.clone(),
                };
                if state.last_logged.as_ref() != Some(&log_key) {
                    tracing::info!(
                        target_terminal = %target.target_terminal,
                        run = %target.run_id,
                        pending_count = target.pending_count,
                        outcome = "deferred",
                        reason = "active_unexpired_pointer",
                        tracker_status = tracker_status.as_str(),
                        attempts = state.attempts,
                        "orch pointer delivery deferred"
                    );
                    state.last_logged = Some(log_key);
                }
                continue;
            }

            if self.in_flight.contains(&target.target_terminal) {
                continue;
            }

            let att = term.map(|_| tracker.effective_attention(&target.target_terminal, now_ms));
            let (classified_status, mode_res) = classify_terminal_for_send(term, att);
            let state = self.target_states.entry(key).or_default();

            if state.last_tracker_status.as_deref() != Some(classified_status.as_str()) {
                state.consecutive_failures = 0;
                state.next_attempt_after_ms = now_ms;
                state.last_tracker_status = Some(classified_status.clone());
            }

            if now_ms < state.next_attempt_after_ms {
                outcome.deferred_count += 1;
                continue;
            }

            match mode_res {
                Err(defer_reason) => {
                    state.attempts += 1;
                    state.consecutive_failures += 1;
                    let delay_ms = compute_backoff_ms(state.consecutive_failures);
                    state.next_attempt_after_ms = now_ms + delay_ms;

                    let log_key = LoggedDeliveryKey {
                        outcome: "deferred",
                        reason: defer_reason.to_string(),
                        detail: None,
                        tracker_status: classified_status.clone(),
                    };
                    if state.last_logged.as_ref() != Some(&log_key) {
                        tracing::info!(
                            target_terminal = %target.target_terminal,
                            run = %target.run_id,
                            pending_count = target.pending_count,
                            outcome = "deferred",
                            reason = defer_reason,
                            tracker_status = classified_status.as_str(),
                            attempts = state.attempts,
                            backoff_ms = delay_ms,
                            "orch pointer delivery deferred"
                        );
                        state.last_logged = Some(log_key);
                    }
                    outcome.deferred_count += 1;
                }
                Ok(mode) => {
                    self.in_flight.insert(target.target_terminal.clone());
                    let herdr = self.herdr.clone();
                    let tx = self.done_tx.clone();
                    let pointer = format_pointer_line(&target.run_id, target.pending_count);
                    let timeout = self.send_timeout;
                    let target_terminal = target.target_terminal.clone();
                    let run_id = target.run_id.clone();
                    let pending_count = target.pending_count;
                    tokio::spawn(async move {
                        let result = herdr
                            .send_guarded_with_mode(&target_terminal, &pointer, mode, timeout)
                            .await;
                        let _ = tx.send(CompletedSend {
                            target_terminal,
                            run_id,
                            pending_count,
                            tracker_status: classified_status,
                            result,
                        });
                    });
                    outcome.spawned_count += 1;
                }
            }
        }

        Ok(outcome)
    }

    pub fn handle_completed(
        &mut self,
        store: &OrchStore,
        completed: CompletedSend,
        now_ms: i64,
    ) -> Result<bool> {
        self.in_flight.remove(&completed.target_terminal);
        let key = (completed.target_terminal.clone(), completed.run_id.clone());

        match completed.result {
            GuardedSendOutcome::Sent { used_plain_send } => {
                let attempts = self
                    .target_states
                    .get(&key)
                    .map(|s| s.attempts + 1)
                    .unwrap_or(1);
                store.mark_pointer_delivered(
                    &completed.target_terminal,
                    &completed.run_id,
                    now_ms,
                )?;
                self.target_states.remove(&key);
                tracing::info!(
                    target_terminal = %completed.target_terminal,
                    run = %completed.run_id,
                    pending_count = completed.pending_count,
                    outcome = "sent",
                    tracker_status = completed.tracker_status.as_str(),
                    send_mode = if used_plain_send { "plain" } else { "if_idle" },
                    attempts,
                    "orch pointer delivered"
                );
                Ok(true)
            }
            GuardedSendOutcome::Deferred { reason, detail } => {
                let state = self.target_states.entry(key).or_default();
                state.attempts += 1;
                state.consecutive_failures += 1;
                let delay_ms = compute_backoff_ms(state.consecutive_failures);
                state.next_attempt_after_ms = now_ms + delay_ms;
                state.last_tracker_status = Some(completed.tracker_status.clone());

                let log_key = LoggedDeliveryKey {
                    outcome: "deferred",
                    reason: reason.clone(),
                    detail: detail.clone(),
                    tracker_status: completed.tracker_status.clone(),
                };
                if state.last_logged.as_ref() != Some(&log_key) {
                    if let Some(ref d) = detail {
                        tracing::info!(
                            target_terminal = %completed.target_terminal,
                            run = %completed.run_id,
                            pending_count = completed.pending_count,
                            outcome = "deferred",
                            reason = reason.as_str(),
                            detail = d.as_str(),
                            tracker_status = completed.tracker_status.as_str(),
                            attempts = state.attempts,
                            backoff_ms = delay_ms,
                            "orch pointer delivery deferred"
                        );
                    } else {
                        tracing::info!(
                            target_terminal = %completed.target_terminal,
                            run = %completed.run_id,
                            pending_count = completed.pending_count,
                            outcome = "deferred",
                            reason = reason.as_str(),
                            tracker_status = completed.tracker_status.as_str(),
                            attempts = state.attempts,
                            backoff_ms = delay_ms,
                            "orch pointer delivery deferred"
                        );
                    }
                    state.last_logged = Some(log_key);
                }
                Ok(false)
            }
            GuardedSendOutcome::Error { reason, detail } => {
                let state = self.target_states.entry(key).or_default();
                state.attempts += 1;
                state.consecutive_failures += 1;
                let delay_ms = compute_backoff_ms(state.consecutive_failures);
                state.next_attempt_after_ms = now_ms + delay_ms;
                state.last_tracker_status = Some(completed.tracker_status.clone());

                let log_key = LoggedDeliveryKey {
                    outcome: "error",
                    reason: reason.clone(),
                    detail: detail.clone(),
                    tracker_status: completed.tracker_status.clone(),
                };
                if state.last_logged.as_ref() != Some(&log_key) {
                    if let Some(ref d) = detail {
                        tracing::info!(
                            target_terminal = %completed.target_terminal,
                            run = %completed.run_id,
                            pending_count = completed.pending_count,
                            outcome = "error",
                            reason = reason.as_str(),
                            detail = d.as_str(),
                            tracker_status = completed.tracker_status.as_str(),
                            attempts = state.attempts,
                            backoff_ms = delay_ms,
                            "orch pointer delivery failed; deferred for retry"
                        );
                    } else {
                        tracing::info!(
                            target_terminal = %completed.target_terminal,
                            run = %completed.run_id,
                            pending_count = completed.pending_count,
                            outcome = "error",
                            reason = reason.as_str(),
                            tracker_status = completed.tracker_status.as_str(),
                            attempts = state.attempts,
                            backoff_ms = delay_ms,
                            "orch pointer delivery failed; deferred for retry"
                        );
                    }
                    state.last_logged = Some(log_key);
                }
                Ok(false)
            }
        }
    }
}

pub struct OrchDeliveryCoordinator;

impl OrchDeliveryCoordinator {
    pub fn is_terminal_safe_for_pointer(
        tracker: &FleetTracker,
        target_terminal: &str,
        now_ms: i64,
    ) -> bool {
        is_terminal_safe_for_pointer(tracker, target_terminal, now_ms)
    }

    pub fn format_pointer_line(run_id: &str, count: usize) -> String {
        format_pointer_line(run_id, count)
    }

    pub async fn deliver_pending_pointers(
        store: &OrchStore,
        herdr: &HerdrClient,
        tracker: &FleetTracker,
        now_ms: i64,
    ) -> Result<DeliveryOutcome> {
        let mut scheduler = DeliveryScheduler::new(herdr.clone());
        let sched_out = scheduler.schedule_pending(store, tracker, now_ms)?;
        let mut outcome = DeliveryOutcome {
            delivered_count: 0,
            deferred_count: sched_out.deferred_count,
            stalled_count: sched_out.stalled_count,
        };
        for _ in 0..sched_out.spawned_count {
            if let Some(completed) = scheduler.recv_completed().await {
                if scheduler.handle_completed(store, completed, now_ms)? {
                    outcome.delivered_count += 1;
                } else {
                    outcome.deferred_count += 1;
                }
            }
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{parse_hc_event_line, HcStatus, HcTerminal};
    use crate::orch::{
        DispatchContext, DispatchStatus, OrchRun, OrchTask, PointerState, RunStatus, TaskStatus,
        POINTER_RETRY_BACKOFF_MS,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn make_terminal(address: &str, state: &str, status_state: &str) -> HcTerminal {
        HcTerminal {
            address: address.to_string(),
            state: state.to_string(),
            reported: true,
            status: Some(HcStatus {
                state: status_state.to_string(),
                ..Default::default()
            }),
            created_ms: Some(1_000),
            ..Default::default()
        }
    }

    #[test]
    fn test_format_pointer_line_canonical() {
        let line = format_pointer_line("run_abc", 2);
        assert_eq!(
            line,
            "[tchelvis] You have 2 orchestration message(s) in run run_abc. Run: tchelvis orch check --run run_abc"
        );
    }

    #[test]
    fn test_is_terminal_safe_for_pointer_guards_states_and_hysteresis() {
        let mut tracker = FleetTracker::new();
        assert!(!is_terminal_safe_for_pointer(&tracker, "t1", 1_000));

        tracker.apply_snapshot(
            vec![
                make_terminal("t_idle", "live", "idle"),
                make_terminal("t_done", "live", "done"),
                make_terminal("t_work", "live", "working"),
                make_terminal("t_block", "live", "blocked"),
                make_terminal("t_err", "live", "error"),
                make_terminal("t_fin", "finished", "idle"),
            ],
            1_000,
        );

        assert!(is_terminal_safe_for_pointer(&tracker, "t_idle", 1_000));
        assert!(is_terminal_safe_for_pointer(&tracker, "t_done", 1_000));
        assert!(!is_terminal_safe_for_pointer(&tracker, "t_work", 1_000));
        assert!(!is_terminal_safe_for_pointer(&tracker, "t_block", 1_000));
        assert!(!is_terminal_safe_for_pointer(&tracker, "t_err", 1_000));
        assert!(!is_terminal_safe_for_pointer(&tracker, "t_fin", 1_000));

        // If t_block transitions to idle at 1100ms, 1500ms clear hysteresis holds it blocked at 2000ms
        let ev = parse_hc_event_line(
            r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"idle"},"seq":2,"terminal":"t_block","type":"status"}}"#,
        )
        .unwrap();
        tracker.apply_stream_item(ev, 1_100);
        assert!(!is_terminal_safe_for_pointer(&tracker, "t_block", 2_000));
        assert!(is_terminal_safe_for_pointer(&tracker, "t_block", 2_700));
    }

    static DELIVERY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn test_deliver_pending_pointers_coalescing_deferral_and_stalling() {
        let _test_guard = DELIVERY_TEST_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let exit_code_file = tmp.path().join("exit_code");
        let log_file = tmp.path().join("hc_send.log");
        fs::write(&exit_code_file, "0").unwrap();

        let fake_hc = tmp.path().join("fake-hc");
        let script = format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"send\" ]; then\n\
               echo \"$*\" >> \"{}\"\n\
               code=$(cat \"{}\")\n\
               exit \"$code\"\n\
             fi\n\
             exit 0\n",
            log_file.display(),
            exit_code_file.display()
        );
        fs::write(&fake_hc, script).unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();

        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());
        let store = OrchStore::open_in_memory().unwrap();

        store
            .create_run(&OrchRun {
                id: "run_1".into(),
                title: "Test Run".into(),
                coordinator_terminal: "t1".into(),
                coordinator_created_ms: 1_000,
                status: RunStatus::Active,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        store
            .create_or_update_task(&OrchTask {
                id: "task_a".into(),
                run_id: "run_1".into(),
                title: "Task A".into(),
                prompt: "Do A".into(),
                deps: vec![],
                failure_count: 0,
                status: TaskStatus::Ready,
                result_summary: None,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        store
            .create_or_update_task(&OrchTask {
                id: "task_b".into(),
                run_id: "run_1".into(),
                title: "Task B".into(),
                prompt: "Do B".into(),
                deps: vec![],
                failure_count: 0,
                status: TaskStatus::Ready,
                result_summary: None,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        store
            .create_dispatch(&DispatchContext {
                id: "disp_a".into(),
                run_id: "run_1".into(),
                task_id: "task_a".into(),
                parent_terminal: "t1".into(),
                assignee_terminal: "t2".into(),
                terminal_created_ms: 2_000,
                turn_at_dispatch: 0,
                agent_kind: "claude".into(),
                worktree_mode: "current".into(),
                worktree_path: None,
                depth: 1,
                status: DispatchStatus::Dispatched,
                idle_since_ms: None,
                blocked_since_ms: None,
                created_at_ms: 1_100,
                updated_at_ms: 1_100,
            })
            .unwrap();
        store
            .create_dispatch(&DispatchContext {
                id: "disp_b".into(),
                run_id: "run_1".into(),
                task_id: "task_b".into(),
                parent_terminal: "t1".into(),
                assignee_terminal: "t3".into(),
                terminal_created_ms: 3_000,
                turn_at_dispatch: 0,
                agent_kind: "claude".into(),
                worktree_mode: "current".into(),
                worktree_path: None,
                depth: 1,
                status: DispatchStatus::Dispatched,
                idle_since_ms: None,
                blocked_since_ms: None,
                created_at_ms: 1_100,
                updated_at_ms: 1_100,
            })
            .unwrap();

        // Worker t2 finishes task_a -> creates pending message for t1
        store
            .record_worker_done("disp_a", "t2", 2_000, "succeeded", "Done A", 1_500)
            .unwrap();

        // 1. Coordinator t1 is currently Working -> pointer delivery is deferred
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1", "live", "working")], 1_600);
        let out1 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 1_600)
                .await
                .unwrap();
        assert_eq!(out1.delivered_count, 0);
        assert_eq!(out1.deferred_count, 1);

        // 2. Coordinator t1 is Idle, but hc send --if idle returns exit code 10 (status_mismatch)
        tracker.apply_snapshot(vec![make_terminal("t1", "live", "idle")], 1_700);
        fs::write(&exit_code_file, "10").unwrap();
        let out2 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 1_700)
                .await
                .unwrap();
        assert_eq!(out2.delivered_count, 0);
        assert_eq!(out2.deferred_count, 1);

        // 3. hc send --if idle returns exit code 0 -> delivered!
        fs::write(&exit_code_file, "0").unwrap();
        let out3 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 2_000)
                .await
                .unwrap();
        assert_eq!(out3.delivered_count, 1);
        assert_eq!(out3.deferred_count, 0);

        // 4. Worker t3 finishes task_b while t1 still has an active unexpired pointer -> coalesced (not sent yet)
        store
            .record_worker_done("disp_b", "t3", 3_000, "succeeded", "Done B", 2_500)
            .unwrap();
        let out4 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 2_600)
                .await
                .unwrap();
        assert_eq!(out4.delivered_count, 0);

        // 5. After 30s backoff, pointer 1 requeues and coalesces with message 2 (attempt 2 for msg 1, attempt 1 for msg 2)
        let t_retry1 = 2_000 + POINTER_RETRY_BACKOFF_MS;
        let out5 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, t_retry1)
                .await
                .unwrap();
        assert_eq!(out5.delivered_count, 1);

        // 6. Attempt 3 for msg 1
        let t_retry2 = t_retry1 + POINTER_RETRY_BACKOFF_MS;
        let out6 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, t_retry2)
                .await
                .unwrap();
        assert_eq!(out6.delivered_count, 1);

        // 7. After 3rd attempt expires, msg 1 transitions to DeliveryStalled (stalled_count == 1), while msg 2 gets its 3rd attempt
        let t_retry3 = t_retry2 + POINTER_RETRY_BACKOFF_MS;
        let out7 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, t_retry3)
                .await
                .unwrap();
        assert_eq!(out7.stalled_count, 1);
        assert_eq!(out7.delivered_count, 1);

        let peeked = store
            .claim_inbox_messages("t1", Some("run_1"), true, t_retry3 + 100)
            .unwrap();
        assert!(peeked
            .messages
            .iter()
            .any(|m| m.pointer_state == PointerState::DeliveryStalled));
    }

    #[tokio::test]
    async fn test_deliver_pending_pointers_emits_info_logs() {
        let _test_guard = DELIVERY_TEST_LOCK.lock().await;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let exit_code_file = tmp.path().join("exit_code");
        fs::write(&exit_code_file, "1").unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        fs::write(
            &fake_hc,
            format!(
                "#!/usr/bin/env bash\n\
                 if [ \"$1\" = \"send\" ]; then\n\
                   exit $(cat \"{}\")\n\
                 fi\n\
                 exit 0\n",
                exit_code_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

        let store = OrchStore::open_in_memory().unwrap();
        store
            .create_run(&OrchRun {
                id: "run_log".into(),
                title: "Log Run".into(),
                coordinator_terminal: "t1".into(),
                coordinator_created_ms: 1_000,
                status: RunStatus::Active,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        store
            .create_or_update_task(&OrchTask {
                id: "task_a".into(),
                run_id: "run_log".into(),
                title: "Task A".into(),
                prompt: "Do A".into(),
                deps: vec![],
                failure_count: 0,
                status: TaskStatus::Ready,
                result_summary: None,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        store
            .create_dispatch(&DispatchContext {
                id: "disp_a".into(),
                run_id: "run_log".into(),
                task_id: "task_a".into(),
                parent_terminal: "t1".into(),
                assignee_terminal: "t2".into(),
                terminal_created_ms: 2_000,
                turn_at_dispatch: 0,
                agent_kind: "claude".into(),
                worktree_mode: "current".into(),
                worktree_path: None,
                depth: 1,
                status: DispatchStatus::Dispatched,
                idle_since_ms: None,
                blocked_since_ms: None,
                created_at_ms: 1_100,
                updated_at_ms: 1_100,
            })
            .unwrap();
        store
            .record_worker_done("disp_a", "t2", 2_000, "succeeded", "Done", 1_200)
            .unwrap();

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(buf.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1", "live", "working")], 1_300);
        let out1 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 1_300)
                .await
                .unwrap();
        assert_eq!((out1.delivered_count, out1.deferred_count), (0, 1));

        tracker.apply_snapshot(vec![make_terminal("t1", "live", "idle")], 1_400);
        let out2 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 1_400)
                .await
                .unwrap();
        assert_eq!((out2.delivered_count, out2.deferred_count), (0, 1));

        fs::write(&exit_code_file, "0").unwrap();
        let out3 =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 1_500)
                .await
                .unwrap();
        assert_eq!((out3.delivered_count, out3.deferred_count), (1, 0));

        drop(_guard);

        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("INFO")
                && logs.contains("target_terminal=t1")
                && logs.contains("run=run_log"),
            "unexpected logs:\n{logs}"
        );
        assert!(logs.contains("pending_count=1"), "unexpected logs:\n{logs}");
        assert!(
            logs.contains("outcome=\"deferred\"") && logs.contains("reason=\"terminal_working\""),
            "unexpected logs:\n{logs}"
        );
        assert!(
            logs.contains("outcome=\"error\""),
            "unexpected logs:\n{logs}"
        );
        assert!(
            logs.contains("outcome=\"sent\""),
            "unexpected logs:\n{logs}"
        );
    }

    fn seed_pending_delivery_store(coord_terminal: &str) -> OrchStore {
        let store = OrchStore::open_in_memory().unwrap();
        store
            .create_run(&OrchRun {
                id: "run_g".into(),
                title: "Lot G Run".into(),
                coordinator_terminal: coord_terminal.into(),
                coordinator_created_ms: 1_000,
                status: RunStatus::Active,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        store
            .create_or_update_task(&OrchTask {
                id: "task_a".into(),
                run_id: "run_g".into(),
                title: "Task A".into(),
                prompt: "Do A".into(),
                deps: vec![],
                failure_count: 0,
                status: TaskStatus::Ready,
                result_summary: None,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        store
            .create_dispatch(&DispatchContext {
                id: "disp_a".into(),
                run_id: "run_g".into(),
                task_id: "task_a".into(),
                parent_terminal: coord_terminal.into(),
                assignee_terminal: "t2".into(),
                terminal_created_ms: 2_000,
                turn_at_dispatch: 0,
                agent_kind: "claude".into(),
                worktree_mode: "current".into(),
                worktree_path: None,
                depth: 1,
                status: DispatchStatus::Dispatched,
                idle_since_ms: None,
                blocked_since_ms: None,
                created_at_ms: 1_100,
                updated_at_ms: 1_100,
            })
            .unwrap();
        store
            .record_worker_done("disp_a", "t2", 2_000, "succeeded", "Done", 1_200)
            .unwrap();
        store
    }

    fn capture_info_logs() -> (
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        tracing::subscriber::DefaultGuard,
    ) {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = BufWriter::default();
        let raw = buf.0.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(buf)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (raw, guard)
    }

    #[tokio::test]
    async fn test_done_coordinator_uses_get_recheck_then_plain_send_and_working_sends_nothing() {
        let _test_guard = DELIVERY_TEST_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let calls_log = tmp.path().join("calls.log");
        let fake_hc = tmp.path().join("fake-hc");
        fs::write(
            &fake_hc,
            format!(
                "#!/usr/bin/env bash\n\
                 echo \"$*\" >> \"{}\"\n\
                 if [ \"$1\" = \"get\" ]; then\n\
                   echo '{{\"address\":\"t1b3\",\"state\":\"live\",\"status\":{{\"state\":\"done\",\"app\":\"claude\"}}}}'\n\
                   exit 0\n\
                 fi\n\
                 if [ \"$1\" = \"send\" ]; then\n\
                   echo '{{\"started\":false,\"status\":\"done\"}}'\n\
                   exit 0\n\
                 fi\n\
                 exit 0\n",
                calls_log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();

        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());
        let store = seed_pending_delivery_store("t1b3");

        // Working coordinator -> no hc call at all
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1b3", "live", "working")], 1_300);
        let d_work =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 1_300)
                .await
                .unwrap();
        assert_eq!((d_work.delivered_count, d_work.deferred_count), (0, 1));
        assert!(!calls_log.exists());

        // Done coordinator -> re-checks `get`, then plain `send` (no `--if idle`), marks delivered
        tracker.apply_snapshot(vec![make_terminal("t1b3", "live", "done")], 1_400);
        let d_done =
            OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 1_400)
                .await
                .unwrap();
        assert_eq!((d_done.delivered_count, d_done.deferred_count), (1, 0));

        let calls = fs::read_to_string(&calls_log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 2, "expected get + plain send, got: {calls}");
        assert!(lines[0].starts_with("get t1b3 --json"));
        assert!(lines[1].starts_with("send t1b3 "));
        assert!(!lines[1].contains("--if"));

        // Pointer is marked delivered (not pending anymore)
        let pending = store.pending_pointer_targets(1_500).unwrap();
        assert!(
            pending.is_empty(),
            "pointer should be marked delivered, got: {pending:?}"
        );
    }

    #[tokio::test]
    async fn test_status_mismatch_logs_detail_and_backoff_limits_attempts_over_simulated_minute() {
        let _test_guard = DELIVERY_TEST_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let calls_log = tmp.path().join("calls.log");
        let mode_file = tmp.path().join("mode");
        fs::write(&mode_file, "mismatch").unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        fs::write(
            &fake_hc,
            format!(
                "#!/usr/bin/env bash\n\
                 echo \"$*\" >> \"{}\"\n\
                 mode=$(cat \"{}\")\n\
                 if [ \"$1\" = \"get\" ]; then\n\
                   echo '{{\"address\":\"t1b3\",\"state\":\"live\",\"status\":{{\"state\":\"done\",\"app\":\"claude\"}}}}'\n\
                   exit 0\n\
                 fi\n\
                 if [ \"$1\" = \"send\" ]; then\n\
                   if [ \"$mode\" = \"mismatch\" ]; then\n\
                     echo '{{\"error\":{{\"hint\":\"nothing was typed\",\"kind\":\"status_mismatch\",\"message\":\"t1b3 is done, not idle\"}}}}'\n\
                     exit 10\n\
                   fi\n\
                   echo '{{\"started\":false,\"status\":\"done\"}}'\n\
                   exit 0\n\
                 fi\n\
                 exit 0\n",
                calls_log.display(),
                mode_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();

        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());
        let store = seed_pending_delivery_store("t1b3");
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1b3", "live", "idle")], 1_000);

        let (raw_logs, _log_guard) = capture_info_logs();
        let mut scheduler = DeliveryScheduler::new(herdr);

        // Simulate 1 minute (60,000 ms) of 500 ms daemon ticks with an injected clock, no real sleeps.
        let mut now_ms = 1_000_i64;
        for _ in 0..120 {
            let sched = scheduler
                .schedule_pending(&store, &tracker, now_ms)
                .unwrap();
            if sched.spawned_count > 0 {
                let completed = scheduler.recv_completed().await.unwrap();
                let delivered = scheduler
                    .handle_completed(&store, completed, now_ms)
                    .unwrap();
                assert!(!delivered);
            }
            now_ms += 500;
        }

        let attempts_after_minute = scheduler.target_attempts("t1b3", "run_g");
        assert!(
            (10..=18).contains(&attempts_after_minute),
            "expected backoff (250ms..5s cap) to limit attempts over 60s to ~14, got {attempts_after_minute}"
        );

        let calls_count = fs::read_to_string(&calls_log).unwrap().lines().count();
        assert_eq!(calls_count, attempts_after_minute as usize);

        let logs_during_minute = String::from_utf8(raw_logs.lock().unwrap().clone()).unwrap();
        let mismatch_lines: Vec<&str> = logs_during_minute
            .lines()
            .filter(|l| l.contains("status_mismatch"))
            .collect();
        assert_eq!(
            mismatch_lines.len(),
            1,
            "identical refusal should log only once on transition, got:\n{logs_during_minute}"
        );
        assert!(mismatch_lines[0].contains("detail=\"t1b3 is done, not idle\""));
        assert!(mismatch_lines[0].contains("tracker_status=\"idle\""));
        assert!(mismatch_lines[0].contains("attempts=1"));

        // Status changes to `done` -> resets backoff immediately so next tick sends right away and logs `sent`
        fs::write(&mode_file, "ok").unwrap();
        tracker.apply_snapshot(vec![make_terminal("t1b3", "live", "done")], now_ms);
        let sched = scheduler
            .schedule_pending(&store, &tracker, now_ms)
            .unwrap();
        assert_eq!(sched.spawned_count, 1);
        let completed = scheduler.recv_completed().await.unwrap();
        let delivered = scheduler
            .handle_completed(&store, completed, now_ms)
            .unwrap();
        assert!(delivered);

        drop(_log_guard);
        let final_logs = String::from_utf8(raw_logs.lock().unwrap().clone()).unwrap();
        assert!(
            final_logs.contains("outcome=\"sent\"")
                && final_logs.contains("tracker_status=\"done\"")
                && final_logs.contains("send_mode=\"plain\""),
            "expected sent log after transition, got:\n{final_logs}"
        );
    }

    #[tokio::test]
    async fn test_slow_send_does_not_block_scheduler_and_ends_as_deferred_send_timeout() {
        let _test_guard = DELIVERY_TEST_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        fs::write(
            &fake_hc,
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"send\" ]; then\n\
               sleep 5\n\
               exit 0\n\
             fi\n\
             exit 0\n",
        )
        .unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();

        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());
        let store = seed_pending_delivery_store("t1");
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1", "live", "idle")], 1_000);

        let (raw_logs, _log_guard) = capture_info_logs();
        let mut scheduler = DeliveryScheduler::with_timeout(herdr, Duration::from_millis(50));

        // 1. Scheduling spawns background send and returns immediately without waiting on hc
        let start = std::time::Instant::now();
        let sched1 = scheduler.schedule_pending(&store, &tracker, 1_000).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(40),
            "schedule_pending must not block on slow hc"
        );
        assert_eq!(sched1.spawned_count, 1);
        assert!(scheduler.is_in_flight("t1"));

        // 2. Second tick while in-flight must not spawn a duplicate send for t1
        let sched2 = scheduler.schedule_pending(&store, &tracker, 1_010).unwrap();
        assert_eq!(sched2.spawned_count, 0);
        assert_eq!(scheduler.in_flight_count(), 1);

        // 3. Spawned task times out cleanly as Deferred(send_timeout), leaving pointer pending
        let completed = scheduler.recv_completed().await.unwrap();
        assert!(matches!(
            completed.result,
            GuardedSendOutcome::Deferred { ref reason, .. } if reason == "send_timeout"
        ));
        let delivered = scheduler
            .handle_completed(&store, completed, 1_060)
            .unwrap();
        assert!(!delivered);
        assert!(!scheduler.is_in_flight("t1"));
        assert_eq!(store.pending_pointer_targets(1_100).unwrap().len(), 1);

        drop(_log_guard);
        let logs = String::from_utf8(raw_logs.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("outcome=\"deferred\"") && logs.contains("reason=\"send_timeout\""),
            "expected deferred(send_timeout) log, got:\n{logs}"
        );
        assert!(
            !logs.contains("outcome=\"error\""),
            "send_timeout must not be logged as error:\n{logs}"
        );
    }
}
