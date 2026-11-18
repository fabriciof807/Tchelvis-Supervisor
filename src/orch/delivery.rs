use crate::herdr::{FleetTracker, HerdrClient};
use crate::model::AttentionState;
use crate::orch::store::OrchStore;
use crate::orch::DeliveryOutcome;
use anyhow::Result;

pub struct OrchDeliveryCoordinator;

pub fn is_terminal_safe_for_pointer(
    tracker: &FleetTracker,
    target_terminal: &str,
    now_ms: i64,
) -> bool {
    let Some(term) = tracker.terminals.get(target_terminal) else {
        return false;
    };
    if term.state == "finished" {
        return false;
    }
    matches!(
        tracker.effective_attention(target_terminal, now_ms),
        AttentionState::Idle | AttentionState::Done { .. }
    )
}

pub fn format_pointer_line(run_id: &str, count: usize) -> String {
    format!(
        "[tchelvis] You have {count} orchestration message(s) in run {run_id}. Run: tchelvis orch check --run {run_id}"
    )
}

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
        let pending_before: usize = store
            .pending_pointer_targets(now_ms)?
            .iter()
            .map(|t| t.pending_count)
            .sum();
        let expired_changed = store.requeue_expired_pointers(now_ms)?;
        let targets = store.pending_pointer_targets(now_ms)?;
        let pending_after: usize = targets.iter().map(|t| t.pending_count).sum();
        let requeued = pending_after.saturating_sub(pending_before);

        let mut outcome = DeliveryOutcome {
            delivered_count: 0,
            deferred_count: 0,
            stalled_count: expired_changed.saturating_sub(requeued),
        };

        for target in targets {
            if !target.has_active_unexpired_pointer && target.pending_count > 0 {
                if !is_terminal_safe_for_pointer(tracker, &target.target_terminal, now_ms) {
                    outcome.deferred_count += 1;
                    continue;
                }
                let pointer = format_pointer_line(&target.run_id, target.pending_count);
                if herdr
                    .send_guarded_if_idle(&target.target_terminal, &pointer)
                    .await?
                {
                    store.mark_pointer_delivered(
                        &target.target_terminal,
                        &target.run_id,
                        now_ms,
                    )?;
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

    #[tokio::test]
    async fn test_deliver_pending_pointers_coalescing_deferral_and_stalling() {
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
}
