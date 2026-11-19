use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tchelvis::config::{CockpitConfig, CockpitPaths};
use tchelvis::daemon::materialize_snapshot;
use tchelvis::doctor::run_doctor;
use tchelvis::herdr::{FleetTracker, HcStatus, HcTerminal, HerdrClient};
use tchelvis::model::{AgentSummaryRow, AttentionState};
use tchelvis::orch::cli::{
    create_child_worktree, execute_ask, execute_dispatch, execute_launch_with_path,
    execute_orch_command, execute_retry, execute_send, execute_send_to, execute_stop,
    map_agent_preset, parse_deps_arg, verify_and_remove_child_worktree, OrchAskArgs, OrchCommands,
    OrchDispatchArgs, OrchLaunchArgs, OrchLaunchError, OrchRetryArgs, OrchSendArgs, OrchSendToArgs,
    OrchSleeper, OrchStopArgs,
};
use tchelvis::orch::delivery::OrchDeliveryCoordinator;
use tchelvis::orch::reconciler::OrchReconciler;
use tchelvis::orch::store::OrchStore;
use tchelvis::orch::{
    DispatchContext, DispatchStatus, MessageType, OrchRun, OrchTask, PointerState, RunStatus,
    TaskStatus, BLOCKED_SUPERVISOR_GRACE_MS, POINTER_RETRY_BACKOFF_MS, WORKER_DONE_QUIET_MS,
};
use tchelvis::providers::ProviderCoordinator;
use tchelvis::store::CacheStore;

fn make_paths(root: &std::path::Path) -> CockpitPaths {
    let paths = CockpitPaths {
        home_dir: root.to_path_buf(),
        config_dir: root.join(".config/tchelvis"),
        data_dir: root.join(".local/share/tchelvis"),
        state_dir: root.join(".local/state/tchelvis"),
        runtime_dir: root.join(".local/state/tchelvis/run"),
    };
    paths.ensure_dirs().unwrap();
    paths
}

fn sample_run(id: &str, coord: &str) -> OrchRun {
    OrchRun {
        id: id.into(),
        title: format!("Run {id}"),
        coordinator_terminal: coord.into(),
        coordinator_created_ms: 1_000,
        status: RunStatus::Active,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    }
}

fn sample_task(run_id: &str, task_id: &str, deps: Vec<&str>) -> OrchTask {
    OrchTask {
        id: task_id.into(),
        run_id: run_id.into(),
        title: format!("Task {task_id}"),
        prompt: format!("Prompt for {task_id}"),
        deps: deps.into_iter().map(String::from).collect(),
        failure_count: 0,
        status: TaskStatus::Pending,
        result_summary: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    }
}

fn sample_dispatch(
    disp_id: &str,
    run_id: &str,
    task_id: &str,
    parent: &str,
    assignee: &str,
    created_ms: i64,
    turn_at_dispatch: u64,
) -> DispatchContext {
    DispatchContext {
        id: disp_id.into(),
        run_id: run_id.into(),
        task_id: task_id.into(),
        parent_terminal: parent.into(),
        assignee_terminal: assignee.into(),
        terminal_created_ms: created_ms,
        turn_at_dispatch,
        agent_kind: "claude".into(),
        worktree_mode: "current".into(),
        worktree_path: None,
        depth: 1,
        status: DispatchStatus::Dispatched,
        idle_since_ms: None,
        blocked_since_ms: None,
        created_at_ms: 1_100,
        updated_at_ms: 1_100,
    }
}

#[test]
fn scenario_1_dag_task_creation_composite_key_and_promotion_or_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = make_paths(tmp.path());
    let store = OrchStore::open(&paths.orch_db_file()).unwrap();

    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store.create_run(&sample_run("run_2", "t1")).unwrap();

    // Reuse task_a across run_1 and run_2 via composite PRIMARY KEY (run_id, id)
    let r1_a = store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    let r2_a = store
        .create_or_update_task(&sample_task("run_2", "task_a", vec![]))
        .unwrap();
    assert_eq!(r1_a.status, TaskStatus::Ready);
    assert_eq!(r2_a.status, TaskStatus::Ready);

    let r1_b = store
        .create_or_update_task(&sample_task("run_1", "task_b", vec!["task_a"]))
        .unwrap();
    let r1_c = store
        .create_or_update_task(&sample_task("run_1", "task_c", vec!["task_b"]))
        .unwrap();
    assert_eq!(r1_b.status, TaskStatus::Pending);
    assert_eq!(r1_c.status, TaskStatus::Pending);

    // Dispatch and complete task_a in run_1 -> task_b promotes to Ready
    store
        .create_dispatch(&sample_dispatch(
            "disp_a", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();
    store
        .record_worker_done("disp_a", "t2", 2_000, "succeeded", "A completed", 2_500)
        .unwrap();

    assert_eq!(
        store.get_task("run_1", "task_a").unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(
        store.get_task("run_1", "task_b").unwrap().unwrap().status,
        TaskStatus::Ready
    );
    assert_eq!(
        store.get_task("run_1", "task_c").unwrap().unwrap().status,
        TaskStatus::Pending
    );
    // run_2 task_a remains Ready
    assert_eq!(
        store.get_task("run_2", "task_a").unwrap().unwrap().status,
        TaskStatus::Ready
    );

    // Dispatch and fail task_b -> task_c propagates to Blocked
    store
        .create_dispatch(&sample_dispatch(
            "disp_b", "run_1", "task_b", "t1", "t3", 3_000, 0,
        ))
        .unwrap();
    store
        .record_worker_done("disp_b", "t3", 3_000, "failed", "B failed", 3_500)
        .unwrap();

    assert_eq!(
        store.get_task("run_1", "task_b").unwrap().unwrap().status,
        TaskStatus::Failed
    );
    assert_eq!(
        store.get_task("run_1", "task_c").unwrap().unwrap().status,
        TaskStatus::Blocked
    );
}

#[test]
fn scenario_2_attempt_fencing_rejects_mismatched_terminal_created_ms() {
    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_1", "run_1", "task_a", "t1", "t2", 10_000, 1,
        ))
        .unwrap();

    // Mismatched terminal_created_ms on worker_done
    let err = store
        .record_worker_done(
            "disp_1",
            "t2",
            99_999,
            "succeeded",
            "stale terminal",
            11_000,
        )
        .unwrap_err();
    assert!(err.to_string().contains("attempt fencing rejected"));

    // Mismatched terminal_created_ms on ask
    let ask_err = store
        .record_question("disp_1", "t2", 99_999, "Can I proceed?", 11_100)
        .unwrap_err();
    assert!(ask_err.to_string().contains("attempt fencing rejected"));

    // Matching (t2, 10_000) succeeds
    let msg = store
        .record_worker_done("disp_1", "t2", 10_000, "succeeded", "fenced ok", 11_200)
        .unwrap();
    assert_eq!(msg.msg_type, MessageType::WorkerDone);
}

#[test]
fn scenario_3_per_task_circuit_breaker_trips_after_three_failures() {
    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_b", vec!["task_a"]))
        .unwrap();

    assert_eq!(
        store
            .record_task_failure("run_1", "task_a", None, "attempt 1 failed", 2_000)
            .unwrap(),
        TaskStatus::Failed
    );
    assert_eq!(
        store
            .record_task_failure("run_1", "task_a", None, "attempt 2 failed", 3_000)
            .unwrap(),
        TaskStatus::Failed
    );
    assert_eq!(
        store
            .record_task_failure("run_1", "task_a", None, "attempt 3 failed", 4_000)
            .unwrap(),
        TaskStatus::CircuitBroken
    );

    let task_a = store.get_task("run_1", "task_a").unwrap().unwrap();
    assert_eq!(task_a.failure_count, 3);
    assert_eq!(task_a.status, TaskStatus::CircuitBroken);

    let task_b = store.get_task("run_1", "task_b").unwrap().unwrap();
    assert_eq!(task_b.status, TaskStatus::Blocked);

    let err = store
        .create_dispatch(&sample_dispatch(
            "disp_4", "run_1", "task_a", "t1", "t2", 5_000, 0,
        ))
        .unwrap_err();
    assert!(err.to_string().contains("circuit breaker tripped"));
}

#[tokio::test]
async fn scenario_4_atomic_guarded_pointer_delivery_coalescing_deferral_and_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let exit_code_file = tmp.path().join("hc_exit_code");
    let send_log = tmp.path().join("hc_send_calls.log");
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
        send_log.display(),
        exit_code_file.display()
    );
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();

    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());
    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_b", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_a", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_b", "run_1", "task_b", "t1", "t3", 3_000, 0,
        ))
        .unwrap();

    // Two workers finish concurrently before pointer delivery runs -> coalesced into 1 pointer line
    store
        .record_worker_done("disp_a", "t2", 2_000, "succeeded", "Done A", 2_100)
        .unwrap();
    store
        .record_worker_done("disp_b", "t3", 3_000, "succeeded", "Done B", 2_200)
        .unwrap();

    let mut tracker = FleetTracker::new();
    // Target t1 is Blocked -> deferred
    tracker.apply_snapshot(
        vec![HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "blocked".into(),
                kind: Some("permission".into()),
                msg: Some("Approve?".into()),
                app: None,
            }),
            status_ms: Some(2_300),
            ..Default::default()
        }],
        2_300,
    );
    let out_blocked =
        OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 2_300)
            .await
            .unwrap();
    assert_eq!(out_blocked.delivered_count, 0);
    assert_eq!(out_blocked.deferred_count, 1);

    // Target t1 is Working -> deferred
    tracker.apply_snapshot(
        vec![HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "working".into(),
                ..Default::default()
            }),
            ..Default::default()
        }],
        4_000,
    );
    let out_working =
        OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 4_000)
            .await
            .unwrap();
    assert_eq!(out_working.delivered_count, 0);
    assert_eq!(out_working.deferred_count, 1);

    // Target t1 is Idle after the 1.5s blocked-clear hysteresis (blocked until 4_000 + 1_500 = 5_500)
    // -> coalesced pointer with count=2 is delivered in 1 hc send --if idle call
    tracker.apply_snapshot(
        vec![HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "idle".into(),
                ..Default::default()
            }),
            ..Default::default()
        }],
        5_600,
    );
    let out_idle =
        OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 5_600)
            .await
            .unwrap();
    assert_eq!(out_idle.delivered_count, 1);

    let log_contents = fs::read_to_string(&send_log).unwrap();
    assert!(log_contents.contains(
        "[tchelvis] You have 2 orchestration message(s) in run run_1. Run: tchelvis orch check --run run_1"
    ));

    // Both pending messages coalesced into the single delivered pointer (pending -> delivered)
    let peeked = store
        .claim_inbox_messages("t1", Some("run_1"), true, 5_600)
        .unwrap();
    assert_eq!(peeked.messages.len(), 2);
    assert!(peeked
        .messages
        .iter()
        .all(|m| m.pointer_state == PointerState::Delivered));

    // Retries at +30s (attempt 2) and +60s (attempt 3), then stalls at +90s
    let t1 = 5_600 + POINTER_RETRY_BACKOFF_MS;
    let r1 = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, t1)
        .await
        .unwrap();
    assert_eq!(r1.delivered_count, 1);

    let t2 = t1 + POINTER_RETRY_BACKOFF_MS;
    let r2 = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, t2)
        .await
        .unwrap();
    assert_eq!(r2.delivered_count, 1);

    let t3 = t2 + POINTER_RETRY_BACKOFF_MS;
    let r3 = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, t3)
        .await
        .unwrap();
    assert_eq!(r3.stalled_count, 2);
    assert_eq!(r3.delivered_count, 0);
}

#[test]
fn scenario_5_strict_target_terminal_scoping_on_inbox_check() {
    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_1", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();

    // Worker t2 asks a question (target_terminal = t1)
    let (q, _) = store
        .record_question("disp_1", "t2", 2_000, "Which DB pool?", 2_100)
        .unwrap();

    // Child t2 checking run_1 sees 0 messages (cannot consume parent t1's inbox)
    let child_check = store
        .claim_inbox_messages("t2", Some("run_1"), false, 2_200)
        .unwrap();
    assert!(child_check.messages.is_empty());

    // Parent t1 checking run_1 claims the question message
    let parent_check = store
        .claim_inbox_messages("t1", Some("run_1"), false, 2_300)
        .unwrap();
    assert_eq!(parent_check.messages.len(), 1);
    assert_eq!(parent_check.messages[0].msg_type, MessageType::Question);

    // Parent t1 replies (target_terminal = t2)
    store
        .record_reply(&q.id, "Use rusqlite WAL", 1, 2_400)
        .unwrap();

    // Parent t1 checking run_1 sees 0 messages (cannot consume child t2's reply)
    let parent_check_2 = store
        .claim_inbox_messages("t1", Some("run_1"), false, 2_500)
        .unwrap();
    assert!(parent_check_2.messages.is_empty());

    // Child t2 checking run_1 claims the reply
    let child_check_2 = store
        .claim_inbox_messages("t2", Some("run_1"), false, 2_600)
        .unwrap();
    assert_eq!(child_check_2.messages.len(), 1);
    assert_eq!(child_check_2.messages[0].msg_type, MessageType::Reply);
    assert_eq!(child_check_2.messages[0].body, "Use rusqlite WAL");
}

#[tokio::test]
async fn scenario_6_reconciler_1500ms_turn_stop_and_3000ms_unloaded_llm_blocked_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let read_counter = tmp.path().join("read_counter");
    fs::write(&read_counter, "0").unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"read\" ]; then\n\
           c=$(cat \"{}\")\n\
           echo $((c + 1)) > \"{}\"\n\
           echo \"screen tail for $2\"\n\
         fi\n",
        read_counter.display(),
        read_counter.display()
    );
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_stop", vec![]))
        .unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_block", vec![]))
        .unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_vanish", vec![]))
        .unwrap();

    store
        .create_dispatch(&sample_dispatch(
            "disp_stop",
            "run_1",
            "task_stop",
            "t1",
            "t2",
            2_000,
            1,
        ))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_block",
            "run_1",
            "task_block",
            "t1",
            "t3",
            3_000,
            1,
        ))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_vanish",
            "run_1",
            "task_vanish",
            "t1",
            "t4",
            4_000,
            1,
        ))
        .unwrap();

    let mut tracker = FleetTracker::new();
    // t2 is idle at turn 2 (> turn_at_dispatch=1); t3 is blocked on permission; t4 is absent (disappeared)
    tracker.apply_snapshot(
        vec![
            HcTerminal {
                address: "t2".into(),
                state: "live".into(),
                created_ms: Some(2_000),
                turn: 2,
                status: Some(HcStatus {
                    state: "idle".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            HcTerminal {
                address: "t3".into(),
                state: "live".into(),
                created_ms: Some(3_000),
                turn: 2,
                status: Some(HcStatus {
                    state: "blocked".into(),
                    kind: Some("permission".into()),
                    msg: Some("Run migration? [y/N]".into()),
                    app: None,
                }),
                status_ms: Some(10_000),
                ..Default::default()
            },
        ],
        10_000,
    );

    // Tick 1 at t=10_000: t4 (vanished) escalates immediately; t2 and t3 start their timers
    let out1 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 10_000)
        .await
        .unwrap();
    assert_eq!(out1.stop_escalations, 1);
    assert_eq!(out1.block_escalations, 0);
    assert!(store.has_message("msg_stop_disp_vanish_1").unwrap());

    // Tick 2 at t=10_000 + WORKER_DONE_QUIET_MS (11_500): t2 escalates (1500ms turn-stop), t3 still in 3000ms grace
    let out2 = OrchReconciler::reconcile_active_dispatches(
        &store,
        &herdr,
        &tracker,
        10_000 + WORKER_DONE_QUIET_MS,
    )
    .await
    .unwrap();
    assert_eq!(out2.stop_escalations, 1);
    assert_eq!(out2.block_escalations, 0);
    assert!(store.has_message("msg_stop_disp_stop_2").unwrap());
    assert_eq!(
        store.get_dispatch("disp_stop").unwrap().unwrap().status,
        DispatchStatus::NeedsReview
    );

    // Tick 3 at t=10_000 + BLOCKED_SUPERVISOR_GRACE_MS (13_000): t3 synthesizes msg_block_disp_block_2
    let out3 = OrchReconciler::reconcile_active_dispatches(
        &store,
        &herdr,
        &tracker,
        10_000 + BLOCKED_SUPERVISOR_GRACE_MS,
    )
    .await
    .unwrap();
    assert_eq!(out3.block_escalations, 1);
    assert!(store.has_message("msg_block_disp_block_2").unwrap());

    // Tick 4 at t=14_000: single-shot check prevents duplicate hc read and duplicate msg_block
    let reads_before = fs::read_to_string(&read_counter)
        .unwrap()
        .trim()
        .to_string();
    let out4 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 14_000)
        .await
        .unwrap();
    let reads_after = fs::read_to_string(&read_counter)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(out4.block_escalations, 0);
    assert_eq!(reads_before, reads_after);
}

#[tokio::test]
async fn scenario_7_active_children_map_and_snapshot_projection_exclude_completed_workers() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = make_paths(tmp.path());
    let cfg = CockpitConfig::default();
    let cache_store = CacheStore::open(&paths.db_file()).unwrap();
    let orch_store = OrchStore::open(&paths.orch_db_file()).unwrap();
    let coord = ProviderCoordinator::new();

    orch_store.create_run(&sample_run("run_1", "t1")).unwrap();
    orch_store
        .create_or_update_task(&sample_task("run_1", "task_active", vec![]))
        .unwrap();
    orch_store
        .create_or_update_task(&sample_task("run_1", "task_done", vec![]))
        .unwrap();

    // t2 is actively dispatched; t3 has already completed its dispatch (waiting for opt-in release)
    orch_store
        .create_dispatch(&sample_dispatch(
            "disp_active",
            "run_1",
            "task_active",
            "t1",
            "t2",
            2_000,
            0,
        ))
        .unwrap();
    orch_store
        .create_dispatch(&sample_dispatch(
            "disp_done",
            "run_1",
            "task_done",
            "t1",
            "t3",
            3_000,
            0,
        ))
        .unwrap();
    orch_store
        .record_worker_done("disp_done", "t3", 3_000, "succeeded", "All done", 3_500)
        .unwrap();

    let mut t1_labels = BTreeMap::new();
    t1_labels.insert("cockpit.orch.role".into(), "coordinator".into());
    t1_labels.insert("cockpit.orch.run".into(), "run_1".into());

    let mut t2_labels = BTreeMap::new();
    t2_labels.insert("cockpit.orch.role".into(), "worker".into());
    t2_labels.insert("cockpit.parent".into(), "t1".into());

    let mut t3_labels = BTreeMap::new();
    t3_labels.insert("cockpit.orch.role".into(), "worker".into());
    t3_labels.insert("cockpit.parent".into(), "t1".into());

    let mut tracker = FleetTracker::new();
    tracker.apply_snapshot(
        vec![
            HcTerminal {
                address: "t1".into(),
                name: Some("coord".into()),
                state: "live".into(),
                labels: t1_labels,
                status: Some(HcStatus {
                    state: "idle".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            HcTerminal {
                address: "t2".into(),
                name: Some("worker-active".into()),
                state: "live".into(),
                labels: t2_labels,
                status: Some(HcStatus {
                    state: "working".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            HcTerminal {
                address: "t3".into(),
                name: Some("worker-done".into()),
                state: "live".into(),
                labels: t3_labels,
                status: Some(HcStatus {
                    state: "idle".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
        4_000,
    );

    let snap = materialize_snapshot(
        &mut tracker,
        &cfg,
        &paths,
        &cache_store,
        &coord,
        true,
        None,
        false,
        false,
        4_000,
    )
    .await;

    let t1_row = snap.agents.iter().find(|a| a.address == "t1").unwrap();
    assert_eq!(t1_row.attention_state, AttentionState::Idle);
    assert_eq!(t1_row.active_children, vec!["t2".to_string()]);

    let summary_row = AgentSummaryRow::from_agent_row(t1_row);
    assert_eq!(summary_row.active_children, vec!["t2".to_string()]);

    // Doctor check verifies orch_db is ok
    let doc = run_doctor(&cfg, &paths, None).await.unwrap();
    assert!(doc
        .checks
        .iter()
        .any(|c| c.name == "orch_db" && c.status == "ok"));
}

#[tokio::test]
async fn test_cli_execute_orch_commands_and_worktree_release_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = make_paths(tmp.path());
    let cfg = CockpitConfig::default();

    // 1. RunCreate with --request-id idempotency
    execute_orch_command(
        OrchCommands::RunCreate {
            title: Some("CLI Test Run".into()),
            coordinator: Some("t1".into()),
            request_id: Some("req_run_1".into()),
            json: true,
        },
        &cfg,
        &paths,
    )
    .await
    .unwrap();

    let store = OrchStore::open(&paths.orch_db_file()).unwrap();
    let runs = store.list_runs().unwrap();
    assert_eq!(runs.len(), 1);
    let run_id = runs[0].id.clone();

    // Calling RunCreate again with the same request_id is a no-op returning the cached receipt
    execute_orch_command(
        OrchCommands::RunCreate {
            title: Some("Duplicate Run".into()),
            coordinator: Some("t1".into()),
            request_id: Some("req_run_1".into()),
            json: true,
        },
        &cfg,
        &paths,
    )
    .await
    .unwrap();
    assert_eq!(store.list_runs().unwrap().len(), 1);

    // 2. TaskCreate via CLI
    execute_orch_command(
        OrchCommands::TaskCreate {
            run: run_id.clone(),
            id: "task_1".into(),
            title: "Task 1".into(),
            prompt: "Implement feature 1".into(),
            deps: None,
            request_id: Some("req_task_1".into()),
            json: true,
        },
        &cfg,
        &paths,
    )
    .await
    .unwrap();
    assert_eq!(
        store.get_task(&run_id, "task_1").unwrap().unwrap().status,
        TaskStatus::Ready
    );

    // 3. Test helper functions and git worktree creation + release safety guard
    assert_eq!(map_agent_preset("codex"), "codex --no-daemon");
    assert_eq!(map_agent_preset("claude"), "claude");
    // agy rejects a positional prompt; it reads the first prompt only via -i.
    assert_eq!(map_agent_preset("agy"), "agy -i");
    assert_eq!(map_agent_preset("antigravity"), "agy -i");
    assert_eq!(
        parse_deps_arg(Some("a, b ,c")).unwrap(),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        parse_deps_arg(Some(r#"["x","y"]"#)).unwrap(),
        vec!["x", "y"]
    );

    // Initialize a temporary git repo to verify create_child_worktree & verify_and_remove_child_worktree
    let repo_dir = tmp.path().join("sample-repo");
    fs::create_dir_all(&repo_dir).unwrap();
    Command::new("git")
        .arg("-C")
        .arg(&repo_dir)
        .arg("init")
        .output()
        .unwrap();
    Command::new("git")
        .arg("-C")
        .arg(&repo_dir)
        .args(["config", "user.email", "test@example.com"])
        .output()
        .unwrap();
    Command::new("git")
        .arg("-C")
        .arg(&repo_dir)
        .args(["config", "user.name", "Test"])
        .output()
        .unwrap();
    fs::write(repo_dir.join("README.md"), "init\n").unwrap();
    Command::new("git")
        .arg("-C")
        .arg(&repo_dir)
        .args(["add", "."])
        .output()
        .unwrap();
    Command::new("git")
        .arg("-C")
        .arg(&repo_dir)
        .args(["commit", "-m", "initial"])
        .output()
        .unwrap();

    let wt_path = create_child_worktree(&repo_dir, "run_wt", "task_wt")
        .await
        .unwrap();
    assert!(wt_path.exists());

    // Dirty worktree refuses release without --force
    fs::write(wt_path.join("dirty.txt"), "uncommitted\n").unwrap();
    let dirty_err = verify_and_remove_child_worktree(&wt_path, "run_wt", "task_wt", false)
        .await
        .unwrap_err();
    assert!(dirty_err.to_string().contains("dirty worktree"));
    fs::remove_file(wt_path.join("dirty.txt")).unwrap();

    // Clean and ancestor of HEAD -> removes cleanly without --force
    verify_and_remove_child_worktree(&wt_path, "run_wt", "task_wt", false)
        .await
        .unwrap();
    assert!(!wt_path.exists());
}

struct FakeOrchSleeper {
    now_ms: AtomicI64,
    sleep_calls: AtomicUsize,
    sleeps: Mutex<Vec<Duration>>,
    on_sleep: Option<Box<dyn Fn(usize, i64) + Send + Sync>>,
}

impl FakeOrchSleeper {
    fn new(start_ms: i64) -> Self {
        Self {
            now_ms: AtomicI64::new(start_ms),
            sleep_calls: AtomicUsize::new(0),
            sleeps: Mutex::new(Vec::new()),
            on_sleep: None,
        }
    }

    fn with_hook<F>(start_ms: i64, hook: F) -> Self
    where
        F: Fn(usize, i64) + Send + Sync + 'static,
    {
        Self {
            now_ms: AtomicI64::new(start_ms),
            sleep_calls: AtomicUsize::new(0),
            sleeps: Mutex::new(Vec::new()),
            on_sleep: Some(Box::new(hook)),
        }
    }

    fn total_slept_ms(&self) -> i64 {
        self.sleeps
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.as_millis() as i64)
            .sum()
    }
}

impl OrchSleeper for FakeOrchSleeper {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let step = self.sleep_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let next_ms = self
            .now_ms
            .fetch_add(duration.as_millis() as i64, Ordering::SeqCst)
            + duration.as_millis() as i64;
        self.sleeps.lock().unwrap().push(duration);
        if let Some(ref hook) = self.on_sleep {
            hook(step, next_ms);
        }
        Box::pin(std::future::ready(()))
    }
}

#[tokio::test]
async fn task_a_send_waits_for_delayed_dispatch_row_and_enforces_fencing() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    let script = "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{\"address\":\"t2\",\"created_ms\":2000,\"state\":\"live\",\"turn\":0,\"labels\":{}}'\n\
           exit 0\n\
         fi\n\
         exit 1\n";
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let db_path = tmp.path().join("orch.db");
    let store = OrchStore::open(&db_path).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();

    // Simulate dispatch row appearing on the 3rd poll sleep (after 75ms virtual time)
    let store_for_hook = store.clone();
    let sleeper = FakeOrchSleeper::with_hook(10_000, move |step, _| {
        if step == 3 {
            store_for_hook
                .create_dispatch(&sample_dispatch(
                    "disp_delayed",
                    "run_1",
                    "task_a",
                    "t1",
                    "t2",
                    2_000,
                    0,
                ))
                .unwrap();
        }
    });

    let msg = execute_send(
        &store,
        &herdr,
        &sleeper,
        OrchSendArgs {
            msg_type: "worker_done".into(),
            outcome: Some("succeeded".into()),
            dispatch_id: "disp_delayed".into(),
            body: "finished immediately".into(),
            request_id: None,
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(msg.msg_type, MessageType::WorkerDone);
    assert_eq!(msg.body, "finished immediately");
    assert!(sleeper.total_slept_ms() > 0);
    assert_eq!(
        store.get_task("run_1", "task_a").unwrap().unwrap().status,
        TaskStatus::Completed
    );

    // Attempt fencing must still reject a wrong caller terminal even after wait
    let spoof_err = execute_send(
        &store,
        &herdr,
        &FakeOrchSleeper::new(20_000),
        OrchSendArgs {
            msg_type: "status".into(),
            outcome: None,
            dispatch_id: "disp_delayed".into(),
            body: "spoof".into(),
            request_id: None,
            caller_terminal: Some("t99".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(spoof_err.to_string().contains("attempt fencing rejected"));

    // Missing dispatch times out bounded at 10s (virtual clock, no wall-clock sleep)
    let timeout_sleeper = FakeOrchSleeper::new(30_000);
    let missing_err = execute_send(
        &store,
        &herdr,
        &timeout_sleeper,
        OrchSendArgs {
            msg_type: "worker_done".into(),
            outcome: Some("succeeded".into()),
            dispatch_id: "disp_never_exists".into(),
            body: "orphan".into(),
            request_id: None,
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(missing_err.to_string().contains("does not exist"));
    assert_eq!(timeout_sleeper.total_slept_ms(), 10_000);
}

#[tokio::test]
async fn task_a_ask_waits_for_delayed_dispatch_row() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    let script = "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{\"address\":\"t2\",\"created_ms\":2000,\"state\":\"live\",\"turn\":0,\"labels\":{}}'\n\
           exit 0\n\
         fi\n\
         exit 1\n";
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();

    let store_for_hook = store.clone();
    let sleeper = FakeOrchSleeper::with_hook(10_000, move |step, _| {
        if step == 2 {
            store_for_hook
                .create_dispatch(&sample_dispatch(
                    "disp_ask_delayed",
                    "run_1",
                    "task_a",
                    "t1",
                    "t2",
                    2_000,
                    0,
                ))
                .unwrap();
        }
    });

    let (q_thread, msg) = execute_ask(
        &store,
        &herdr,
        &sleeper,
        OrchAskArgs {
            dispatch_id: "disp_ask_delayed".into(),
            question: "Should I proceed?".into(),
            request_id: None,
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(q_thread.status, "open");
    assert_eq!(msg.msg_type, MessageType::Question);
    assert!(sleeper.total_slept_ms() > 0);
}

#[tokio::test]
async fn task_b_dispatch_rechecks_liveness_and_reports_launch_failure_with_exit_code_and_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let get_count_file = tmp.path().join("get_count");
    fs::write(&get_count_file, "0").unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    // run -> live (no created_ms, matching real hc 0.1.21)
    // get #1 (inside run_worker) -> live with created_ms=5000
    // get #2 (inside liveness re-check <=2s) -> finished with exit_code=2
    // read -> 25 lines ending in unexpected argument error
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo '{{\"address\":\"t5\",\"agent\":\"agy\",\"cwd\":\"/tmp\",\"state\":\"live\",\"turn\":0}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"get\" ]; then\n\
           if [ \"$2\" = \"t1\" ]; then\n\
             echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"/tmp\"}}'\n\
             exit 0\n\
           fi\n\
           c=$(cat \"{}\")\n\
           c=$((c + 1))\n\
           echo \"$c\" > \"{}\"\n\
           if [ \"$c\" -eq 1 ]; then\n\
             echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"labels\":{{}}}}'\n\
           else\n\
             echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"finished\",\"exit_code\":2,\"turn\":0,\"labels\":{{}}}}'\n\
           fi\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"read\" ]; then\n\
           for i in $(seq 1 25); do\n\
             echo \"boot line $i\"\n\
           done\n\
           echo \"Error: unexpected argument '--bad'\"\n\
           exit 0\n\
         fi\n\
         exit 0\n",
        get_count_file.display(),
        get_count_file.display(),
    );
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();

    let sleeper = FakeOrchSleeper::new(10_000);
    let out = execute_dispatch(
        &store,
        &herdr,
        &sleeper,
        OrchDispatchArgs {
            run: "run_1".into(),
            task: "task_a".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("agy".into()),
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    assert_ne!(out.dispatch.status, DispatchStatus::Dispatched);
    assert_eq!(out.dispatch.status, DispatchStatus::NeedsReview);
    assert_eq!(out.exit_code, Some(2));
    let tail = out.output_tail.as_deref().unwrap_or("");
    assert!(
        tail.contains("Error: unexpected argument '--bad'"),
        "expected error tail, got: {tail}"
    );
    assert!(
        !tail.contains("boot line 1\n"),
        "tail should keep ~20 lines: {tail}"
    );
    let reason = out.failure_reason.as_deref().unwrap_or("");
    assert!(
        reason.contains("exit_code=2"),
        "reason missing exit_code=2: {reason}"
    );
    assert!(
        reason.contains("Error: unexpected argument '--bad'"),
        "reason missing tail: {reason}"
    );

    let task = store.get_task("run_1", "task_a").unwrap().unwrap();
    assert_eq!(task.failure_count, 1);
    assert_eq!(task.status, TaskStatus::Failed);
    let inbox = store
        .claim_inbox_messages("t1", Some("run_1"), true, 12_000)
        .unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].msg_type, MessageType::Escalation);
    assert!(sleeper.total_slept_ms() <= 2_000);
}

#[tokio::test]
async fn task_b_dispatch_does_not_overwrite_fast_completed_worker_as_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let get_count_file = tmp.path().join("get_count");
    fs::write(&get_count_file, "0").unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    let script = "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo '{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"first_message\":{\"sent\":true}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"get\" ]; then\n\
           if [ \"$2\" = \"t1\" ]; then\n\
             echo '{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"/tmp\"}'\n\
             exit 0\n\
           fi\n\
           echo '{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"finished\",\"exit_code\":0,\"turn\":1,\"labels\":{}}'\n\
           exit 0\n\
         fi\n\
         exit 0\n";
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();

    // Simulate the worker reporting worker_done during the 1st liveness re-check sleep
    let store_for_hook = store.clone();
    let sleeper = FakeOrchSleeper::with_hook(10_000, move |step, now_ms| {
        if step == 1 {
            let active = store_for_hook.list_active_dispatches().unwrap();
            if let Some(d) = active.first() {
                store_for_hook
                    .record_worker_done(&d.id, "t5", 5_000, "succeeded", "fast worker done", now_ms)
                    .unwrap();
            }
        }
    });

    let out = execute_dispatch(
        &store,
        &herdr,
        &sleeper,
        OrchDispatchArgs {
            run: "run_1".into(),
            task: "task_a".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("claude".into()),
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(out.dispatch.status, DispatchStatus::Completed);
    assert!(out.failure_reason.is_none());
    let task = store.get_task("run_1", "task_a").unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.failure_count, 0);
}

#[tokio::test]
async fn task_b_dispatch_healthy_worker_stays_dispatched_after_2s_recheck() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    let script = "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo '{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"first_message\":{\"sent\":true}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"get\" ]; then\n\
           if [ \"$2\" = \"t1\" ]; then\n\
             echo '{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"/tmp\"}'\n\
             exit 0\n\
           fi\n\
           echo '{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"labels\":{}}'\n\
           exit 0\n\
         fi\n\
         exit 0\n";
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();

    let sleeper = FakeOrchSleeper::new(10_000);
    let out = execute_dispatch(
        &store,
        &herdr,
        &sleeper,
        OrchDispatchArgs {
            run: "run_1".into(),
            task: "task_a".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("claude".into()),
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(out.dispatch.status, DispatchStatus::Dispatched);
    assert!(out.failure_reason.is_none());
    assert!(out.exit_code.is_none());
    assert_eq!(sleeper.total_slept_ms(), 2_000);
}

#[tokio::test]
async fn task_b_dispatch_failed_read_tail_does_not_fail_failure_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    let script = "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo '{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"finished\",\"exit_code\":3,\"turn\":0,\"first_message\":{\"sent\":true}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"/tmp\"}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"read\" ]; then\n\
           echo \"read error\" >&2\n\
           exit 1\n\
         fi\n\
         exit 0\n";
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();

    let sleeper = FakeOrchSleeper::new(10_000);
    let out = execute_dispatch(
        &store,
        &herdr,
        &sleeper,
        OrchDispatchArgs {
            run: "run_1".into(),
            task: "task_a".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("agy".into()),
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(out.dispatch.status, DispatchStatus::NeedsReview);
    assert_eq!(out.exit_code, Some(3));
    assert!(out.output_tail.is_none());
    assert!(out
        .failure_reason
        .as_deref()
        .unwrap_or("")
        .contains("exit_code=3"));
}

fn init_temp_git_repo(repo_dir: &std::path::Path) {
    fs::create_dir_all(repo_dir).unwrap();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo_dir)
            .args(&args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed", args);
    }
    fs::write(repo_dir.join("README.md"), "init\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-m", "initial"]] {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo_dir)
            .args(&args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed", args);
    }
}

#[tokio::test]
async fn lot_d_failed_dispatch_new_child_cleans_up_clean_worktree_and_closes_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    init_temp_git_repo(&repo_dir);

    // Case A: liveness re-check detects worker exited with code 2
    let get_count_file = tmp.path().join("get_count_a");
    let term_ops_log = tmp.path().join("term_ops_a.log");
    fs::write(&get_count_file, "0").unwrap();
    let fake_hc_a = tmp.path().join("fake-hc-a");
    let script_a = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo '{{\"address\":\"t5\",\"agent\":\"agy\",\"cwd\":\"/tmp\",\"state\":\"live\",\"turn\":0}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"get\" ]; then\n\
           if [ \"$2\" = \"t1\" ]; then\n\
             echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"{}\"}}'\n\
             exit 0\n\
           fi\n\
           c=$(cat \"{}\")\n\
           c=$((c + 1))\n\
           echo \"$c\" > \"{}\"\n\
           if [ \"$c\" -eq 1 ]; then\n\
             echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"labels\":{{}}}}'\n\
           else\n\
             echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"finished\",\"exit_code\":2,\"turn\":0,\"labels\":{{}}}}'\n\
           fi\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"kill\" ] || [ \"$1\" = \"rm\" ]; then\n\
           echo \"$1 $2\" >> \"{}\"\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"read\" ]; then\n\
           echo \"Error: failed\"\n\
           exit 0\n\
         fi\n\
         exit 0\n",
        repo_dir.display(),
        get_count_file.display(),
        get_count_file.display(),
        term_ops_log.display(),
    );
    fs::write(&fake_hc_a, script_a).unwrap();
    fs::set_permissions(&fake_hc_a, fs::Permissions::from_mode(0o755)).unwrap();

    let store = OrchStore::open(&tmp.path().join("orch_a.db")).unwrap();
    store.create_run(&sample_run("run_a", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_a", "task_a", vec![]))
        .unwrap();

    let out_a = execute_dispatch(
        &store,
        &HerdrClient::new(fake_hc_a.to_string_lossy().to_string()),
        &FakeOrchSleeper::new(10_000),
        OrchDispatchArgs {
            run: "run_a".into(),
            task: "task_a".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("agy".into()),
            worktree: "new-child".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(out_a.dispatch.status, DispatchStatus::NeedsReview);
    let wt_a = repo_dir.join(".work/orch-run_a-task_a");
    assert!(
        !wt_a.exists(),
        "clean child worktree should be removed on failed launch"
    );
    let ops_a = fs::read_to_string(&term_ops_log).unwrap_or_default();
    assert!(
        ops_a.contains("kill t5"),
        "expected hc kill t5, got: {ops_a}"
    );
    assert!(ops_a.contains("rm t5"), "expected hc rm t5, got: {ops_a}");

    // Case B: first_message.sent == false (trust dialog)
    let term_ops_b = tmp.path().join("term_ops_b.log");
    let fake_hc_b = tmp.path().join("fake-hc-b");
    let script_b = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo '{{\"address\":\"t6\",\"created_ms\":6000,\"state\":\"live\",\"turn\":0,\"first_message\":{{\"sent\":false,\"reason\":\"trust_dialog\"}}}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"{}\"}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"kill\" ] || [ \"$1\" = \"rm\" ]; then\n\
           echo \"$1 $2\" >> \"{}\"\n\
           exit 0\n\
         fi\n\
         exit 0\n",
        repo_dir.display(),
        term_ops_b.display(),
    );
    fs::write(&fake_hc_b, script_b).unwrap();
    fs::set_permissions(&fake_hc_b, fs::Permissions::from_mode(0o755)).unwrap();

    store
        .create_or_update_task(&sample_task("run_a", "task_b", vec![]))
        .unwrap();
    let out_b = execute_dispatch(
        &store,
        &HerdrClient::new(fake_hc_b.to_string_lossy().to_string()),
        &FakeOrchSleeper::new(20_000),
        OrchDispatchArgs {
            run: "run_a".into(),
            task: "task_b".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("claude".into()),
            worktree: "new-child".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(out_b.dispatch.status, DispatchStatus::NeedsReview);
    assert!(!repo_dir.join(".work/orch-run_a-task_b").exists());
    let ops_b = fs::read_to_string(&term_ops_b).unwrap_or_default();
    assert!(
        ops_b.contains("kill t6"),
        "expected hc kill t6, got: {ops_b}"
    );
    assert!(ops_b.contains("rm t6"), "expected hc rm t6, got: {ops_b}");

    // Case C: run_worker errors after create_child_worktree
    let fake_hc_c = tmp.path().join("fake-hc-c");
    let script_c = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"{}\"}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo 'agent binary not found' >&2\n\
           exit 1\n\
         fi\n\
         exit 1\n",
        repo_dir.display(),
    );
    fs::write(&fake_hc_c, script_c).unwrap();
    fs::set_permissions(&fake_hc_c, fs::Permissions::from_mode(0o755)).unwrap();

    store
        .create_or_update_task(&sample_task("run_a", "task_c", vec![]))
        .unwrap();
    let err_c = execute_dispatch(
        &store,
        &HerdrClient::new(fake_hc_c.to_string_lossy().to_string()),
        &FakeOrchSleeper::new(30_000),
        OrchDispatchArgs {
            run: "run_a".into(),
            task: "task_c".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("claude".into()),
            worktree: "new-child".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(err_c.to_string().contains("hc run failed"));
    assert!(
        !repo_dir.join(".work/orch-run_a-task_c").exists(),
        "clean worktree must be removed even when run_worker errors"
    );
}

#[tokio::test]
async fn lot_d_failed_dispatch_new_child_keeps_dirty_worktree_and_reports_it() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    init_temp_git_repo(&repo_dir);

    let fake_hc = tmp.path().join("fake-hc");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"{}\"}}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"run\" ]; then\n\
           cwd=\"\"\n\
           while [ \"$#\" -gt 0 ]; do\n\
             if [ \"$1\" = \"--cwd\" ]; then cwd=\"$2\"; shift 2; continue; fi\n\
             shift\n\
           done\n\
           echo 'partial work' > \"$cwd/wip.txt\"\n\
           echo '{{\"address\":\"t7\",\"created_ms\":7000,\"state\":\"finished\",\"exit_code\":1,\"turn\":0,\"first_message\":{{\"sent\":true}}}}'\n\
           exit 0\n\
         fi\n\
         exit 0\n",
        repo_dir.display(),
    );
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_d", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_d", "task_dirty", vec![]))
        .unwrap();

    let out = execute_dispatch(
        &store,
        &HerdrClient::new(fake_hc.to_string_lossy().to_string()),
        &FakeOrchSleeper::new(10_000),
        OrchDispatchArgs {
            run: "run_d".into(),
            task: "task_dirty".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("claude".into()),
            worktree: "new-child".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    let wt = repo_dir.join(".work/orch-run_d-task_dirty");
    assert!(wt.exists(), "dirty worktree must never be force-deleted");
    assert!(wt.join("wip.txt").exists());
    let reason = out.failure_reason.as_deref().unwrap_or("");
    assert!(
        reason.contains("worktree") && reason.contains("dirty"),
        "failure_reason should explain why dirty worktree was kept: {reason}"
    );
}

#[tokio::test]
async fn lot_d_send_and_ask_reject_missing_caller_identity_or_hc_lookup_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let failing_hc = tmp.path().join("failing-hc");
    fs::write(&failing_hc, "#!/usr/bin/env bash\nexit 1\n").unwrap();
    fs::set_permissions(&failing_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let failing_herdr = HerdrClient::new(failing_hc.to_string_lossy().to_string());

    let working_hc = tmp.path().join("working-hc");
    fs::write(
        &working_hc,
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{\"address\":\"t2\",\"created_ms\":2000,\"state\":\"live\",\"turn\":0,\"labels\":{}}'\n\
           exit 0\n\
         fi\n\
         exit 1\n",
    )
    .unwrap();
    fs::set_permissions(&working_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let working_herdr = HerdrClient::new(working_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_1", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();

    // Missing caller identity on send (no --caller and no HC_TERMINAL_ID) must error, not borrow disp.assignee_terminal
    if std::env::var_os("HC_TERMINAL_ID").is_none() {
        let send_no_caller = execute_send(
            &store,
            &working_herdr,
            &FakeOrchSleeper::new(10_000),
            OrchSendArgs {
                msg_type: "worker_done".into(),
                outcome: Some("succeeded".into()),
                dispatch_id: "disp_1".into(),
                body: "done".into(),
                request_id: None,
                caller_terminal: None,
            },
        )
        .await
        .unwrap_err();
        assert!(
            send_no_caller.to_string().contains("caller")
                || send_no_caller.to_string().contains("HC_TERMINAL_ID"),
            "unexpected error: {send_no_caller}"
        );

        let ask_no_caller = execute_ask(
            &store,
            &working_herdr,
            &FakeOrchSleeper::new(10_000),
            OrchAskArgs {
                dispatch_id: "disp_1".into(),
                question: "hi?".into(),
                request_id: None,
                caller_terminal: None,
            },
        )
        .await
        .unwrap_err();
        assert!(
            ask_no_caller.to_string().contains("caller")
                || ask_no_caller.to_string().contains("HC_TERMINAL_ID"),
            "unexpected error: {ask_no_caller}"
        );
    }

    // hc lookup failure on send/ask must error, not borrow disp.terminal_created_ms
    let send_hc_fail = execute_send(
        &store,
        &failing_herdr,
        &FakeOrchSleeper::new(10_000),
        OrchSendArgs {
            msg_type: "worker_done".into(),
            outcome: Some("succeeded".into()),
            dispatch_id: "disp_1".into(),
            body: "done".into(),
            request_id: None,
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        send_hc_fail.to_string().contains("fencing") || send_hc_fail.to_string().contains("hc"),
        "unexpected error: {send_hc_fail}"
    );

    let ask_hc_fail = execute_ask(
        &store,
        &failing_herdr,
        &FakeOrchSleeper::new(10_000),
        OrchAskArgs {
            dispatch_id: "disp_1".into(),
            question: "hi?".into(),
            request_id: None,
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        ask_hc_fail.to_string().contains("fencing") || ask_hc_fail.to_string().contains("hc"),
        "unexpected error: {ask_hc_fail}"
    );

    // Dispatch must still be untouched
    assert_eq!(
        store.get_dispatch("disp_1").unwrap().unwrap().status,
        DispatchStatus::Dispatched
    );
}

#[tokio::test]
async fn lot_d_delivery_error_on_first_target_defers_and_still_delivers_to_second_target() {
    let tmp = tempfile::tempdir().unwrap();
    let sent_log = tmp.path().join("sent.log");
    let fake_hc = tmp.path().join("fake-hc");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"send\" ]; then\n\
           # args: send --if idle <terminal> <text> --json\n\
           target=\"$4\"\n\
           if [ \"$target\" = \"t1\" ]; then\n\
             echo \"broken pipe\" >&2\n\
             exit 1\n\
           fi\n\
           echo \"$target: $5\" >> \"{}\"\n\
           exit 0\n\
         fi\n\
         exit 0\n",
        sent_log.display()
    );
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store.create_run(&sample_run("run_2", "t9")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_or_update_task(&sample_task("run_2", "task_b", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_1", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_2", "run_2", "task_b", "t9", "t3", 3_000, 0,
        ))
        .unwrap();
    store
        .record_worker_done("disp_1", "t2", 2_000, "succeeded", "Done 1", 4_000)
        .unwrap();
    store
        .record_worker_done("disp_2", "t3", 3_000, "succeeded", "Done 2", 4_100)
        .unwrap();

    let mut tracker = FleetTracker::new();
    tracker.apply_snapshot(
        vec![
            HcTerminal {
                address: "t1".into(),
                state: "live".into(),
                status: Some(HcStatus {
                    state: "idle".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            HcTerminal {
                address: "t9".into(),
                state: "live".into(),
                status: Some(HcStatus {
                    state: "idle".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
        4_200,
    );

    let out = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 4_200)
        .await
        .unwrap();
    assert_eq!(out.delivered_count, 1);
    assert_eq!(out.deferred_count, 1);

    let sent = fs::read_to_string(&sent_log).unwrap();
    assert!(
        sent.contains("t9:"),
        "expected t9 pointer sent, got: {sent}"
    );

    // t1 remains pending for retry while t9 is delivered
    let t1_msg = store
        .claim_inbox_messages("t1", Some("run_1"), true, 4_300)
        .unwrap();
    assert_eq!(t1_msg.messages[0].pointer_state, PointerState::Pending);
    let t9_msg = store
        .claim_inbox_messages("t9", Some("run_2"), true, 4_300)
        .unwrap();
    assert_eq!(t9_msg.messages[0].pointer_state, PointerState::Delivered);
}

#[tokio::test]
async fn lot_d_reconciler_does_not_false_escalate_missing_terminal_within_grace_or_missing_created_ms(
) {
    let tmp = tempfile::tempdir().unwrap();
    let get_mode = tmp.path().join("get_mode");
    fs::write(&get_mode, "missing").unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           mode=$(cat \"{}\")\n\
           if [ \"$mode\" = \"live\" ]; then\n\
             echo '{{\"address\":\"t2\",\"created_ms\":2000,\"state\":\"live\",\"turn\":0,\"labels\":{{}}}}'\n\
             exit 0\n\
           fi\n\
           exit 1\n\
         fi\n\
         exit 0\n",
        get_mode.display()
    );
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    let mut disp = sample_dispatch("disp_fresh", "run_1", "task_a", "t1", "t2", 2_000, 0);
    disp.created_at_ms = 10_000;
    store.create_dispatch(&disp).unwrap();

    // 1. Right after dispatch (now=10_500), t2 is not yet in FleetTracker -> must NOT false-escalate
    let mut tracker = FleetTracker::new();
    let out1 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 10_500)
        .await
        .unwrap();
    assert_eq!(out1.stop_escalations, 0);
    assert_eq!(
        store.get_dispatch("disp_fresh").unwrap().unwrap().status,
        DispatchStatus::Dispatched
    );

    // 2. Partial event creates t2 in FleetTracker with created_ms: None -> must NOT count as recreated
    let ev = tchelvis::herdr::parse_hc_event_line(
        r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"working"},"seq":1,"terminal":"t2","type":"status"}}"#,
    )
    .unwrap();
    tracker.apply_stream_item(ev, 10_800);
    assert_eq!(tracker.terminals.get("t2").unwrap().created_ms, None);

    let out2 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &tracker, 12_000)
        .await
        .unwrap();
    assert_eq!(out2.stop_escalations, 0);
    assert_eq!(
        store.get_dispatch("disp_fresh").unwrap().unwrap().status,
        DispatchStatus::Dispatched
    );

    // 3. Even after grace window (now=20_000), if t2 is missing from tracker but `hc get` says live -> do NOT escalate
    let empty_tracker = FleetTracker::new();
    fs::write(&get_mode, "live").unwrap();
    let out3 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &empty_tracker, 20_000)
        .await
        .unwrap();
    assert_eq!(out3.stop_escalations, 0);

    // 4. After grace window when `hc get` confirms missing -> escalate as terminal_missing
    fs::write(&get_mode, "missing").unwrap();
    let out4 = OrchReconciler::reconcile_active_dispatches(&store, &herdr, &empty_tracker, 20_000)
        .await
        .unwrap();
    assert_eq!(out4.stop_escalations, 1);
    assert!(store.has_message("msg_stop_disp_fresh_0").unwrap());
}

#[tokio::test]
async fn lot_d_duplicate_worker_done_is_rejected_and_preserves_single_inbox_message() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_hc = tmp.path().join("fake-hc");
    fs::write(
        &fake_hc,
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{\"address\":\"t2\",\"created_ms\":2000,\"state\":\"live\",\"turn\":1,\"labels\":{}}'\n\
           exit 0\n\
         fi\n\
         exit 1\n",
    )
    .unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_b", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_ok", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_fail",
            "run_1",
            "task_b",
            "t1",
            "t2",
            2_000,
            0,
        ))
        .unwrap();

    // First worker_done via execute_send with request_id succeeds
    let first = execute_send(
        &store,
        &herdr,
        &FakeOrchSleeper::new(5_000),
        OrchSendArgs {
            msg_type: "worker_done".into(),
            outcome: Some("succeeded".into()),
            dispatch_id: "disp_ok".into(),
            body: "first report".into(),
            request_id: Some("req_wd_1".into()),
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(first.body, "first report");

    // Retry with same request_id returns cached receipt
    let retry = execute_send(
        &store,
        &herdr,
        &FakeOrchSleeper::new(5_100),
        OrchSendArgs {
            msg_type: "worker_done".into(),
            outcome: Some("succeeded".into()),
            dispatch_id: "disp_ok".into(),
            body: "first report".into(),
            request_id: Some("req_wd_1".into()),
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(retry.id, first.id);

    // Second worker_done without same request_id is rejected
    let dup_err = store
        .record_worker_done("disp_ok", "t2", 2_000, "succeeded", "second report", 5_200)
        .unwrap_err();
    assert!(
        dup_err.to_string().contains("already") || dup_err.to_string().contains("once"),
        "expected clear duplicate rejection error, got: {dup_err}"
    );

    // Failed dispatch: second worker_done must not increment failure_count a second time
    store
        .record_worker_done("disp_fail", "t2", 2_000, "failed", "fail 1", 6_000)
        .unwrap();
    let dup_fail_err = store
        .record_worker_done("disp_fail", "t2", 2_000, "failed", "fail 2", 6_100)
        .unwrap_err();
    assert!(
        dup_fail_err.to_string().contains("already") || dup_fail_err.to_string().contains("once")
    );
    assert_eq!(
        store
            .get_task("run_1", "task_b")
            .unwrap()
            .unwrap()
            .failure_count,
        1
    );

    // Inbox for t1 has exactly 2 messages (one per dispatch), not 4
    let inbox = store
        .claim_inbox_messages("t1", Some("run_1"), true, 6_200)
        .unwrap();
    assert_eq!(inbox.messages.len(), 2);

    // Worker preamble instructs reporting worker_done once
    let preamble = tchelvis::orch::preamble::build_dispatch_preamble(
        "run_1", "task_a", "disp_ok", "t1", 1, "Do work",
    );
    assert!(
        preamble.to_ascii_lowercase().contains("once"),
        "preamble should instruct reporting worker_done once: {preamble}"
    );
}

#[tokio::test]
async fn lot_d_coordinator_claude_done_receives_worker_done_pointer_after_deferral_and_earlier_pointer(
) {
    let tmp = tempfile::tempdir().unwrap();
    let send_log = tmp.path().join("hc_send.log");
    let fake_hc = tmp.path().join("fake-hc");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"send\" ]; then\n\
           echo \"$*\" >> \"{}\"\n\
           exit 0\n\
         fi\n\
         exit 0\n",
        send_log.display()
    );
    fs::write(&fake_hc, script).unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open_in_memory().unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_b", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_a", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_b", "run_1", "task_b", "t1", "t3", 3_000, 0,
        ))
        .unwrap();

    // Coordinator t1 (Claude Code) is currently Working when an escalation occurs on disp_a
    let mut tracker = FleetTracker::new();
    tracker.apply_snapshot(
        vec![HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "working".into(),
                app: Some("claude".into()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        5_000,
    );
    store
        .record_turn_stop_escalation("disp_a", 0, "worker_exited_or_errored", "", 5_000)
        .unwrap();

    // Tick 1 while Working -> deferred
    let d1 = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 5_100)
        .await
        .unwrap();
    assert_eq!(d1.delivered_count, 0);
    assert_eq!(d1.deferred_count, 1);

    // Claude Code Stop hook fires: state=done, app=claude -> pointer is delivered
    tracker.apply_snapshot(
        vec![HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "done".into(),
                app: Some("claude".into()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        6_000,
    );
    let d2 = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 6_000)
        .await
        .unwrap();
    assert_eq!(d2.delivered_count, 1);

    // Later (t=10_000), worker_done arrives for disp_b while msg_1 pointer is unexpired (until 36_000)
    store
        .record_worker_done("disp_b", "t3", 3_000, "succeeded", "Done B", 10_000)
        .unwrap();
    let d3 = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 10_000)
        .await
        .unwrap();
    assert_eq!(d3.delivered_count, 0);

    // Once backoff expires at 36_000, has_active_unexpired_pointer clears and both messages are delivered
    let d4 = OrchDeliveryCoordinator::deliver_pending_pointers(
        &store,
        &herdr,
        &tracker,
        6_000 + POINTER_RETRY_BACKOFF_MS,
    )
    .await
    .unwrap();
    assert_eq!(d4.delivered_count, 1);

    let log_text = fs::read_to_string(&send_log).unwrap();
    assert!(
        log_text.contains("You have 2 orchestration message(s) in run run_1"),
        "expected coalesced pointer with 2 messages once unexpired pointer expired: {log_text}"
    );
}

#[tokio::test]
async fn lot_f_launch_missing_agent_fails_agent_preflight_and_creates_no_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let empty_bin = tmp.path().join("empty_bin");
    fs::create_dir_all(&empty_bin).unwrap();
    let hc_log = tmp.path().join("hc_calls.log");
    let fake_hc = tmp.path().join("fake-hc");
    fs::write(
        &fake_hc,
        format!(
            "#!/usr/bin/env bash\n\
             echo \"$*\" >> \"{}\"\n\
             if [ \"$1\" = \"get\" ]; then\n\
               echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"/tmp\"}}'\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            hc_log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    let err = execute_launch_with_path(
        &store,
        &herdr,
        &FakeOrchSleeper::new(10_000),
        OrchLaunchArgs {
            agent: "nonexistent-xyz".into(),
            prompt: "Do work".into(),
            title: Some("Bad agent launch".into()),
            run: None,
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
        Some(empty_bin.as_os_str()),
    )
    .await
    .unwrap_err();

    let launch_err = err
        .downcast_ref::<OrchLaunchError>()
        .expect("error should downcast to OrchLaunchError");
    assert_eq!(launch_err.failed_stage, "agent_preflight");
    assert_eq!(launch_err.agent, "nonexistent-xyz");
    assert!(!launch_err.detail.is_empty());

    // Verify JSON serialization shape {failed_stage, agent, detail}
    let val = serde_json::to_value(launch_err).unwrap();
    assert_eq!(val["failed_stage"], "agent_preflight");
    assert_eq!(val["agent"], "nonexistent-xyz");
    assert!(val["detail"].as_str().is_some_and(|s| !s.is_empty()));

    // No run, no dispatch, and no hc run spawn must have been performed
    assert!(store.list_runs().unwrap().is_empty());
    assert!(store.list_active_dispatches().unwrap().is_empty());
    let calls = fs::read_to_string(&hc_log).unwrap_or_default();
    assert!(
        !calls.contains("run "),
        "hc run must not run on preflight fail: {calls}"
    );
}

#[tokio::test]
async fn lot_f_launch_happy_path_creates_run_task_dispatch_and_reuses_existing_run() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake_agent = bin_dir.join("claude");
    fs::write(&fake_agent, "#!/usr/bin/env sh\nexit 0\n").unwrap();
    fs::set_permissions(&fake_agent, fs::Permissions::from_mode(0o755)).unwrap();

    let repo_dir = tmp.path().join("repo");
    init_temp_git_repo(&repo_dir);

    let fake_hc = tmp.path().join("fake-hc");
    fs::write(
        &fake_hc,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"get\" ]; then\n\
               if [ \"$2\" = \"t1\" ]; then\n\
                 echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"{}\"}}'\n\
                 exit 0\n\
               fi\n\
               echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"labels\":{{}}}}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"run\" ]; then\n\
               echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"first_message\":{{\"sent\":true}}}}'\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            repo_dir.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());
    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();

    // 1. Without --run: creates run, task, and dispatch in one call
    let out1 = execute_launch_with_path(
        &store,
        &herdr,
        &FakeOrchSleeper::new(10_000),
        OrchLaunchArgs {
            agent: "claude".into(),
            prompt: "Implement feature X".into(),
            title: Some("Feature X".into()),
            run: None,
            worktree: "new-child".into(),
            request_id: Some("req_launch_1".into()),
            caller_terminal: Some("t1".into()),
        },
        Some(bin_dir.as_os_str()),
    )
    .await
    .unwrap();

    assert_eq!(out1.run.coordinator_terminal, "t1");
    assert_eq!(out1.run.title, "Feature X");
    assert_eq!(out1.task.run_id, out1.run.id);
    assert_eq!(out1.task.prompt, "Implement feature X");
    assert_eq!(out1.dispatch.run_id, out1.run.id);
    assert_eq!(out1.dispatch.task_id, out1.task.id);
    assert_eq!(out1.dispatch.assignee_terminal, "t5");
    assert_eq!(out1.dispatch.status, DispatchStatus::Dispatched);
    assert_eq!(out1.dispatch.worktree_mode, "new-child");
    assert!(out1
        .dispatch
        .worktree_path
        .as_deref()
        .is_some_and(|p| std::path::Path::new(p).exists()));

    // Request-id idempotency returns same output without spawning a 2nd time
    let out1_dup = execute_launch_with_path(
        &store,
        &herdr,
        &FakeOrchSleeper::new(10_100),
        OrchLaunchArgs {
            agent: "claude".into(),
            prompt: "Implement feature X".into(),
            title: Some("Feature X".into()),
            run: None,
            worktree: "new-child".into(),
            request_id: Some("req_launch_1".into()),
            caller_terminal: Some("t1".into()),
        },
        Some(bin_dir.as_os_str()),
    )
    .await
    .unwrap();
    assert_eq!(out1_dup.dispatch.id, out1.dispatch.id);
    assert_eq!(store.list_runs().unwrap().len(), 1);

    // 2. With --run: reuses existing run and creates a second task & dispatch
    let out2 = execute_launch_with_path(
        &store,
        &herdr,
        &FakeOrchSleeper::new(12_000),
        OrchLaunchArgs {
            agent: "claude".into(),
            prompt: "Implement feature Y".into(),
            title: Some("Feature Y".into()),
            run: Some(out1.run.id.clone()),
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
        Some(bin_dir.as_os_str()),
    )
    .await
    .unwrap();

    assert_eq!(out2.run.id, out1.run.id);
    assert_ne!(out2.task.id, out1.task.id);
    assert_ne!(out2.dispatch.id, out1.dispatch.id);
    assert_eq!(store.list_runs().unwrap().len(), 1);
    assert_eq!(store.list_tasks(&out1.run.id).unwrap().len(), 2);
}

#[tokio::test]
async fn lot_f_launch_spawn_and_liveness_failures_report_failed_stage_exit_code_and_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake_agent = bin_dir.join("agy");
    fs::write(&fake_agent, "#!/usr/bin/env sh\nexit 0\n").unwrap();
    fs::set_permissions(&fake_agent, fs::Permissions::from_mode(0o755)).unwrap();

    // Case A: liveness re-check detects worker died with exit_code=2 and output tail
    let get_count_file = tmp.path().join("get_count");
    fs::write(&get_count_file, "0").unwrap();
    let fake_hc_liveness = tmp.path().join("fake-hc-live");
    fs::write(
        &fake_hc_liveness,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"run\" ]; then\n\
               echo '{{\"address\":\"t5\",\"agent\":\"agy\",\"cwd\":\"/tmp\",\"state\":\"live\",\"turn\":0}}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"get\" ]; then\n\
               if [ \"$2\" = \"t1\" ]; then\n\
                 echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"/tmp\"}}'\n\
                 exit 0\n\
               fi\n\
               c=$(cat \"{}\")\n\
               c=$((c + 1))\n\
               echo \"$c\" > \"{}\"\n\
               if [ \"$c\" -eq 1 ]; then\n\
                 echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"labels\":{{}}}}'\n\
               else\n\
                 echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"finished\",\"exit_code\":2,\"turn\":0,\"labels\":{{}}}}'\n\
               fi\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"read\" ]; then\n\
               echo 'Fatal: bad flag --xyz'\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            get_count_file.display(),
            get_count_file.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_hc_liveness, fs::Permissions::from_mode(0o755)).unwrap();

    let store = OrchStore::open(&tmp.path().join("orch_live.db")).unwrap();
    let err_launch = execute_launch_with_path(
        &store,
        &HerdrClient::new(fake_hc_liveness.to_string_lossy().to_string()),
        &FakeOrchSleeper::new(10_000),
        OrchLaunchArgs {
            agent: "agy".into(),
            prompt: "Run task".into(),
            title: None,
            run: None,
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
        Some(bin_dir.as_os_str()),
    )
    .await
    .unwrap_err();

    let l_err = err_launch.downcast_ref::<OrchLaunchError>().unwrap();
    assert_eq!(l_err.failed_stage, "launch");
    assert_eq!(l_err.exit_code, Some(2));
    assert!(l_err
        .output_tail
        .as_deref()
        .unwrap_or("")
        .contains("Fatal: bad flag --xyz"));

    // Case B: hc run fails immediately -> failed_stage = "spawn"
    let fake_hc_spawn = tmp.path().join("fake-hc-spawn");
    fs::write(
        &fake_hc_spawn,
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"get\" ]; then\n\
           echo '{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"/tmp\"}'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"run\" ]; then\n\
           echo 'core socket refused' >&2\n\
           exit 7\n\
         fi\n\
         exit 1\n",
    )
    .unwrap();
    fs::set_permissions(&fake_hc_spawn, fs::Permissions::from_mode(0o755)).unwrap();

    let store_b = OrchStore::open(&tmp.path().join("orch_spawn.db")).unwrap();
    let err_spawn = execute_launch_with_path(
        &store_b,
        &HerdrClient::new(fake_hc_spawn.to_string_lossy().to_string()),
        &FakeOrchSleeper::new(10_000),
        OrchLaunchArgs {
            agent: "agy".into(),
            prompt: "Run task".into(),
            title: None,
            run: None,
            worktree: "current".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
        Some(bin_dir.as_os_str()),
    )
    .await
    .unwrap_err();

    let s_err = err_spawn.downcast_ref::<OrchLaunchError>().unwrap();
    assert_eq!(s_err.failed_stage, "spawn");
    assert!(s_err.detail.contains("core socket refused"));
    assert_eq!(s_err.exit_code, Some(7));
}

#[tokio::test]
async fn lot_f_send_to_worker_fencing_live_busy_and_finished_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let worker_state = tmp.path().join("worker_state");
    let worker_status = tmp.path().join("worker_status");
    let send_log = tmp.path().join("hc_send.log");
    fs::write(&worker_state, "live").unwrap();
    fs::write(&worker_status, "idle").unwrap();

    let fake_hc = tmp.path().join("fake-hc");
    fs::write(
        &fake_hc,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"get\" ]; then\n\
               if [ \"$2\" = \"t1\" ]; then\n\
                 echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0}}'\n\
                 exit 0\n\
               fi\n\
               if [ \"$2\" = \"t9\" ]; then\n\
                 echo '{{\"address\":\"t9\",\"created_ms\":9000,\"state\":\"live\",\"turn\":0}}'\n\
                 exit 0\n\
               fi\n\
               if [ \"$2\" = \"t2\" ]; then\n\
                 st=$(cat \"{}\")\n\
                 sub=$(cat \"{}\")\n\
                 echo \"{{\\\"address\\\":\\\"t2\\\",\\\"created_ms\\\":2000,\\\"state\\\":\\\"$st\\\",\\\"turn\\\":1,\\\"status\\\":{{\\\"state\\\":\\\"$sub\\\"}}}}\"\n\
                 exit 0\n\
               fi\n\
               exit 1\n\
             fi\n\
             if [ \"$1\" = \"send\" ]; then\n\
               echo \"$*\" >> \"{}\"\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            worker_state.display(),
            worker_status.display(),
            send_log.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();
    store
        .create_dispatch(&sample_dispatch(
            "disp_1", "run_1", "task_a", "t1", "t2", 2_000, 0,
        ))
        .unwrap();

    // 1. Non-parent caller (t9 or worker t2 itself) is rejected by fencing
    let non_parent_err = execute_send_to(
        &store,
        &herdr,
        &FakeOrchSleeper::new(5_000),
        OrchSendToArgs {
            to: "dispatch:disp_1".into(),
            body: "spoofed".into(),
            request_id: None,
            caller_terminal: Some("t9".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        non_parent_err.to_string().contains("fencing")
            || non_parent_err.to_string().contains("parent"),
        "expected parent fencing error, got: {non_parent_err}"
    );

    let self_err = execute_send_to(
        &store,
        &herdr,
        &FakeOrchSleeper::new(5_050),
        OrchSendToArgs {
            to: "dispatch:disp_1".into(),
            body: "self".into(),
            request_id: None,
            caller_terminal: Some("t2".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        self_err.to_string().contains("fencing") || self_err.to_string().contains("parent"),
        "expected parent fencing error for worker self-send, got: {self_err}"
    );

    // 2. Parent t1 sends while worker t2 is idle -> delivered immediately via guarded hc send
    let out_idle = execute_send_to(
        &store,
        &herdr,
        &FakeOrchSleeper::new(5_100),
        OrchSendToArgs {
            to: "dispatch:disp_1".into(),
            body: "Check edge case X".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();
    assert!(out_idle.delivered_immediately);
    assert_eq!(out_idle.message.target_terminal, "t2");
    assert_eq!(out_idle.message.body, "Check edge case X");
    let sent_log_txt = fs::read_to_string(&send_log).unwrap();
    assert!(sent_log_txt.contains("send --if idle t2"));
    assert!(sent_log_txt.contains("Check edge case X"));

    // 3. Parent t1 sends while worker t2 is working (busy) -> queued in mailbox for pointer delivery
    fs::write(&worker_status, "working").unwrap();
    let out_busy = execute_send_to(
        &store,
        &herdr,
        &FakeOrchSleeper::new(5_200),
        OrchSendToArgs {
            to: "dispatch:disp_1".into(),
            body: "Also check Y".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();
    assert!(!out_busy.delivered_immediately);
    assert_eq!(out_busy.message.pointer_state, PointerState::Pending);

    // Verify store records both messages so `status` / list_messages_for_run shows them
    let msgs = store.list_messages_for_run("run_1").unwrap();
    assert_eq!(msgs.len(), 2);

    // When worker t2 becomes idle, OrchDeliveryCoordinator delivers the queued pointer
    let mut tracker = FleetTracker::new();
    tracker.apply_snapshot(
        vec![HcTerminal {
            address: "t2".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "idle".into(),
                ..Default::default()
            }),
            ..Default::default()
        }],
        5_300,
    );
    let deliv = OrchDeliveryCoordinator::deliver_pending_pointers(&store, &herdr, &tracker, 5_300)
        .await
        .unwrap();
    assert_eq!(deliv.delivered_count, 1);

    // 4. Finished worker terminal -> clear error suggesting `orch retry`
    fs::write(&worker_state, "finished").unwrap();
    let finished_err = execute_send_to(
        &store,
        &herdr,
        &FakeOrchSleeper::new(5_400),
        OrchSendToArgs {
            to: "dispatch:disp_1".into(),
            body: "Are you still there?".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        finished_err.to_string().contains("orch retry"),
        "expected error suggesting `orch retry`, got: {finished_err}"
    );
}

#[tokio::test]
async fn lot_f_retry_creates_new_dispatch_supersedes_old_and_respects_circuit_breaker() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    init_temp_git_repo(&repo_dir);
    let spawn_count = tmp.path().join("spawn_count");
    let term_ops = tmp.path().join("term_ops.log");
    fs::write(&spawn_count, "0").unwrap();

    let fake_hc = tmp.path().join("fake-hc");
    fs::write(
        &fake_hc,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"get\" ]; then\n\
               if [ \"$2\" = \"t1\" ]; then\n\
                 echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"{}\"}}'\n\
                 exit 0\n\
               fi\n\
               echo '{{\"address\":\"'\"$2\"'\",\"created_ms\":6000,\"state\":\"live\",\"turn\":0,\"labels\":{{}}}}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"run\" ]; then\n\
               c=$(cat \"{}\")\n\
               c=$((c + 1))\n\
               echo \"$c\" > \"{}\"\n\
               addr=\"t$((10 + c))\"\n\
               echo \"{{\\\"address\\\":\\\"$addr\\\",\\\"created_ms\\\":$((6000 + c)),\\\"state\\\":\\\"live\\\",\\\"turn\\\":0,\\\"first_message\\\":{{\\\"sent\\\":true}}}}\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"kill\" ] || [ \"$1\" = \"rm\" ]; then\n\
               echo \"$1 $2\" >> \"{}\"\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            repo_dir.display(),
            spawn_count.display(),
            spawn_count.display(),
            term_ops.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
        .unwrap();

    // Initial dispatch with new-child worktree
    let disp1 = execute_dispatch(
        &store,
        &herdr,
        &FakeOrchSleeper::new(10_000),
        OrchDispatchArgs {
            run: "run_1".into(),
            task: "task_a".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("claude".into()),
            worktree: "new-child".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(disp1.dispatch.assignee_terminal, "t11");
    assert_eq!(disp1.dispatch.worktree_mode, "new-child");

    // Simulate worker failure once (failure_count = 1 < 3)
    store
        .record_worker_done("disp_1_missing", "t11", 6001, "failed", "nope", 12_000)
        .unwrap_err();
    store
        .record_worker_done(
            &disp1.dispatch.id,
            "t11",
            6001,
            "failed",
            "attempt 1 failed",
            12_000,
        )
        .unwrap();
    assert_eq!(
        store
            .get_task("run_1", "task_a")
            .unwrap()
            .unwrap()
            .failure_count,
        1
    );

    // Retry creates a new dispatch for the same task, same agent kind & worktree mode, marks old Superseded
    let retried = execute_retry(
        &store,
        &herdr,
        &FakeOrchSleeper::new(13_000),
        OrchRetryArgs {
            dispatch_id: disp1.dispatch.id.clone(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(retried.previous_dispatch_id, disp1.dispatch.id);
    assert_eq!(retried.previous_status, DispatchStatus::Superseded);
    assert_eq!(
        store
            .get_dispatch(&disp1.dispatch.id)
            .unwrap()
            .unwrap()
            .status,
        DispatchStatus::Superseded
    );
    assert_ne!(retried.dispatch_output.dispatch.id, disp1.dispatch.id);
    assert_eq!(retried.dispatch_output.dispatch.task_id, "task_a");
    assert_eq!(retried.dispatch_output.dispatch.agent_kind, "claude");
    assert_eq!(retried.dispatch_output.dispatch.worktree_mode, "new-child");
    assert_eq!(retried.dispatch_output.dispatch.assignee_terminal, "t12");
    assert_eq!(
        retried.dispatch_output.dispatch.status,
        DispatchStatus::Dispatched
    );

    // Trip the circuit breaker (2 more failures -> failure_count = 3)
    store
        .record_worker_done(
            &retried.dispatch_output.dispatch.id,
            "t12",
            6002,
            "failed",
            "attempt 2 failed",
            14_000,
        )
        .unwrap();
    store
        .record_task_failure(
            "run_1",
            "task_a",
            Some(&retried.dispatch_output.dispatch.id),
            "attempt 3 failed",
            15_000,
        )
        .unwrap();
    assert_eq!(
        store.get_task("run_1", "task_a").unwrap().unwrap().status,
        TaskStatus::CircuitBroken
    );

    // Retry must respect the circuit breaker and refuse
    let breaker_err = execute_retry(
        &store,
        &herdr,
        &FakeOrchSleeper::new(16_000),
        OrchRetryArgs {
            dispatch_id: retried.dispatch_output.dispatch.id.clone(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(breaker_err.to_string().contains("circuit breaker tripped"));
}

#[tokio::test]
async fn lot_f_stop_closes_terminal_marks_stopped_restores_task_and_preserves_dirty_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    init_temp_git_repo(&repo_dir);
    let term_ops = tmp.path().join("term_ops.log");

    let fake_hc = tmp.path().join("fake-hc");
    fs::write(
        &fake_hc,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"get\" ]; then\n\
               if [ \"$2\" = \"t1\" ]; then\n\
                 echo '{{\"address\":\"t1\",\"created_ms\":1000,\"state\":\"live\",\"turn\":0,\"cwd\":\"{}\"}}'\n\
                 exit 0\n\
               fi\n\
               echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"labels\":{{}}}}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"run\" ]; then\n\
               echo '{{\"address\":\"t5\",\"created_ms\":5000,\"state\":\"live\",\"turn\":0,\"first_message\":{{\"sent\":true}}}}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"kill\" ] || [ \"$1\" = \"rm\" ]; then\n\
               echo \"$1 $2\" >> \"{}\"\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            repo_dir.display(),
            term_ops.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_hc, fs::Permissions::from_mode(0o755)).unwrap();
    let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());

    let store = OrchStore::open(&tmp.path().join("orch.db")).unwrap();
    store.create_run(&sample_run("run_1", "t1")).unwrap();
    store
        .create_or_update_task(&sample_task("run_1", "task_dirty", vec![]))
        .unwrap();

    let disp = execute_dispatch(
        &store,
        &herdr,
        &FakeOrchSleeper::new(10_000),
        OrchDispatchArgs {
            run: "run_1".into(),
            task: "task_dirty".into(),
            title: None,
            prompt: None,
            terminal: None,
            agent: Some("claude".into()),
            worktree: "new-child".into(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    // Dirty the child worktree
    let wt_path = std::path::PathBuf::from(disp.dispatch.worktree_path.as_ref().unwrap());
    fs::write(wt_path.join("uncommitted.rs"), "// work in progress\n").unwrap();

    let stop_out = execute_stop(
        &store,
        &herdr,
        &FakeOrchSleeper::new(11_000),
        OrchStopArgs {
            dispatch_id: disp.dispatch.id.clone(),
            request_id: None,
            caller_terminal: Some("t1".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(stop_out.dispatch_status, DispatchStatus::Stopped);
    assert_eq!(stop_out.task_status, TaskStatus::Ready);
    assert!(stop_out.terminal_closed);
    assert!(stop_out.worktree_kept);
    assert!(
        wt_path.exists(),
        "dirty worktree must never be deleted by stop"
    );

    let ops = fs::read_to_string(&term_ops).unwrap();
    assert!(ops.contains("kill t5"), "expected hc kill t5, got: {ops}");
    assert!(ops.contains("rm t5"), "expected hc rm t5, got: {ops}");

    let db_disp = store.get_dispatch(&disp.dispatch.id).unwrap().unwrap();
    assert_eq!(db_disp.status, DispatchStatus::Stopped);
    let db_task = store.get_task("run_1", "task_dirty").unwrap().unwrap();
    assert_eq!(db_task.status, TaskStatus::Ready);
    assert_eq!(db_task.failure_count, 0);
}
