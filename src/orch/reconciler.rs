use crate::herdr::{FleetTracker, HerdrClient};
use crate::model::AttentionState;
use crate::orch::store::OrchStore;
use crate::orch::{
    DispatchStatus, ReconcileOutcome, BLOCKED_SUPERVISOR_GRACE_MS, WORKER_DONE_QUIET_MS,
};
use anyhow::Result;

pub struct OrchReconciler;

impl OrchReconciler {
    pub async fn reconcile_active_dispatches(
        store: &OrchStore,
        herdr: &HerdrClient,
        tracker: &FleetTracker,
        now_ms: i64,
    ) -> Result<ReconcileOutcome> {
        let dispatches = store.list_active_dispatches()?;
        let mut outcome = ReconcileOutcome::default();

        for disp in dispatches {
            if disp.status != DispatchStatus::Dispatched {
                continue;
            }

            let term = tracker.terminals.get(&disp.assignee_terminal);
            let exited_or_errored = match term {
                None => true,
                Some(t) => {
                    t.created_ms != Some(disp.terminal_created_ms)
                        || t.state == "finished"
                        || t.status_name() == Some("error")
                }
            };

            if exited_or_errored {
                let turn = term.map(|t| t.turn).unwrap_or(disp.turn_at_dispatch);
                let same_instance =
                    term.is_some_and(|t| t.created_ms == Some(disp.terminal_created_ms));
                let tail = if same_instance {
                    herdr
                        .read_tail_lines(&disp.assignee_terminal, 20)
                        .await
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let reason = match term {
                    Some(t) if t.created_ms != Some(disp.terminal_created_ms) => {
                        "worker_exited_or_errored (terminal_recreated)".to_string()
                    }
                    Some(t) => {
                        let mut details = Vec::new();
                        if let Some(code) = t.exit_code {
                            details.push(format!("exit_code={code}"));
                        }
                        if let Some(detail) = t
                            .status_msg()
                            .or(t.reason.as_deref())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            details.push(format!("detail={detail}"));
                        }
                        if details.is_empty() {
                            "worker_exited_or_errored".to_string()
                        } else {
                            format!("worker_exited_or_errored ({})", details.join(", "))
                        }
                    }
                    None => "worker_exited_or_errored (terminal_missing)".to_string(),
                };
                if store
                    .record_turn_stop_escalation(&disp.id, turn, &reason, &tail, now_ms)?
                    .is_some()
                {
                    outcome.stop_escalations += 1;
                    outcome.state_changed = true;
                }
                continue;
            }

            let Some(term) = term else {
                continue;
            };
            let att = tracker.effective_attention(&disp.assignee_terminal, now_ms);

            if term.status_name() == Some("working")
                && matches!(att, AttentionState::Working { .. })
            {
                if disp.idle_since_ms.is_some() || disp.blocked_since_ms.is_some() {
                    store.update_dispatch_timers(&disp.id, None, None, now_ms)?;
                }
                continue;
            }

            if matches!(att, AttentionState::Blocked { .. }) {
                if disp.blocked_since_ms.is_none() {
                    store.update_dispatch_timers(
                        &disp.id,
                        disp.idle_since_ms,
                        Some(now_ms),
                        now_ms,
                    )?;
                }
                let blocked_since = disp.blocked_since_ms.unwrap_or(now_ms);
                if now_ms - blocked_since >= BLOCKED_SUPERVISOR_GRACE_MS {
                    let msg_id = format!("msg_block_{}_{}", disp.id, term.turn);
                    if !store.has_message(&msg_id)? {
                        let tail = herdr
                            .read_tail_lines(&disp.assignee_terminal, 25)
                            .await
                            .unwrap_or_default();
                        if store
                            .record_blocked_escalation(
                                &disp.id,
                                term.turn,
                                term.status_msg(),
                                &tail,
                                now_ms,
                            )?
                            .is_some()
                        {
                            outcome.block_escalations += 1;
                            outcome.state_changed = true;
                        }
                    }
                }
                continue;
            }

            if term.turn > disp.turn_at_dispatch
                && (matches!(att, AttentionState::Idle | AttentionState::Done { .. })
                    || term.is_plain_shell_foreground())
                && !store.has_open_question_for_dispatch(&disp.id)?
            {
                if disp.idle_since_ms.is_none() {
                    store.update_dispatch_timers(&disp.id, Some(now_ms), None, now_ms)?;
                }
                let idle_since = disp.idle_since_ms.unwrap_or(now_ms);
                if now_ms - idle_since >= WORKER_DONE_QUIET_MS {
                    let tail = herdr
                        .read_tail_lines(&disp.assignee_terminal, 25)
                        .await
                        .unwrap_or_default();
                    if store
                        .record_turn_stop_escalation(
                            &disp.id,
                            term.turn,
                            "worker_returned_to_prompt_without_worker_done",
                            &tail,
                            now_ms,
                        )?
                        .is_some()
                    {
                        outcome.stop_escalations += 1;
                        outcome.state_changed = true;
                    }
                }
            }
        }

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{HcStatus, HcTerminal};
    use crate::orch::{DispatchContext, OrchRun, OrchTask, RunStatus, TaskStatus};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn setup_store_with_dispatch(disp_id: &str, assignee: &str, created_ms: i64) -> OrchStore {
        let store = OrchStore::open_in_memory().unwrap();
        store
            .create_run(&OrchRun {
                id: "run_1".into(),
                title: "Reconcile Run".into(),
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
            .create_dispatch(&DispatchContext {
                id: disp_id.into(),
                run_id: "run_1".into(),
                task_id: "task_a".into(),
                parent_terminal: "t1".into(),
                assignee_terminal: assignee.into(),
                terminal_created_ms: created_ms,
                turn_at_dispatch: 1,
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
    }

    #[tokio::test]
    async fn test_reconciler_1500ms_idle_turn_stop_and_working_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        fs::write(
            &fake_hc,
            "#!/usr/bin/env bash\nif [ \"$1\" = \"read\" ]; then\n  echo \"worker finished output\"\nfi\n",
        )
        .unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

        let store = setup_store_with_dispatch("disp_1", "t2", 2_000);
        let mut tracker = FleetTracker::new();

        // Worker t2 advances turn from 1 -> 2 and enters idle at t=2000ms
        tracker.apply_snapshot(
            vec![HcTerminal {
                address: "t2".into(),
                state: "live".into(),
                created_ms: Some(2_000),
                turn: 2,
                status: Some(HcStatus {
                    state: "idle".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            2_000,
        );

        // First tick at t=2000ms sets idle_since_ms = Some(2000), does not escalate yet
        let out1 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 2_000)
            .await
            .unwrap();
        assert_eq!(out1.stop_escalations, 0);
        assert!(!out1.state_changed);
        assert_eq!(
            store.get_dispatch("disp_1").unwrap().unwrap().idle_since_ms,
            Some(2_000)
        );

        // Worker resumes working at t=2500ms -> resets idle_since_ms to None
        tracker.apply_snapshot(
            vec![HcTerminal {
                address: "t2".into(),
                state: "live".into(),
                created_ms: Some(2_000),
                turn: 2,
                status: Some(HcStatus {
                    state: "working".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            2_500,
        );
        let out2 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 2_500)
            .await
            .unwrap();
        assert_eq!(out2.stop_escalations, 0);
        assert_eq!(
            store.get_dispatch("disp_1").unwrap().unwrap().idle_since_ms,
            None
        );

        // Worker goes idle again at t=3000ms
        tracker.apply_snapshot(
            vec![HcTerminal {
                address: "t2".into(),
                state: "live".into(),
                created_ms: Some(2_000),
                turn: 2,
                status: Some(HcStatus {
                    state: "done".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            3_000,
        );
        let out3 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 3_000)
            .await
            .unwrap();
        assert_eq!(out3.stop_escalations, 0);

        // At t=4400ms (1400ms < 1500ms), still no escalation
        let out4 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 4_400)
            .await
            .unwrap();
        assert_eq!(out4.stop_escalations, 0);

        // At t=4500ms (1500ms >= WORKER_DONE_QUIET_MS), escalates and transitions to NeedsReview
        let out5 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 4_500)
            .await
            .unwrap();
        assert_eq!(out5.stop_escalations, 1);
        assert!(out5.state_changed);
        assert!(store.has_message("msg_stop_disp_1_2").unwrap());
        assert_eq!(
            store.get_dispatch("disp_1").unwrap().unwrap().status,
            DispatchStatus::NeedsReview
        );
    }

    #[tokio::test]
    async fn test_reconciler_3000ms_blocked_fallback_reads_tail_only_once() {
        let tmp = tempfile::tempdir().unwrap();
        let read_count_file = tmp.path().join("read_count");
        fs::write(&read_count_file, "0").unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        let script = format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"read\" ]; then\n\
               c=$(cat \"{}\")\n\
               echo $((c + 1)) > \"{}\"\n\
               echo \"Allow cargo test? [y/N]\"\n\
             fi\n",
            read_count_file.display(),
            read_count_file.display()
        );
        fs::write(&fake_hc, script).unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

        let store = setup_store_with_dispatch("disp_1", "t2", 2_000);
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(
            vec![HcTerminal {
                address: "t2".into(),
                state: "live".into(),
                created_ms: Some(2_000),
                turn: 2,
                status: Some(HcStatus {
                    state: "blocked".into(),
                    kind: Some("permission".into()),
                    msg: Some("Allow cargo test?".into()),
                    app: None,
                }),
                status_ms: Some(2_000),
                ..Default::default()
            }],
            2_000,
        );

        // At t=2000ms, records blocked_since_ms = Some(2000)
        let out1 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 2_000)
            .await
            .unwrap();
        assert_eq!(out1.block_escalations, 0);
        assert_eq!(
            store
                .get_dispatch("disp_1")
                .unwrap()
                .unwrap()
                .blocked_since_ms,
            Some(2_000)
        );

        // At t=4900ms (< 3000ms), no escalation and 0 hc read calls
        let out2 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 4_900)
            .await
            .unwrap();
        assert_eq!(out2.block_escalations, 0);
        assert_eq!(fs::read_to_string(&read_count_file).unwrap().trim(), "0");

        // At t=5000ms (>= 3000ms), synthesizes msg_block_disp_1_2 and calls hc read once
        let out3 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 5_000)
            .await
            .unwrap();
        assert_eq!(out3.block_escalations, 1);
        assert!(out3.state_changed);
        assert!(store.has_message("msg_block_disp_1_2").unwrap());
        assert_eq!(fs::read_to_string(&read_count_file).unwrap().trim(), "1");

        // Subsequent tick at t=5500ms sees store.has_message("msg_block_disp_1_2") == true -> skips hc read!
        let out4 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 5_500)
            .await
            .unwrap();
        assert_eq!(out4.block_escalations, 0);
        assert!(!out4.state_changed);
        assert_eq!(fs::read_to_string(&read_count_file).unwrap().trim(), "1");
    }

    #[tokio::test]
    async fn test_reconciler_disappeared_or_restarted_terminal_escalates_immediately() {
        let herdr = HerdrClient::new("nonexistent-hc-bin");
        let store = setup_store_with_dispatch("disp_1", "t2", 2_000);
        let mut tracker = FleetTracker::new();

        // Terminal t2 has mismatched created_ms (restarted terminal on same ID)
        tracker.apply_snapshot(
            vec![HcTerminal {
                address: "t2".into(),
                state: "live".into(),
                created_ms: Some(9_999),
                turn: 0,
                ..Default::default()
            }],
            3_000,
        );

        let out = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 3_000)
            .await
            .unwrap();
        assert_eq!(out.stop_escalations, 1);
        assert!(out.state_changed);
        assert!(store.has_message("msg_stop_disp_1_0").unwrap());
        assert_eq!(
            store.get_dispatch("disp_1").unwrap().unwrap().status,
            DispatchStatus::NeedsReview
        );
    }

    #[tokio::test]
    async fn test_reconciler_exited_worker_includes_exit_code_and_tail_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        let script = "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"read\" ]; then\n\
               for i in $(seq 1 25); do\n\
                 echo \"output line $i\"\n\
               done\n\
               echo \"Error: unexpected argument '--bad-flag'\"\n\
               exit 0\n\
             fi\n\
             exit 1\n";
        fs::write(&fake_hc, script).unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

        let store = setup_store_with_dispatch("disp_1", "t2", 2_000);
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(
            vec![HcTerminal {
                address: "t2".into(),
                state: "finished".into(),
                created_ms: Some(2_000),
                exit_code: Some(2),
                turn: 0,
                ..Default::default()
            }],
            3_000,
        );

        let out = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 3_000)
            .await
            .unwrap();
        assert_eq!(out.stop_escalations, 1);
        assert!(out.state_changed);

        let inbox = store
            .claim_inbox_messages("t1", Some("run_1"), true, 3_100)
            .unwrap();
        assert_eq!(inbox.messages.len(), 1);
        let body = &inbox.messages[0].body;
        assert!(
            body.contains("exit_code=2"),
            "body missing exit_code=2: {body}"
        );
        assert!(
            body.contains("Error: unexpected argument '--bad-flag'"),
            "body missing error tail: {body}"
        );
        // Should cap at ~20 lines so early lines (1..6) are trimmed
        assert!(
            !body.contains("output line 1\n"),
            "body should keep only last ~20 lines: {body}"
        );
        assert!(
            body.contains("output line 25"),
            "body missing recent lines: {body}"
        );
    }

    #[tokio::test]
    async fn test_reconciler_exited_worker_succeeds_when_read_tail_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        let script = "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"read\" ]; then\n\
               echo \"cannot read terminal\" >&2\n\
               exit 1\n\
             fi\n\
             exit 1\n";
        fs::write(&fake_hc, script).unwrap();
        fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

        let store = setup_store_with_dispatch("disp_1", "t2", 2_000);
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(
            vec![HcTerminal {
                address: "t2".into(),
                state: "finished".into(),
                created_ms: Some(2_000),
                exit_code: Some(127),
                turn: 0,
                ..Default::default()
            }],
            3_000,
        );

        let out = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 3_000)
            .await
            .unwrap();
        assert_eq!(out.stop_escalations, 1);
        assert!(out.state_changed);

        let inbox = store
            .claim_inbox_messages("t1", Some("run_1"), true, 3_100)
            .unwrap();
        assert_eq!(inbox.messages.len(), 1);
        let body = &inbox.messages[0].body;
        assert!(
            body.contains("exit_code=127"),
            "body missing exit_code=127: {body}"
        );
    }
}
