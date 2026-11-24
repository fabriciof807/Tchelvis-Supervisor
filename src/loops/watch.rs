use crate::herdr::FleetTracker;
use crate::loops::state::{load_state, save_state, RunStatus};
use crate::orch::store::{LoopRunRecord, OrchStore};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

pub trait ProcTable: Send + Sync {
    fn is_alive(&self, pid: u32, expected_start_ticks: Option<u64>) -> bool;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxProcTable;

fn read_proc_state_and_start_ticks(pid: u32) -> Option<(char, u64)> {
    if pid == 0 {
        return None;
    }
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = raw.rfind(')')?;
    let fields: Vec<&str> = raw.get(rparen + 1..)?.split_whitespace().collect();
    if fields.len() < 20 {
        return None;
    }
    let state = fields[0].chars().next()?;
    let start_ticks = fields[19].parse::<u64>().ok()?;
    Some((state, start_ticks))
}

impl ProcTable for LinuxProcTable {
    fn is_alive(&self, pid: u32, expected_start_ticks: Option<u64>) -> bool {
        let Some((state, start_ticks)) = read_proc_state_and_start_ticks(pid) else {
            return false;
        };
        if state == 'Z' || state == 'X' {
            return false;
        }
        expected_start_ticks.is_none_or(|exp| exp == start_ticks)
    }
}

pub fn current_process_identity() -> (u32, Option<u64>) {
    let pid = std::process::id();
    let ticks = read_proc_state_and_start_ticks(pid).map(|(_, t)| t);
    (pid, ticks)
}

fn caller_is_alive_for_delivery(tracker: &FleetTracker, rec: &LoopRunRecord) -> bool {
    let (Some(terminal), Some(expected_ms)) =
        (rec.caller_terminal.as_deref(), rec.caller_created_ms)
    else {
        return false;
    };
    let Some(term) = tracker.terminals.get(terminal) else {
        return false;
    };
    if term.state == "finished" {
        return false;
    }
    term.created_ms == Some(expected_ms)
}

/// How long a registered loop may take to report its engine process.
const ENGINE_START_GRACE_MS: i64 = 60_000;

pub fn watch_loops(
    store: &OrchStore,
    tracker: &FleetTracker,
    procs: &dyn ProcTable,
    now_ms: i64,
) -> Result<usize> {
    let mut updates = 0usize;
    for rec in store.list_loops()? {
        let run_id = rec.run_id.clone();
        // One loop that cannot be updated must not keep the others from being noticed.
        match watch_one(store, tracker, procs, rec, now_ms) {
            Ok(n) => updates += n,
            Err(err) => tracing::warn!(run = %run_id, "loop watch failed: {err:#}"),
        }
    }
    Ok(updates)
}

fn final_status(dir: &Path) -> Option<RunStatus> {
    load_state(dir)
        .ok()
        .map(|st| st.status)
        .filter(RunStatus::is_final)
}

/// The final status to record for a loop still marked as running, if it ended.
fn ended_status(rec: &LoopRunRecord, procs: &dyn ProcTable, now_ms: i64) -> Option<RunStatus> {
    let dir = PathBuf::from(&rec.dir);
    if let Some(status) = final_status(&dir) {
        return Some(status);
    }
    let engine = rec
        .engine_pid
        .map(|pid| (pid, rec.engine_start_ticks))
        .or_else(|| {
            let st = load_state(&dir).ok()?;
            Some((st.engine_pid?, st.engine_start_ticks))
        });
    let dead = match engine {
        Some((pid, ticks)) => !procs.is_alive(pid, ticks),
        None => now_ms.saturating_sub(rec.created_at_ms) > ENGINE_START_GRACE_MS,
    };
    if !dead {
        return None;
    }
    // The engine may have saved its result and exited between the two checks. It is gone now, so
    // nothing else writes the state from here on.
    if let Some(status) = final_status(&dir) {
        return Some(status);
    }
    if let Ok(mut st) = load_state(&dir) {
        st.status = RunStatus::Error;
        st.reason = Some("ENGINE_DIED".to_string());
        if st.summary.trim().is_empty() {
            st.summary = "ENGINE_DIED".to_string();
        }
        st.finished_at_ms = Some(now_ms);
        if let Err(err) = save_state(&dir, &st) {
            tracing::warn!(run = %rec.run_id, "could not mark the loop state as dead: {err:#}");
        }
    }
    Some(RunStatus::Error)
}

fn watch_one(
    store: &OrchStore,
    tracker: &FleetTracker,
    procs: &dyn ProcTable,
    mut rec: LoopRunRecord,
    now_ms: i64,
) -> Result<usize> {
    let mut updates = 0usize;
    if rec.status == "RUNNING" {
        let Some(status) = ended_status(&rec, procs, now_ms) else {
            return Ok(0);
        };
        store.record_loop_final(&rec.run_id, status.as_str(), now_ms)?;
        updates += 1;
        if let Some(refreshed) = store.get_loop(&rec.run_id)? {
            rec = refreshed;
        }
    }

    if matches!(rec.notify_state.as_str(), "pending" | "delivered") {
        if store.sync_loop_notify_state(&rec.run_id, now_ms)? {
            updates += 1;
            if let Some(refreshed) = store.get_loop(&rec.run_id)? {
                rec = refreshed;
            }
        }
        if rec.notify_state == "pending"
            && !caller_is_alive_for_delivery(tracker, &rec)
            && store.mark_loop_undelivered(&rec.run_id, now_ms)?
        {
            updates += 1;
        }
    }
    Ok(updates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::HcTerminal;
    use crate::loops::state::{
        load_state, save_state, CallerInfo, RunState, RunStatus, LOOP_STATE_SCHEMA_VERSION,
    };
    use crate::orch::store::LoopRunRecord;
    use crate::orch::PointerState;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    struct FakeProcTable {
        alive: HashMap<u32, u64>,
    }

    impl ProcTable for FakeProcTable {
        fn is_alive(&self, pid: u32, expected_start_ticks: Option<u64>) -> bool {
            match self.alive.get(&pid) {
                None => false,
                Some(&ticks) => expected_start_ticks.is_none_or(|exp| exp == ticks),
            }
        }
    }

    fn make_terminal(addr: &str, created_ms: i64, state: &str) -> HcTerminal {
        HcTerminal {
            address: addr.to_string(),
            state: state.to_string(),
            created_ms: Some(created_ms),
            ..Default::default()
        }
    }

    fn sample_run_state(
        dir: &Path,
        run_id: &str,
        pid: u32,
        ticks: u64,
        heartbeat: i64,
    ) -> RunState {
        let state = RunState {
            schema_version: LOOP_STATE_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            workflow: "worker-reviewer-critic".to_string(),
            status: RunStatus::Running,
            reason: None,
            sandbox: "bwrap".to_string(),
            sandbox_exposures: vec![],
            preflight: vec![],
            current_cycle: 1,
            max_cycles: 3,
            branch: None,
            base_commit: Some("deadbeef".to_string()),
            engine_pid: Some(pid),
            engine_start_ticks: Some(ticks),
            caller: Some(CallerInfo {
                terminal: "t1".to_string(),
                created_ms: 1_000,
            }),
            heartbeat_at_ms: heartbeat,
            last_activity_at_ms: heartbeat,
            started_at_ms: 1_000,
            finished_at_ms: None,
            executions: vec![],
            files: vec![],
            summary: String::new(),
        };
        save_state(dir, &state).unwrap();
        state
    }

    fn sample_loop_rec(
        run_id: &str,
        dir: &Path,
        pid: u32,
        ticks: u64,
        caller: Option<(&str, i64)>,
    ) -> LoopRunRecord {
        LoopRunRecord {
            run_id: run_id.to_string(),
            dir: dir.display().to_string(),
            workflow: "worker-reviewer-critic".to_string(),
            status: "RUNNING".to_string(),
            caller_terminal: caller.map(|(t, _)| t.to_string()),
            caller_created_ms: caller.map(|(_, ms)| ms),
            engine_pid: Some(pid),
            engine_start_ticks: Some(ticks),
            orch_run_id: None,
            notify_state: if caller.is_some() {
                "pending".to_string()
            } else {
                "none".to_string()
            },
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        }
    }

    #[test]
    fn final_state_creates_one_message_even_when_called_twice_or_from_two_connections() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("orch.db");
        let store1 = OrchStore::open(&db_path).unwrap();
        let store2 = OrchStore::open(&db_path).unwrap();

        let rec = sample_loop_rec("20261124-001", tmp.path(), 42, 999, Some(("t1", 1_000)));
        store1.register_loop(&rec).unwrap();

        let first = store1
            .record_loop_final("20261124-001", "PASS", 2_000)
            .unwrap();
        assert!(first.is_some());

        let second_same = store1
            .record_loop_final("20261124-001", "PASS", 2_100)
            .unwrap();
        assert!(second_same.is_none());

        let second_other_conn = store2
            .record_loop_final("20261124-001", "ERROR", 2_200)
            .unwrap();
        assert!(second_other_conn.is_none());

        // Concurrent race across two separate DB connections on another run
        let rec2 = sample_loop_rec("20261124-002", tmp.path(), 43, 1000, Some(("t1", 1_000)));
        store1.register_loop(&rec2).unwrap();
        let results: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let s1 = store1.clone();
        let s2 = store2.clone();
        let r1 = Arc::clone(&results);
        let r2 = Arc::clone(&results);
        std::thread::scope(|s| {
            s.spawn(move || {
                let m = s1.record_loop_final("20261124-002", "PASS", 3_000).unwrap();
                r1.lock().unwrap().push(m.map(|x| x.id));
            });
            s.spawn(move || {
                let m = s2
                    .record_loop_final("20261124-002", "ESCALATE", 3_001)
                    .unwrap();
                r2.lock().unwrap().push(m.map(|x| x.id));
            });
        });
        let res = results.lock().unwrap();
        assert_eq!(res.iter().filter(|x| x.is_some()).count(), 1);

        let msgs1 = store1.list_messages_for_run("loop_20261124-001").unwrap();
        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs1[0].id, "msg_loop_20261124-001");
        assert_eq!(
            msgs1[0].body,
            "Loop 20261124-001: PASS. Rode: tchelvis loop result 20261124-001"
        );
        let msgs2 = store2.list_messages_for_run("loop_20261124-002").unwrap();
        assert_eq!(msgs2.len(), 1);
    }

    #[test]
    fn dead_engine_becomes_error_engine_died() {
        let tmp = TempDir::new().unwrap();
        let run1_dir = tmp.path().join("r1");
        let run2_dir = tmp.path().join("r2");
        std::fs::create_dir_all(&run1_dir).unwrap();
        std::fs::create_dir_all(&run2_dir).unwrap();

        let store = OrchStore::open_in_memory().unwrap();
        // Run 1: pid is gone completely.
        sample_run_state(&run1_dir, "run-gone", 5001, 111, 1_000);
        store
            .register_loop(&sample_loop_rec(
                "run-gone",
                &run1_dir,
                5001,
                111,
                Some(("t1", 1_000)),
            ))
            .unwrap();

        // Run 2: pid exists in proc table, but with DIFFERENT start_ticks (PID reused!).
        sample_run_state(&run2_dir, "run-reused", 5002, 222, 1_000);
        store
            .register_loop(&sample_loop_rec(
                "run-reused",
                &run2_dir,
                5002,
                222,
                Some(("t1", 1_000)),
            ))
            .unwrap();

        let procs = FakeProcTable {
            alive: HashMap::from([(5002, 999)]),
        };
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1", 1_000, "running")], 5_000);

        let updates = watch_loops(&store, &tracker, &procs, 5_000).unwrap();
        assert_eq!(updates, 2);

        for (rid, rdir) in [("run-gone", &run1_dir), ("run-reused", &run2_dir)] {
            let st = load_state(rdir).unwrap();
            assert_eq!(st.status, RunStatus::Error);
            assert_eq!(st.reason.as_deref(), Some("ENGINE_DIED"));
            let rec = store.get_loop(rid).unwrap().unwrap();
            assert_eq!(rec.status, "ERROR");
            let msgs = store.list_messages_for_run(&format!("loop_{rid}")).unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(
                msgs[0].body,
                format!("Loop {rid}: ERROR. Rode: tchelvis loop result {rid}")
            );
            assert!(!msgs[0].body.contains("ENGINE_DIED"));
        }
    }

    #[test]
    fn live_engine_with_old_heartbeat_is_not_killed() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("r-live");
        std::fs::create_dir_all(&run_dir).unwrap();
        sample_run_state(&run_dir, "run-live", 6001, 333, 1_000);

        let store = OrchStore::open_in_memory().unwrap();
        store
            .register_loop(&sample_loop_rec(
                "run-live",
                &run_dir,
                6001,
                333,
                Some(("t1", 1_000)),
            ))
            .unwrap();

        let procs = FakeProcTable {
            alive: HashMap::from([(6001, 333)]),
        };
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1", 1_000, "running")], 99_999_999);

        let updates = watch_loops(&store, &tracker, &procs, 99_999_999).unwrap();
        assert_eq!(updates, 0);

        let st = load_state(&run_dir).unwrap();
        assert_eq!(st.status, RunStatus::Running);
        let rec = store.get_loop("run-live").unwrap().unwrap();
        assert_eq!(rec.status, "RUNNING");
        assert!(store
            .list_messages_for_run("loop_run-live")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn caller_with_other_created_ms_or_absent_is_marked_undelivered() {
        let tmp = TempDir::new().unwrap();
        let store = OrchStore::open_in_memory().unwrap();

        store
            .register_loop(&sample_loop_rec(
                "run-reused-term",
                tmp.path(),
                7001,
                10,
                Some(("t1", 1_000)),
            ))
            .unwrap();
        store
            .register_loop(&sample_loop_rec(
                "run-absent-term",
                tmp.path(),
                7002,
                11,
                Some(("t2", 1_000)),
            ))
            .unwrap();

        store
            .record_loop_final("run-reused-term", "PASS", 2_000)
            .unwrap();
        store
            .record_loop_final("run-absent-term", "PASS", 2_000)
            .unwrap();

        // t1 exists now with DIFFERENT created_ms (2_500 != 1_000); t2 is absent.
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1", 2_500, "running")], 3_000);

        let procs = FakeProcTable {
            alive: HashMap::new(),
        };
        watch_loops(&store, &tracker, &procs, 3_000).unwrap();

        let rec1 = store.get_loop("run-reused-term").unwrap().unwrap();
        let rec2 = store.get_loop("run-absent-term").unwrap().unwrap();
        assert_eq!(rec1.notify_state, "undelivered");
        assert_eq!(rec2.notify_state, "undelivered");

        // DeliveryScheduler must never pick up messages for reused or absent terminals.
        assert!(store.pending_pointer_targets(3_000).unwrap().is_empty());
        let msg1 = store.list_messages_for_run("loop_run-reused-term").unwrap();
        assert_eq!(msg1[0].pointer_state, PointerState::DeliveryStalled);
    }

    #[test]
    fn orch_check_shows_the_loop_message() {
        let tmp = TempDir::new().unwrap();
        let store = OrchStore::open_in_memory().unwrap();
        store
            .register_loop(&sample_loop_rec(
                "run-check",
                tmp.path(),
                8001,
                44,
                Some(("t1", 1_000)),
            ))
            .unwrap();
        store.record_loop_final("run-check", "PASS", 2_000).unwrap();

        let inbox = store
            .claim_inbox_messages("t1", None, false, 2_500)
            .unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].id, "msg_loop_run-check");
        assert_eq!(
            inbox.messages[0].body,
            "Loop run-check: PASS. Rode: tchelvis loop result run-check"
        );
    }

    #[test]
    fn existing_database_is_migrated() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("legacy_orch.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runs (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    coordinator_terminal TEXT NOT NULL,
                    coordinator_created_ms INTEGER NOT NULL,
                    status TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );",
            )
            .unwrap();
        }
        let store = OrchStore::open(&db_path).unwrap();
        assert!(store.list_loops().unwrap().is_empty());
        let rec = sample_loop_rec("migrated-run", tmp.path(), 9001, 1, None);
        store.register_loop(&rec).unwrap();
        assert_eq!(store.list_loops().unwrap().len(), 1);
    }

    #[test]
    fn unreadable_state_json_does_not_break_the_daemon_cycle() {
        let tmp = TempDir::new().unwrap();
        let bad_dir = tmp.path().join("corrupt");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("state.json"), "{ not valid json").unwrap();

        let missing_dir = tmp.path().join("missing-dir");

        let store = OrchStore::open_in_memory().unwrap();
        store
            .register_loop(&sample_loop_rec(
                "bad-json-live",
                &bad_dir,
                9101,
                77,
                Some(("t1", 1_000)),
            ))
            .unwrap();
        store
            .register_loop(&sample_loop_rec(
                "missing-dir-dead",
                &missing_dir,
                9102,
                88,
                Some(("t1", 1_000)),
            ))
            .unwrap();

        let procs = FakeProcTable {
            alive: HashMap::from([(9101, 77)]),
        };
        let mut tracker = FleetTracker::new();
        tracker.apply_snapshot(vec![make_terminal("t1", 1_000, "running")], 5_000);

        let res = watch_loops(&store, &tracker, &procs, 5_000);
        assert!(res.is_ok(), "watch_loops should not fail: {res:?}");
        assert_eq!(
            store.get_loop("bad-json-live").unwrap().unwrap().status,
            "RUNNING"
        );
        assert_eq!(
            store.get_loop("missing-dir-dead").unwrap().unwrap().status,
            "ERROR"
        );
    }

    #[test]
    fn linux_proc_table_checks_live_and_reused_ticks() {
        let (pid, ticks) = current_process_identity();
        assert!(ticks.is_some());
        let procs = LinuxProcTable;
        assert!(procs.is_alive(pid, ticks));
        assert!(!procs.is_alive(pid, ticks.map(|t| t.wrapping_add(1))));
        assert!(!procs.is_alive(0, None));
    }

    struct FinishesWhileChecked {
        dir: std::path::PathBuf,
    }

    impl ProcTable for FinishesWhileChecked {
        fn is_alive(&self, _pid: u32, _ticks: Option<u64>) -> bool {
            // The engine saves its final state and exits between the two checks.
            let mut st = load_state(&self.dir).unwrap();
            st.status = RunStatus::Pass;
            st.summary = "tudo certo".to_string();
            save_state(&self.dir, &st).unwrap();
            false
        }
    }

    #[test]
    fn engine_that_finishes_during_the_check_keeps_its_result() {
        let tmp = TempDir::new().unwrap();
        let store = OrchStore::open_in_memory().unwrap();
        sample_run_state(tmp.path(), "r1", 10, 20, 1_000);
        store
            .register_loop(&sample_loop_rec("r1", tmp.path(), 10, 20, None))
            .unwrap();
        let procs = FinishesWhileChecked {
            dir: tmp.path().to_path_buf(),
        };
        watch_loops(&store, &FleetTracker::new(), &procs, 5_000).unwrap();
        assert_eq!(store.get_loop("r1").unwrap().unwrap().status, "PASS");
        let st = load_state(tmp.path()).unwrap();
        assert_eq!(st.status, RunStatus::Pass);
        assert_eq!(st.summary, "tudo certo");
    }

    #[test]
    fn engine_that_never_started_is_an_error_after_a_minute() {
        let tmp = TempDir::new().unwrap();
        let store = OrchStore::open_in_memory().unwrap();
        let mut rec = sample_loop_rec("r1", tmp.path(), 1, 1, None);
        rec.engine_pid = None;
        rec.engine_start_ticks = None;
        store.register_loop(&rec).unwrap();
        let none = FakeProcTable {
            alive: HashMap::new(),
        };
        let status = |now: i64| {
            watch_loops(&store, &FleetTracker::new(), &none, now).unwrap();
            store.get_loop("r1").unwrap().unwrap().status
        };
        assert_eq!(status(1_000 + 59_000), "RUNNING");
        assert_eq!(status(1_000 + 61_000), "ERROR");
    }

    #[test]
    fn one_bad_row_does_not_stop_the_others() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("orch.db");
        let store = OrchStore::open(&db).unwrap();
        for id in ["a-bad", "b-good"] {
            let dir = tmp.path().join(id);
            std::fs::create_dir_all(&dir).unwrap();
            sample_run_state(&dir, id, 10, 20, 1_000);
            store
                .register_loop(&sample_loop_rec(id, &dir, 10, 20, None))
                .unwrap();
        }
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER bad BEFORE UPDATE ON loop_runs WHEN OLD.run_id = 'a-bad'
                 BEGIN SELECT RAISE(ABORT, 'disco'); END;",
            )
            .unwrap();
        let none = FakeProcTable {
            alive: HashMap::new(),
        };
        let _ = watch_loops(&store, &FleetTracker::new(), &none, 5_000);
        assert_eq!(store.get_loop("b-good").unwrap().unwrap().status, "ERROR");
    }
}
