use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tchelvis::config::{CockpitConfig, CockpitPaths};
use tchelvis::daemon::materialize_snapshot;
use tchelvis::doctor::run_doctor;
use tchelvis::herdr::{FleetTracker, HcStatus, HcTerminal, HerdrClient};
use tchelvis::model::{AgentSummaryRow, AttentionState};
use tchelvis::orch::cli::{
    create_child_worktree, execute_orch_command, map_agent_preset, parse_deps_arg,
    verify_and_remove_child_worktree, OrchCommands,
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
