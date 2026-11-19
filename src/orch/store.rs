use crate::orch::{
    generate_orch_id, DispatchContext, DispatchStatus, InboxCheckResult, MessageType, OrchMessage,
    OrchRun, OrchTask, PendingPointerTarget, PointerState, QuestionThread, RunStatus, TaskStatus,
    MAX_DELIVERY_ATTEMPTS, MAX_DISPATCH_FAILURES, MAX_ORCH_DEPTH, POINTER_RETRY_BACKOFF_MS,
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

const DISPATCH_COLUMNS: &str =
    "id, run_id, task_id, parent_terminal, assignee_terminal, terminal_created_ms, \
    turn_at_dispatch, agent_kind, worktree_mode, worktree_path, depth, status, \
    idle_since_ms, blocked_since_ms, created_at_ms, updated_at_ms";

#[derive(Clone)]
pub struct OrchStore {
    conn: Arc<Mutex<Connection>>,
}

impl OrchStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed opening orchestration DB at {}", path.display()))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                coordinator_terminal TEXT NOT NULL,
                coordinator_created_ms INTEGER NOT NULL,
                status TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                id TEXT NOT NULL,
                title TEXT NOT NULL,
                prompt TEXT NOT NULL,
                deps_json TEXT NOT NULL DEFAULT '[]',
                failure_count INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                result_summary TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (run_id, id)
            );
            CREATE TABLE IF NOT EXISTS dispatch_contexts (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                task_id TEXT NOT NULL,
                parent_terminal TEXT NOT NULL,
                assignee_terminal TEXT NOT NULL,
                terminal_created_ms INTEGER NOT NULL,
                turn_at_dispatch INTEGER NOT NULL DEFAULT 0,
                agent_kind TEXT NOT NULL,
                worktree_mode TEXT NOT NULL,
                worktree_path TEXT,
                depth INTEGER NOT NULL DEFAULT 1,
                status TEXT NOT NULL,
                idle_since_ms INTEGER,
                blocked_since_ms INTEGER,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                FOREIGN KEY (run_id, task_id) REFERENCES tasks(run_id, id) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                task_id TEXT,
                dispatch_id TEXT,
                from_handle TEXT NOT NULL,
                to_handle TEXT NOT NULL,
                target_terminal TEXT NOT NULL,
                msg_type TEXT NOT NULL,
                outcome TEXT,
                body TEXT NOT NULL,
                pointer_state TEXT NOT NULL,
                pointer_terminal TEXT,
                delivery_attempts INTEGER NOT NULL DEFAULT 0,
                next_retry_at_ms INTEGER,
                delivered_at_ms INTEGER,
                acknowledged_at_ms INTEGER,
                created_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS question_threads (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                dispatch_id TEXT NOT NULL REFERENCES dispatch_contexts(id) ON DELETE CASCADE,
                question_msg_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                reply_msg_id TEXT REFERENCES messages(id) ON DELETE SET NULL,
                status TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                answered_at_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS mutation_receipts (
                request_id TEXT PRIMARY KEY,
                command TEXT NOT NULL,
                response_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_run_status ON tasks(run_id, status);
            CREATE INDEX IF NOT EXISTS idx_dispatches_status ON dispatch_contexts(status);
            CREATE INDEX IF NOT EXISTS idx_dispatches_assignee ON dispatch_contexts(assignee_terminal, status);
            CREATE INDEX IF NOT EXISTS idx_messages_target_pointer ON messages(target_terminal, pointer_state, run_id);
            ",
        )?;
        Ok(())
    }

    fn map_run_row(r: &Row<'_>) -> rusqlite::Result<OrchRun> {
        let status_str: String = r.get(4)?;
        let status = RunStatus::from_str(&status_str).unwrap_or(RunStatus::Active);
        Ok(OrchRun {
            id: r.get(0)?,
            title: r.get(1)?,
            coordinator_terminal: r.get(2)?,
            coordinator_created_ms: r.get(3)?,
            status,
            created_at_ms: r.get(5)?,
            updated_at_ms: r.get(6)?,
        })
    }

    fn map_task_row(r: &Row<'_>) -> rusqlite::Result<OrchTask> {
        let deps_json: String = r.get(4)?;
        let deps: Vec<String> = serde_json::from_str(&deps_json).unwrap_or_default();
        let status_str: String = r.get(6)?;
        let status = TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Pending);
        Ok(OrchTask {
            run_id: r.get(0)?,
            id: r.get(1)?,
            title: r.get(2)?,
            prompt: r.get(3)?,
            deps,
            failure_count: r.get(5)?,
            status,
            result_summary: r.get(7)?,
            created_at_ms: r.get(8)?,
            updated_at_ms: r.get(9)?,
        })
    }

    fn map_dispatch_row(r: &Row<'_>) -> rusqlite::Result<DispatchContext> {
        let turn_i64: i64 = r.get(6)?;
        let status_str: String = r.get(11)?;
        let status = DispatchStatus::from_str(&status_str).unwrap_or(DispatchStatus::Dispatched);
        Ok(DispatchContext {
            id: r.get(0)?,
            run_id: r.get(1)?,
            task_id: r.get(2)?,
            parent_terminal: r.get(3)?,
            assignee_terminal: r.get(4)?,
            terminal_created_ms: r.get(5)?,
            turn_at_dispatch: turn_i64.max(0) as u64,
            agent_kind: r.get(7)?,
            worktree_mode: r.get(8)?,
            worktree_path: r.get(9)?,
            depth: r.get(10)?,
            status,
            idle_since_ms: r.get(12)?,
            blocked_since_ms: r.get(13)?,
            created_at_ms: r.get(14)?,
            updated_at_ms: r.get(15)?,
        })
    }

    fn map_message_row(r: &Row<'_>) -> rusqlite::Result<OrchMessage> {
        let msg_type_str: String = r.get(7)?;
        let msg_type = MessageType::from_str(&msg_type_str).unwrap_or(MessageType::Status);
        let pointer_state_str: String = r.get(10)?;
        let pointer_state =
            PointerState::from_str(&pointer_state_str).unwrap_or(PointerState::Pending);
        Ok(OrchMessage {
            id: r.get(0)?,
            run_id: r.get(1)?,
            task_id: r.get(2)?,
            dispatch_id: r.get(3)?,
            from_handle: r.get(4)?,
            to_handle: r.get(5)?,
            target_terminal: r.get(6)?,
            msg_type,
            outcome: r.get(8)?,
            body: r.get(9)?,
            pointer_state,
            pointer_terminal: r.get(11)?,
            delivery_attempts: r.get(12)?,
            next_retry_at_ms: r.get(13)?,
            delivered_at_ms: r.get(14)?,
            acknowledged_at_ms: r.get(15)?,
            created_at_ms: r.get(16)?,
        })
    }

    fn map_question_row(r: &Row<'_>) -> rusqlite::Result<QuestionThread> {
        Ok(QuestionThread {
            id: r.get(0)?,
            run_id: r.get(1)?,
            dispatch_id: r.get(2)?,
            question_msg_id: r.get(3)?,
            reply_msg_id: r.get(4)?,
            status: r.get(5)?,
            created_at_ms: r.get(6)?,
            answered_at_ms: r.get(7)?,
        })
    }

    pub fn get_mutation_receipt(&self, request_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let row = conn
            .query_row(
                "SELECT response_json FROM mutation_receipts WHERE request_id = ?1",
                params![request_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    pub fn save_mutation_receipt(
        &self,
        request_id: &str,
        command: &str,
        response_json: &str,
        now_ms: i64,
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO mutation_receipts (request_id, command, response_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(request_id) DO UPDATE SET
                 command = excluded.command,
                 response_json = excluded.response_json",
            params![request_id, command, response_json, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn create_run(&self, run: &OrchRun) -> Result<()> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO runs (id, title, coordinator_terminal, coordinator_created_ms, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id,
                run.title,
                run.coordinator_terminal,
                run.coordinator_created_ms,
                run.status.as_str(),
                run.created_at_ms,
                run.updated_at_ms,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<OrchRun>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let row = conn
            .query_row(
                "SELECT id, title, coordinator_terminal, coordinator_created_ms, status, created_at_ms, updated_at_ms
                 FROM runs WHERE id = ?1",
                params![run_id],
                Self::map_run_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_runs(&self) -> Result<Vec<OrchRun>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, title, coordinator_terminal, coordinator_created_ms, status, created_at_ms, updated_at_ms
             FROM runs ORDER BY created_at_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map([], Self::map_run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn create_or_update_task(&self, task: &OrchTask) -> Result<OrchTask> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let saved = Self::create_or_update_task_tx(&tx, task)?;
        tx.commit()?;
        Ok(saved)
    }

    fn create_or_update_task_tx(tx: &Transaction<'_>, task: &OrchTask) -> Result<OrchTask> {
        let run_exists: Option<String> = tx
            .query_row(
                "SELECT id FROM runs WHERE id = ?1",
                params![task.run_id],
                |r| r.get(0),
            )
            .optional()?;
        if run_exists.is_none() {
            bail!("run '{}' does not exist", task.run_id);
        }

        for dep_id in &task.deps {
            if dep_id == &task.id {
                bail!("task '{}' cannot depend on itself", task.id);
            }
        }

        let mut all_completed = true;
        let mut any_failed_or_broken = false;

        for dep_id in &task.deps {
            let dep_status_str: Option<String> = tx
                .query_row(
                    "SELECT status FROM tasks WHERE run_id = ?1 AND id = ?2",
                    params![task.run_id, dep_id],
                    |r| r.get(0),
                )
                .optional()?;
            match dep_status_str {
                None => {
                    bail!(
                        "prerequisite task '{}' does not exist in run '{}'",
                        dep_id,
                        task.run_id
                    );
                }
                Some(s) => {
                    let st = TaskStatus::from_str(&s).unwrap_or(TaskStatus::Pending);
                    if st != TaskStatus::Completed {
                        all_completed = false;
                    }
                    if matches!(
                        st,
                        TaskStatus::Failed | TaskStatus::CircuitBroken | TaskStatus::Blocked
                    ) {
                        any_failed_or_broken = true;
                    }
                }
            }
        }

        // Verify no cycle is introduced
        let existing_deps = Self::load_run_deps_map_tx(tx, &task.run_id)?;
        let mut graph = existing_deps;
        graph.insert(task.id.clone(), task.deps.clone());
        if Self::has_cycle(&graph, &task.id) {
            bail!(
                "cycle detected in task dependencies for task '{}' in run '{}'",
                task.id,
                task.run_id
            );
        }

        let computed_status = if task.failure_count >= MAX_DISPATCH_FAILURES {
            TaskStatus::CircuitBroken
        } else if matches!(
            task.status,
            TaskStatus::Dispatched
                | TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::CircuitBroken
        ) {
            task.status
        } else if any_failed_or_broken {
            TaskStatus::Blocked
        } else if all_completed {
            TaskStatus::Ready
        } else {
            TaskStatus::Pending
        };

        let deps_json = serde_json::to_string(&task.deps)?;
        tx.execute(
            "INSERT INTO tasks (run_id, id, title, prompt, deps_json, failure_count, status, result_summary, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(run_id, id) DO UPDATE SET
                 title = excluded.title,
                 prompt = excluded.prompt,
                 deps_json = excluded.deps_json,
                 failure_count = excluded.failure_count,
                 status = excluded.status,
                 result_summary = COALESCE(excluded.result_summary, tasks.result_summary),
                 updated_at_ms = excluded.updated_at_ms",
            params![
                task.run_id,
                task.id,
                task.title,
                task.prompt,
                deps_json,
                task.failure_count,
                computed_status.as_str(),
                task.result_summary,
                task.created_at_ms,
                task.updated_at_ms,
            ],
        )?;

        let saved = tx.query_row(
            "SELECT run_id, id, title, prompt, deps_json, failure_count, status, result_summary, created_at_ms, updated_at_ms
             FROM tasks WHERE run_id = ?1 AND id = ?2",
            params![task.run_id, task.id],
            Self::map_task_row,
        )?;
        Ok(saved)
    }

    fn load_run_deps_map_tx(
        tx: &Transaction<'_>,
        run_id: &str,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let mut stmt = tx.prepare("SELECT id, deps_json FROM tasks WHERE run_id = ?1")?;
        let mut map = BTreeMap::new();
        let rows = stmt.query_map(params![run_id], |r| {
            let id: String = r.get(0)?;
            let deps_json: String = r.get(1)?;
            let deps: Vec<String> = serde_json::from_str(&deps_json).unwrap_or_default();
            Ok((id, deps))
        })?;
        for row in rows {
            let (id, deps) = row?;
            map.insert(id, deps);
        }
        Ok(map)
    }

    fn has_cycle(graph: &BTreeMap<String, Vec<String>>, start: &str) -> bool {
        fn dfs(
            node: &str,
            graph: &BTreeMap<String, Vec<String>>,
            visiting: &mut BTreeSet<String>,
            visited: &mut BTreeSet<String>,
        ) -> bool {
            if visiting.contains(node) {
                return true;
            }
            if visited.contains(node) {
                return false;
            }
            visiting.insert(node.to_string());
            if let Some(deps) = graph.get(node) {
                for dep in deps {
                    if dfs(dep, graph, visiting, visited) {
                        return true;
                    }
                }
            }
            visiting.remove(node);
            visited.insert(node.to_string());
            false
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        dfs(start, graph, &mut visiting, &mut visited)
    }

    pub fn get_task(&self, run_id: &str, task_id: &str) -> Result<Option<OrchTask>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let row = conn
            .query_row(
                "SELECT run_id, id, title, prompt, deps_json, failure_count, status, result_summary, created_at_ms, updated_at_ms
                 FROM tasks WHERE run_id = ?1 AND id = ?2",
                params![run_id, task_id],
                Self::map_task_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_tasks(&self, run_id: &str) -> Result<Vec<OrchTask>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT run_id, id, title, prompt, deps_json, failure_count, status, result_summary, created_at_ms, updated_at_ms
             FROM tasks WHERE run_id = ?1 ORDER BY created_at_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], Self::map_task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn promote_or_block_dependent_tasks(&self, run_id: &str, now_ms: i64) -> Result<()> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::promote_or_block_dependent_tasks_tx(&tx, run_id, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    fn promote_or_block_dependent_tasks_tx(
        tx: &Transaction<'_>,
        run_id: &str,
        now_ms: i64,
    ) -> Result<()> {
        // Loop until fixed point so multi-hop transitive blocked states propagate cleanly.
        loop {
            let mut stmt = tx.prepare(
                "SELECT run_id, id, title, prompt, deps_json, failure_count, status, result_summary, created_at_ms, updated_at_ms
                 FROM tasks WHERE run_id = ?1 ORDER BY created_at_ms ASC, id ASC",
            )?;
            let tasks = stmt
                .query_map(params![run_id], Self::map_task_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let status_map: BTreeMap<String, TaskStatus> =
                tasks.iter().map(|t| (t.id.clone(), t.status)).collect();

            let mut changed = false;
            for task in &tasks {
                if !matches!(task.status, TaskStatus::Pending | TaskStatus::Blocked) {
                    continue;
                }
                let mut all_completed = true;
                let mut any_failed_or_broken = false;
                for dep_id in &task.deps {
                    match status_map.get(dep_id) {
                        Some(TaskStatus::Completed) => {}
                        Some(
                            TaskStatus::Failed | TaskStatus::CircuitBroken | TaskStatus::Blocked,
                        ) => {
                            all_completed = false;
                            any_failed_or_broken = true;
                        }
                        _ => {
                            all_completed = false;
                        }
                    }
                }
                let target_status = if any_failed_or_broken {
                    TaskStatus::Blocked
                } else if all_completed {
                    TaskStatus::Ready
                } else {
                    TaskStatus::Pending
                };

                if target_status != task.status {
                    tx.execute(
                        "UPDATE tasks SET status = ?3, updated_at_ms = ?4 WHERE run_id = ?1 AND id = ?2",
                        params![run_id, task.id, target_status.as_str(), now_ms],
                    )?;
                    changed = true;
                }
            }

            if !changed {
                break;
            }
        }
        Ok(())
    }

    pub fn caller_orchestration_depth(
        &self,
        caller_terminal: &str,
        caller_created_ms: i64,
    ) -> Result<i32> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let parent_depth: Option<i32> = conn
            .query_row(
                "SELECT MAX(depth) FROM dispatch_contexts
                 WHERE assignee_terminal = ?1
                   AND terminal_created_ms = ?2
                   AND status IN ('starting', 'dispatched', 'waiting_for_reply')",
                params![caller_terminal, caller_created_ms],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(parent_depth.map(|d| d + 1).unwrap_or(1))
    }

    pub fn create_dispatch(&self, disp: &DispatchContext) -> Result<()> {
        if disp.depth < 1 || disp.depth > MAX_ORCH_DEPTH {
            bail!(
                "orchestration depth {} exceeds MAX_ORCH_DEPTH ({})",
                disp.depth,
                MAX_ORCH_DEPTH
            );
        }
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let task_row: Option<(i32, String)> = tx
            .query_row(
                "SELECT failure_count, status FROM tasks WHERE run_id = ?1 AND id = ?2",
                params![disp.run_id, disp.task_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((failure_count, status_str)) = task_row else {
            bail!(
                "task '{}' does not exist in run '{}'",
                disp.task_id,
                disp.run_id
            );
        };
        let task_status = TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Pending);
        if failure_count >= MAX_DISPATCH_FAILURES || task_status == TaskStatus::CircuitBroken {
            bail!(
                "circuit breaker tripped for task '{}' in run '{}' (failure_count = {})",
                disp.task_id,
                disp.run_id,
                failure_count
            );
        }

        let insert_sql = format!(
            "INSERT INTO dispatch_contexts ({DISPATCH_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
        );
        tx.execute(
            &insert_sql,
            params![
                disp.id,
                disp.run_id,
                disp.task_id,
                disp.parent_terminal,
                disp.assignee_terminal,
                disp.terminal_created_ms,
                disp.turn_at_dispatch as i64,
                disp.agent_kind,
                disp.worktree_mode,
                disp.worktree_path,
                disp.depth,
                disp.status.as_str(),
                disp.idle_since_ms,
                disp.blocked_since_ms,
                disp.created_at_ms,
                disp.updated_at_ms,
            ],
        )?;

        tx.execute(
            "UPDATE tasks SET status = ?3, updated_at_ms = ?4 WHERE run_id = ?1 AND id = ?2",
            params![
                disp.run_id,
                disp.task_id,
                TaskStatus::Dispatched.as_str(),
                disp.updated_at_ms
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    fn get_dispatch_tx(tx: &Transaction<'_>, dispatch_id: &str) -> Result<Option<DispatchContext>> {
        let sql = format!("SELECT {DISPATCH_COLUMNS} FROM dispatch_contexts WHERE id = ?1");
        let row = tx
            .query_row(&sql, params![dispatch_id], Self::map_dispatch_row)
            .optional()?;
        Ok(row)
    }

    pub fn get_dispatch(&self, dispatch_id: &str) -> Result<Option<DispatchContext>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let sql = format!("SELECT {DISPATCH_COLUMNS} FROM dispatch_contexts WHERE id = ?1");
        let row = conn
            .query_row(&sql, params![dispatch_id], Self::map_dispatch_row)
            .optional()?;
        Ok(row)
    }

    pub fn list_active_dispatches(&self) -> Result<Vec<DispatchContext>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let sql = format!(
            "SELECT {DISPATCH_COLUMNS}
             FROM dispatch_contexts
             WHERE status IN ('starting', 'dispatched', 'waiting_for_reply')
             ORDER BY created_at_ms ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], Self::map_dispatch_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_dispatches_for_run(&self, run_id: &str) -> Result<Vec<DispatchContext>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let sql = format!(
            "SELECT {DISPATCH_COLUMNS}
             FROM dispatch_contexts
             WHERE run_id = ?1
             ORDER BY created_at_ms ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![run_id], Self::map_dispatch_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn active_dispatches_by_parent(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT parent_terminal, assignee_terminal
             FROM dispatch_contexts
             WHERE status IN ('starting', 'dispatched', 'waiting_for_reply')
             ORDER BY created_at_ms ASC, id ASC",
        )?;
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (parent, assignee) = row?;
            let entry = map.entry(parent).or_default();
            if !entry.contains(&assignee) {
                entry.push(assignee);
            }
        }
        Ok(map)
    }

    pub fn update_dispatch_timers(
        &self,
        dispatch_id: &str,
        idle_since_ms: Option<i64>,
        blocked_since_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT idle_since_ms, blocked_since_ms FROM dispatch_contexts WHERE id = ?1",
                params![dispatch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((cur_idle, cur_blocked)) = current else {
            return Ok(false);
        };
        if cur_idle == idle_since_ms && cur_blocked == blocked_since_ms {
            return Ok(false);
        }
        tx.execute(
            "UPDATE dispatch_contexts
             SET idle_since_ms = ?2, blocked_since_ms = ?3, updated_at_ms = ?4
             WHERE id = ?1",
            params![dispatch_id, idle_since_ms, blocked_since_ms, now_ms],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn update_dispatch_status(
        &self,
        dispatch_id: &str,
        status: DispatchStatus,
        now_ms: i64,
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE dispatch_contexts SET status = ?2, updated_at_ms = ?3 WHERE id = ?1",
            params![dispatch_id, status.as_str(), now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn clear_dispatch_worktree_path(&self, dispatch_id: &str, now_ms: i64) -> Result<()> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE dispatch_contexts SET worktree_path = NULL, updated_at_ms = ?2 WHERE id = ?1",
            params![dispatch_id, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_task_failure(
        &self,
        run_id: &str,
        task_id: &str,
        dispatch_id: Option<&str>,
        reason: &str,
        now_ms: i64,
    ) -> Result<TaskStatus> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let coord_term: String = tx
            .query_row(
                "SELECT coordinator_terminal FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("run '{}' not found", run_id))?;

        let cur_failures: i32 = tx
            .query_row(
                "SELECT failure_count FROM tasks WHERE run_id = ?1 AND id = ?2",
                params![run_id, task_id],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("task '{}' not found in run '{}'", task_id, run_id))?;

        let new_failures = cur_failures + 1;
        let new_status = if new_failures >= MAX_DISPATCH_FAILURES {
            TaskStatus::CircuitBroken
        } else {
            TaskStatus::Failed
        };

        tx.execute(
            "UPDATE tasks
             SET failure_count = ?3, status = ?4, result_summary = ?5, updated_at_ms = ?6
             WHERE run_id = ?1 AND id = ?2",
            params![
                run_id,
                task_id,
                new_failures,
                new_status.as_str(),
                reason,
                now_ms
            ],
        )?;

        let mut target_terminal = coord_term;
        let mut from_handle = format!("task:{task_id}");

        if let Some(disp_id) = dispatch_id {
            if let Some((parent_term, assignee_term)) = tx
                .query_row(
                    "SELECT parent_terminal, assignee_terminal FROM dispatch_contexts WHERE id = ?1",
                    params![disp_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?
            {
                target_terminal = parent_term;
                from_handle = assignee_term;
            }
            tx.execute(
                "UPDATE dispatch_contexts SET status = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![disp_id, DispatchStatus::NeedsReview.as_str(), now_ms],
            )?;
        }

        let msg_id = generate_orch_id("msg");
        tx.execute(
            "INSERT INTO messages (
                id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'failed', ?9, ?10, NULL, 0, NULL, NULL, NULL, ?11)",
            params![
                msg_id,
                run_id,
                Some(task_id),
                dispatch_id,
                from_handle,
                target_terminal,
                target_terminal,
                MessageType::Escalation.as_str(),
                reason,
                PointerState::Pending.as_str(),
                now_ms,
            ],
        )?;

        Self::promote_or_block_dependent_tasks_tx(&tx, run_id, now_ms)?;
        tx.commit()?;
        Ok(new_status)
    }

    fn load_and_verify_fenced_dispatch_tx(
        tx: &Transaction<'_>,
        dispatch_id: &str,
        caller_terminal: &str,
        caller_created_ms: i64,
    ) -> Result<DispatchContext> {
        let disp = Self::get_dispatch_tx(tx, dispatch_id)?
            .with_context(|| format!("dispatch '{}' does not exist", dispatch_id))?;

        if disp.assignee_terminal != caller_terminal
            || disp.terminal_created_ms != caller_created_ms
        {
            bail!(
                "attempt fencing rejected for dispatch '{}': expected ({}, {}), got ({}, {})",
                dispatch_id,
                disp.assignee_terminal,
                disp.terminal_created_ms,
                caller_terminal,
                caller_created_ms
            );
        }
        Ok(disp)
    }

    pub fn record_worker_done(
        &self,
        dispatch_id: &str,
        caller_terminal: &str,
        caller_created_ms: i64,
        outcome: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<OrchMessage> {
        if !matches!(outcome, "succeeded" | "failed") {
            bail!(
                "invalid worker_done outcome '{}': must be 'succeeded' or 'failed'",
                outcome
            );
        }
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let disp = Self::load_and_verify_fenced_dispatch_tx(
            &tx,
            dispatch_id,
            caller_terminal,
            caller_created_ms,
        )?;

        if matches!(
            disp.status,
            DispatchStatus::Completed | DispatchStatus::Failed | DispatchStatus::Stopped
        ) {
            bail!(
                "worker_done already recorded for dispatch '{}' (status={}): worker_done may only be reported once per dispatch",
                dispatch_id,
                disp.status
            );
        }

        let already_has_worker_done: Option<String> = tx
            .query_row(
                "SELECT id FROM messages WHERE dispatch_id = ?1 AND msg_type = ?2 LIMIT 1",
                params![dispatch_id, MessageType::WorkerDone.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing_msg_id) = already_has_worker_done {
            bail!(
                "worker_done already recorded for dispatch '{}' (message={}): worker_done may only be reported once per dispatch",
                dispatch_id,
                existing_msg_id
            );
        }

        let (disp_status, task_status) = if outcome == "succeeded" {
            (DispatchStatus::Completed, TaskStatus::Completed)
        } else {
            let cur_failures: i32 = tx
                .query_row(
                    "SELECT failure_count FROM tasks WHERE run_id = ?1 AND id = ?2",
                    params![disp.run_id, disp.task_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let next_failures = cur_failures + 1;
            tx.execute(
                "UPDATE tasks SET failure_count = ?3 WHERE run_id = ?1 AND id = ?2",
                params![disp.run_id, disp.task_id, next_failures],
            )?;
            let t_status = if next_failures >= MAX_DISPATCH_FAILURES {
                TaskStatus::CircuitBroken
            } else {
                TaskStatus::Failed
            };
            (DispatchStatus::Failed, t_status)
        };

        tx.execute(
            "UPDATE dispatch_contexts
             SET status = ?2, idle_since_ms = NULL, blocked_since_ms = NULL, updated_at_ms = ?3
             WHERE id = ?1",
            params![disp.id, disp_status.as_str(), now_ms],
        )?;

        tx.execute(
            "UPDATE tasks
             SET status = ?3, result_summary = ?4, updated_at_ms = ?5
             WHERE run_id = ?1 AND id = ?2",
            params![
                disp.run_id,
                disp.task_id,
                task_status.as_str(),
                body,
                now_ms
            ],
        )?;

        let msg = OrchMessage {
            id: generate_orch_id("msg"),
            run_id: disp.run_id.clone(),
            task_id: Some(disp.task_id.clone()),
            dispatch_id: Some(disp.id.clone()),
            from_handle: disp.assignee_terminal.clone(),
            to_handle: disp.parent_terminal.clone(),
            target_terminal: disp.parent_terminal.clone(),
            msg_type: MessageType::WorkerDone,
            outcome: Some(outcome.to_string()),
            body: body.to_string(),
            pointer_state: PointerState::Pending,
            pointer_terminal: None,
            delivery_attempts: 0,
            next_retry_at_ms: None,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
            created_at_ms: now_ms,
        };

        Self::insert_message_tx(&tx, &msg)?;
        Self::promote_or_block_dependent_tasks_tx(&tx, &disp.run_id, now_ms)?;
        tx.commit()?;
        Ok(msg)
    }

    pub fn record_worker_message(
        &self,
        dispatch_id: &str,
        caller_terminal: &str,
        caller_created_ms: i64,
        msg_type: MessageType,
        body: &str,
        now_ms: i64,
    ) -> Result<OrchMessage> {
        if !matches!(msg_type, MessageType::Escalation | MessageType::Status) {
            bail!(
                "record_worker_message only accepts Escalation or Status, got {}",
                msg_type
            );
        }
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let disp = Self::load_and_verify_fenced_dispatch_tx(
            &tx,
            dispatch_id,
            caller_terminal,
            caller_created_ms,
        )?;

        let (pointer_state, acknowledged_at_ms) = if msg_type == MessageType::Status {
            (PointerState::Acknowledged, Some(now_ms))
        } else {
            tx.execute(
                "UPDATE dispatch_contexts SET status = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![disp.id, DispatchStatus::NeedsReview.as_str(), now_ms],
            )?;
            (PointerState::Pending, None)
        };

        let msg = OrchMessage {
            id: generate_orch_id("msg"),
            run_id: disp.run_id.clone(),
            task_id: Some(disp.task_id.clone()),
            dispatch_id: Some(disp.id.clone()),
            from_handle: disp.assignee_terminal.clone(),
            to_handle: disp.parent_terminal.clone(),
            target_terminal: disp.parent_terminal.clone(),
            msg_type,
            outcome: None,
            body: body.to_string(),
            pointer_state,
            pointer_terminal: None,
            delivery_attempts: 0,
            next_retry_at_ms: None,
            delivered_at_ms: None,
            acknowledged_at_ms,
            created_at_ms: now_ms,
        };

        Self::insert_message_tx(&tx, &msg)?;
        tx.commit()?;
        Ok(msg)
    }

    pub fn record_question(
        &self,
        dispatch_id: &str,
        caller_terminal: &str,
        caller_created_ms: i64,
        question_body: &str,
        now_ms: i64,
    ) -> Result<(QuestionThread, OrchMessage)> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let disp = Self::load_and_verify_fenced_dispatch_tx(
            &tx,
            dispatch_id,
            caller_terminal,
            caller_created_ms,
        )?;

        tx.execute(
            "UPDATE dispatch_contexts
             SET status = ?2, idle_since_ms = NULL, blocked_since_ms = NULL, updated_at_ms = ?3
             WHERE id = ?1",
            params![disp.id, DispatchStatus::WaitingForReply.as_str(), now_ms],
        )?;

        let msg = OrchMessage {
            id: generate_orch_id("msg"),
            run_id: disp.run_id.clone(),
            task_id: Some(disp.task_id.clone()),
            dispatch_id: Some(disp.id.clone()),
            from_handle: disp.assignee_terminal.clone(),
            to_handle: disp.parent_terminal.clone(),
            target_terminal: disp.parent_terminal.clone(),
            msg_type: MessageType::Question,
            outcome: None,
            body: question_body.to_string(),
            pointer_state: PointerState::Pending,
            pointer_terminal: None,
            delivery_attempts: 0,
            next_retry_at_ms: None,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
            created_at_ms: now_ms,
        };
        Self::insert_message_tx(&tx, &msg)?;

        let q_thread = QuestionThread {
            id: generate_orch_id("q"),
            run_id: disp.run_id,
            dispatch_id: disp.id,
            question_msg_id: msg.id.clone(),
            reply_msg_id: None,
            status: "open".to_string(),
            created_at_ms: now_ms,
            answered_at_ms: None,
        };

        tx.execute(
            "INSERT INTO question_threads (
                id, run_id, dispatch_id, question_msg_id, reply_msg_id, status, created_at_ms, answered_at_ms
             ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL)",
            params![
                q_thread.id,
                q_thread.run_id,
                q_thread.dispatch_id,
                q_thread.question_msg_id,
                q_thread.status,
                q_thread.created_at_ms,
            ],
        )?;

        tx.commit()?;
        Ok((q_thread, msg))
    }

    pub fn record_reply(
        &self,
        question_id: &str,
        reply_body: &str,
        child_current_turn: u64,
        now_ms: i64,
    ) -> Result<(QuestionThread, OrchMessage)> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let mut q_thread = tx
            .query_row(
                "SELECT id, run_id, dispatch_id, question_msg_id, reply_msg_id, status, created_at_ms, answered_at_ms
                 FROM question_threads WHERE id = ?1",
                params![question_id],
                Self::map_question_row,
            )
            .optional()?
            .with_context(|| format!("question thread '{}' does not exist", question_id))?;

        if q_thread.status != "open" {
            bail!(
                "question thread '{}' is already '{}'",
                question_id,
                q_thread.status
            );
        }

        let disp = Self::get_dispatch_tx(&tx, &q_thread.dispatch_id)?
            .with_context(|| format!("dispatch '{}' not found", q_thread.dispatch_id))?;

        let msg = OrchMessage {
            id: generate_orch_id("msg"),
            run_id: disp.run_id.clone(),
            task_id: Some(disp.task_id.clone()),
            dispatch_id: Some(disp.id.clone()),
            from_handle: disp.parent_terminal.clone(),
            to_handle: disp.assignee_terminal.clone(),
            target_terminal: disp.assignee_terminal.clone(),
            msg_type: MessageType::Reply,
            outcome: None,
            body: reply_body.to_string(),
            pointer_state: PointerState::Pending,
            pointer_terminal: None,
            delivery_attempts: 0,
            next_retry_at_ms: None,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
            created_at_ms: now_ms,
        };
        Self::insert_message_tx(&tx, &msg)?;

        q_thread.reply_msg_id = Some(msg.id.clone());
        q_thread.status = "answered".to_string();
        q_thread.answered_at_ms = Some(now_ms);

        tx.execute(
            "UPDATE question_threads
             SET reply_msg_id = ?2, status = 'answered', answered_at_ms = ?3
             WHERE id = ?1",
            params![q_thread.id, msg.id, now_ms],
        )?;

        tx.execute(
            "UPDATE dispatch_contexts
             SET status = ?2, turn_at_dispatch = ?3, idle_since_ms = NULL, blocked_since_ms = NULL, updated_at_ms = ?4
             WHERE id = ?1",
            params![
                disp.id,
                DispatchStatus::Dispatched.as_str(),
                child_current_turn as i64,
                now_ms
            ],
        )?;

        tx.commit()?;
        Ok((q_thread, msg))
    }

    pub fn get_question_thread(&self, question_id: &str) -> Result<Option<QuestionThread>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let row = conn
            .query_row(
                "SELECT id, run_id, dispatch_id, question_msg_id, reply_msg_id, status, created_at_ms, answered_at_ms
                 FROM question_threads WHERE id = ?1",
                params![question_id],
                Self::map_question_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_open_questions_for_run(&self, run_id: &str) -> Result<Vec<QuestionThread>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, run_id, dispatch_id, question_msg_id, reply_msg_id, status, created_at_ms, answered_at_ms
             FROM question_threads
             WHERE run_id = ?1 AND status = 'open'
             ORDER BY created_at_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], Self::map_question_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn has_open_question_for_dispatch(&self, dispatch_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let exists: Option<i32> = conn
            .query_row(
                "SELECT 1 FROM question_threads WHERE dispatch_id = ?1 AND status = 'open' LIMIT 1",
                params![dispatch_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    pub fn has_message(&self, msg_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let exists: Option<i32> = conn
            .query_row(
                "SELECT 1 FROM messages WHERE id = ?1",
                params![msg_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    pub fn record_turn_stop_escalation(
        &self,
        dispatch_id: &str,
        turn: u64,
        reason: &str,
        tail_preview: &str,
        now_ms: i64,
    ) -> Result<Option<OrchMessage>> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let disp = Self::get_dispatch_tx(&tx, dispatch_id)?;
        let Some(disp) = disp else {
            return Ok(None);
        };
        if disp.status != DispatchStatus::Dispatched {
            return Ok(None);
        }

        let open_q: Option<i32> = tx
            .query_row(
                "SELECT 1 FROM question_threads WHERE dispatch_id = ?1 AND status = 'open' LIMIT 1",
                params![dispatch_id],
                |r| r.get(0),
            )
            .optional()?;
        if open_q.is_some() {
            return Ok(None);
        }

        let msg_id = format!("msg_stop_{dispatch_id}_{turn}");
        let body = if tail_preview.trim().is_empty() {
            format!(
                "Worker {} stopped on turn {} (reason: {}).",
                disp.assignee_terminal, turn, reason
            )
        } else {
            format!(
                "Worker {} stopped on turn {} (reason: {}).\n--- last lines ---\n{}",
                disp.assignee_terminal,
                turn,
                reason,
                tail_preview.trim()
            )
        };

        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages (
                id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, NULL, 0, NULL, NULL, NULL, ?11)",
            params![
                msg_id,
                disp.run_id,
                Some(disp.task_id.clone()),
                Some(disp.id.clone()),
                disp.assignee_terminal,
                disp.parent_terminal,
                disp.parent_terminal,
                MessageType::Escalation.as_str(),
                body,
                PointerState::Pending.as_str(),
                now_ms,
            ],
        )?;

        tx.execute(
            "UPDATE dispatch_contexts
             SET status = ?2, updated_at_ms = ?3
             WHERE id = ?1",
            params![disp.id, DispatchStatus::NeedsReview.as_str(), now_ms],
        )?;

        if inserted == 0 {
            tx.commit()?;
            return Ok(None);
        }

        let msg = tx.query_row(
            "SELECT id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                    msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                    next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
             FROM messages WHERE id = ?1",
            params![msg_id],
            Self::map_message_row,
        )?;
        tx.commit()?;
        Ok(Some(msg))
    }

    pub fn record_blocked_escalation(
        &self,
        dispatch_id: &str,
        turn: u64,
        status_msg: Option<&str>,
        tail_preview: &str,
        now_ms: i64,
    ) -> Result<Option<OrchMessage>> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let disp = Self::get_dispatch_tx(&tx, dispatch_id)?;
        let Some(disp) = disp else {
            return Ok(None);
        };
        if disp.status != DispatchStatus::Dispatched {
            return Ok(None);
        }

        let msg_id = format!("msg_block_{dispatch_id}_{turn}");
        let status_detail = status_msg.unwrap_or("interactive prompt");
        let body = if tail_preview.trim().is_empty() {
            format!(
                "Worker {} blocked on turn {} (reason: worker_blocked_on_prompt, detail: {}).",
                disp.assignee_terminal, turn, status_detail
            )
        } else {
            format!(
                "Worker {} blocked on turn {} (reason: worker_blocked_on_prompt, detail: {}).\n--- last lines ---\n{}",
                disp.assignee_terminal,
                turn,
                status_detail,
                tail_preview.trim()
            )
        };

        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages (
                id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, NULL, 0, NULL, NULL, NULL, ?11)",
            params![
                msg_id,
                disp.run_id,
                Some(disp.task_id.clone()),
                Some(disp.id.clone()),
                disp.assignee_terminal,
                disp.parent_terminal,
                disp.parent_terminal,
                MessageType::Escalation.as_str(),
                body,
                PointerState::Pending.as_str(),
                now_ms,
            ],
        )?;

        if inserted == 0 {
            tx.commit()?;
            return Ok(None);
        }

        let msg = tx.query_row(
            "SELECT id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                    msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                    next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
             FROM messages WHERE id = ?1",
            params![msg_id],
            Self::map_message_row,
        )?;
        tx.commit()?;
        Ok(Some(msg))
    }

    pub fn requeue_expired_pointers_detailed(&self, now_ms: i64) -> Result<(usize, usize)> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let stalled = tx.execute(
            "UPDATE messages
             SET pointer_state = 'delivery_stalled'
             WHERE pointer_state = 'delivered'
               AND next_retry_at_ms IS NOT NULL
               AND next_retry_at_ms <= ?1
               AND delivery_attempts >= ?2",
            params![now_ms, MAX_DELIVERY_ATTEMPTS],
        )?;

        let requeued = tx.execute(
            "UPDATE messages
             SET pointer_state = 'pending'
             WHERE pointer_state = 'delivered'
               AND next_retry_at_ms IS NOT NULL
               AND next_retry_at_ms <= ?1
               AND delivery_attempts < ?2",
            params![now_ms, MAX_DELIVERY_ATTEMPTS],
        )?;

        tx.commit()?;
        Ok((requeued, stalled))
    }

    pub fn requeue_expired_pointers(&self, now_ms: i64) -> Result<usize> {
        let (requeued, stalled) = self.requeue_expired_pointers_detailed(now_ms)?;
        Ok(requeued + stalled)
    }

    pub fn pending_pointer_targets(&self, now_ms: i64) -> Result<Vec<PendingPointerTarget>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT target_terminal,
                    run_id,
                    SUM(CASE WHEN pointer_state = 'pending' THEN 1 ELSE 0 END) AS pending_cnt,
                    SUM(CASE WHEN pointer_state = 'delivered' AND (next_retry_at_ms IS NULL OR next_retry_at_ms > ?1) THEN 1 ELSE 0 END) AS active_delivered_cnt
             FROM messages
             WHERE pointer_state IN ('pending', 'delivered')
             GROUP BY target_terminal, run_id
             HAVING pending_cnt > 0
             ORDER BY MIN(created_at_ms) ASC, target_terminal ASC, run_id ASC",
        )?;

        let rows = stmt
            .query_map(params![now_ms], |r| {
                let target_terminal: String = r.get(0)?;
                let run_id: String = r.get(1)?;
                let pending_cnt: i64 = r.get(2)?;
                let active_delivered_cnt: i64 = r.get(3)?;
                Ok(PendingPointerTarget {
                    target_terminal,
                    run_id,
                    pending_count: pending_cnt.max(0) as usize,
                    has_active_unexpired_pointer: active_delivered_cnt > 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn mark_pointer_delivered(
        &self,
        target_terminal: &str,
        run_id: &str,
        now_ms: i64,
    ) -> Result<usize> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_retry = now_ms + POINTER_RETRY_BACKOFF_MS;
        let updated = tx.execute(
            "UPDATE messages
             SET pointer_state = 'delivered',
                 pointer_terminal = ?1,
                 delivery_attempts = delivery_attempts + 1,
                 delivered_at_ms = ?3,
                 next_retry_at_ms = ?4
             WHERE target_terminal = ?1
               AND run_id = ?2
               AND pointer_state = 'pending'",
            params![target_terminal, run_id, now_ms, next_retry],
        )?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn claim_inbox_messages(
        &self,
        target_terminal: &str,
        run_id_filter: Option<&str>,
        peek: bool,
        now_ms: i64,
    ) -> Result<InboxCheckResult> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let mut messages = if let Some(run_id) = run_id_filter {
            let mut stmt = tx.prepare(
                "SELECT id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                        msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                        next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
                 FROM messages
                 WHERE target_terminal = ?1
                   AND run_id = ?2
                   AND pointer_state IN ('pending', 'delivered', 'delivery_stalled')
                 ORDER BY created_at_ms ASC, id ASC",
            )?;
            let rows = stmt
                .query_map(params![target_terminal, run_id], Self::map_message_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let mut stmt = tx.prepare(
                "SELECT id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                        msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                        next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
                 FROM messages
                 WHERE target_terminal = ?1
                   AND pointer_state IN ('pending', 'delivered', 'delivery_stalled')
                 ORDER BY created_at_ms ASC, id ASC",
            )?;
            let rows = stmt
                .query_map(params![target_terminal], Self::map_message_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        if !peek && !messages.is_empty() {
            if let Some(run_id) = run_id_filter {
                tx.execute(
                    "UPDATE messages
                     SET pointer_state = 'acknowledged',
                         acknowledged_at_ms = ?3
                     WHERE target_terminal = ?1
                       AND run_id = ?2
                       AND pointer_state IN ('pending', 'delivered', 'delivery_stalled')",
                    params![target_terminal, run_id, now_ms],
                )?;
            } else {
                tx.execute(
                    "UPDATE messages
                     SET pointer_state = 'acknowledged',
                         acknowledged_at_ms = ?2
                     WHERE target_terminal = ?1
                       AND pointer_state IN ('pending', 'delivered', 'delivery_stalled')",
                    params![target_terminal, now_ms],
                )?;
            }
            for msg in &mut messages {
                msg.pointer_state = PointerState::Acknowledged;
                msg.acknowledged_at_ms = Some(now_ms);
            }
        }

        let mut relevant_runs = BTreeSet::new();
        if let Some(run_id) = run_id_filter {
            relevant_runs.insert(run_id.to_string());
        } else {
            for msg in &messages {
                relevant_runs.insert(msg.run_id.clone());
            }
            let mut run_stmt = tx.prepare("SELECT id FROM runs WHERE coordinator_terminal = ?1")?;
            let run_ids = run_stmt
                .query_map(params![target_terminal], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for r_id in run_ids {
                relevant_runs.insert(r_id);
            }
        }

        let mut ready_tasks = Vec::new();
        let mut blocked_tasks = Vec::new();
        let mut active_dispatches = Vec::new();

        for r_id in &relevant_runs {
            let mut task_stmt = tx.prepare(
                "SELECT run_id, id, title, prompt, deps_json, failure_count, status, result_summary, created_at_ms, updated_at_ms
                 FROM tasks
                 WHERE run_id = ?1 AND status IN ('ready', 'blocked', 'circuit_broken')
                 ORDER BY created_at_ms ASC, id ASC",
            )?;
            let tasks = task_stmt
                .query_map(params![r_id], Self::map_task_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for t in tasks {
                if t.status == TaskStatus::Ready {
                    ready_tasks.push(t);
                } else {
                    blocked_tasks.push(t);
                }
            }

            let disp_sql = format!(
                "SELECT {DISPATCH_COLUMNS}
                 FROM dispatch_contexts
                 WHERE run_id = ?1 AND status IN ('starting', 'dispatched', 'waiting_for_reply', 'needs_review')
                 ORDER BY created_at_ms ASC, id ASC"
            );
            let mut disp_stmt = tx.prepare(&disp_sql)?;
            let disps = disp_stmt
                .query_map(params![r_id], Self::map_dispatch_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            active_dispatches.extend(disps);
        }

        tx.commit()?;
        Ok(InboxCheckResult {
            target_terminal: target_terminal.to_string(),
            run_id_filter: run_id_filter.map(String::from),
            messages,
            ready_tasks,
            blocked_tasks,
            active_dispatches,
        })
    }

    fn verify_parent_caller_tx(
        tx: &Transaction<'_>,
        disp: &DispatchContext,
        caller_terminal: &str,
        caller_created_ms: i64,
    ) -> Result<()> {
        if disp.parent_terminal != caller_terminal {
            bail!(
                "attempt fencing rejected for dispatch '{}': only parent '{}' may act on worker '{}' (got caller '{}')",
                disp.id,
                disp.parent_terminal,
                disp.assignee_terminal,
                caller_terminal
            );
        }

        let coord_created: Option<(String, i64)> = tx
            .query_row(
                "SELECT coordinator_terminal, coordinator_created_ms FROM runs WHERE id = ?1",
                params![disp.run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((coord_term, coord_ms)) = coord_created {
            if coord_term == disp.parent_terminal && coord_ms != caller_created_ms {
                bail!(
                    "attempt fencing rejected for dispatch '{}': expected parent ({}, {}), got ({}, {})",
                    disp.id,
                    disp.parent_terminal,
                    coord_ms,
                    caller_terminal,
                    caller_created_ms
                );
            }
        }
        Ok(())
    }

    pub fn verify_parent_caller(
        &self,
        dispatch_id: &str,
        caller_terminal: &str,
        caller_created_ms: i64,
    ) -> Result<DispatchContext> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let disp = Self::get_dispatch_tx(&tx, dispatch_id)?
            .with_context(|| format!("dispatch '{}' does not exist", dispatch_id))?;
        Self::verify_parent_caller_tx(&tx, &disp, caller_terminal, caller_created_ms)?;
        Ok(disp)
    }

    pub fn list_messages_for_run(&self, run_id: &str) -> Result<Vec<OrchMessage>> {
        let conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                    msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                    next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
             FROM messages
             WHERE run_id = ?1
             ORDER BY created_at_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], Self::map_message_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_coordinator_message(
        &self,
        dispatch_id: &str,
        caller_terminal: &str,
        caller_created_ms: i64,
        body: &str,
        delivered_immediately: bool,
        child_current_turn: u64,
        now_ms: i64,
    ) -> Result<(OrchMessage, DispatchStatus)> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let disp = Self::get_dispatch_tx(&tx, dispatch_id)?
            .with_context(|| format!("dispatch '{}' does not exist", dispatch_id))?;
        Self::verify_parent_caller_tx(&tx, &disp, caller_terminal, caller_created_ms)?;

        if matches!(
            disp.status,
            DispatchStatus::Completed
                | DispatchStatus::Failed
                | DispatchStatus::Stopped
                | DispatchStatus::Superseded
        ) {
            bail!(
                "cannot send message to dispatch '{}' because its status is '{}'; use `tchelvis orch retry {}` to launch a new attempt",
                disp.id,
                disp.status,
                disp.id
            );
        }

        let (
            pointer_state,
            pointer_terminal,
            delivery_attempts,
            delivered_at_ms,
            acknowledged_at_ms,
        ) = if delivered_immediately {
            (
                PointerState::Acknowledged,
                Some(disp.assignee_terminal.clone()),
                1,
                Some(now_ms),
                Some(now_ms),
            )
        } else {
            (PointerState::Pending, None, 0, None, None)
        };

        let msg = OrchMessage {
            id: generate_orch_id("msg"),
            run_id: disp.run_id.clone(),
            task_id: Some(disp.task_id.clone()),
            dispatch_id: Some(disp.id.clone()),
            from_handle: disp.parent_terminal.clone(),
            to_handle: disp.assignee_terminal.clone(),
            target_terminal: disp.assignee_terminal.clone(),
            msg_type: MessageType::Directive,
            outcome: None,
            body: body.to_string(),
            pointer_state,
            pointer_terminal,
            delivery_attempts,
            next_retry_at_ms: None,
            delivered_at_ms,
            acknowledged_at_ms,
            created_at_ms: now_ms,
        };
        Self::insert_message_tx(&tx, &msg)?;

        let new_disp_status = if disp.status == DispatchStatus::WaitingForReply {
            DispatchStatus::WaitingForReply
        } else {
            DispatchStatus::Dispatched
        };
        let next_turn = child_current_turn.max(disp.turn_at_dispatch);
        tx.execute(
            "UPDATE dispatch_contexts
             SET status = ?2, turn_at_dispatch = ?3, idle_since_ms = NULL, blocked_since_ms = NULL, updated_at_ms = ?4
             WHERE id = ?1",
            params![disp.id, new_disp_status.as_str(), next_turn as i64, now_ms],
        )?;

        tx.commit()?;
        Ok((msg, new_disp_status))
    }

    pub fn supersede_dispatch(&self, dispatch_id: &str, now_ms: i64) -> Result<DispatchContext> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let disp = Self::get_dispatch_tx(&tx, dispatch_id)?
            .with_context(|| format!("dispatch '{}' does not exist", dispatch_id))?;
        if matches!(
            disp.status,
            DispatchStatus::Completed | DispatchStatus::Superseded
        ) {
            bail!(
                "dispatch '{}' has status '{}' and cannot be superseded",
                disp.id,
                disp.status
            );
        }

        tx.execute(
            "UPDATE dispatch_contexts
             SET status = ?2, idle_since_ms = NULL, blocked_since_ms = NULL, updated_at_ms = ?3
             WHERE id = ?1",
            params![disp.id, DispatchStatus::Superseded.as_str(), now_ms],
        )?;
        tx.execute(
            "UPDATE question_threads
             SET status = 'cancelled'
             WHERE dispatch_id = ?1 AND status = 'open'",
            params![disp.id],
        )?;

        let updated = Self::get_dispatch_tx(&tx, &disp.id)?
            .with_context(|| format!("dispatch '{}' disappeared after update", disp.id))?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn stop_dispatch(
        &self,
        dispatch_id: &str,
        now_ms: i64,
    ) -> Result<(DispatchContext, TaskStatus)> {
        let mut conn = self.conn.lock().expect("orch sqlite mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let disp = Self::get_dispatch_tx(&tx, dispatch_id)?
            .with_context(|| format!("dispatch '{}' does not exist", dispatch_id))?;
        if matches!(
            disp.status,
            DispatchStatus::Completed | DispatchStatus::Superseded
        ) {
            bail!(
                "dispatch '{}' has status '{}' and cannot be stopped",
                disp.id,
                disp.status
            );
        }

        tx.execute(
            "UPDATE dispatch_contexts
             SET status = ?2, idle_since_ms = NULL, blocked_since_ms = NULL, updated_at_ms = ?3
             WHERE id = ?1",
            params![disp.id, DispatchStatus::Stopped.as_str(), now_ms],
        )?;
        tx.execute(
            "UPDATE question_threads
             SET status = 'cancelled'
             WHERE dispatch_id = ?1 AND status = 'open'",
            params![disp.id],
        )?;

        let task = tx
            .query_row(
                "SELECT run_id, id, title, prompt, deps_json, failure_count, status, result_summary, created_at_ms, updated_at_ms
                 FROM tasks WHERE run_id = ?1 AND id = ?2",
                params![disp.run_id, disp.task_id],
                Self::map_task_row,
            )
            .optional()?
            .with_context(|| format!("task '{}' not found in run '{}'", disp.task_id, disp.run_id))?;

        let next_task_status = if matches!(
            task.status,
            TaskStatus::CircuitBroken | TaskStatus::Completed
        ) {
            task.status
        } else {
            let other_active: Option<i32> = tx
                .query_row(
                    "SELECT 1 FROM dispatch_contexts
                     WHERE run_id = ?1 AND task_id = ?2 AND id != ?3
                       AND status IN ('starting', 'dispatched', 'waiting_for_reply')
                     LIMIT 1",
                    params![disp.run_id, disp.task_id, disp.id],
                    |r| r.get(0),
                )
                .optional()?;
            if other_active.is_some() {
                TaskStatus::Dispatched
            } else {
                let mut all_completed = true;
                let mut any_failed_or_broken = false;
                for dep_id in &task.deps {
                    let dep_st_str: Option<String> = tx
                        .query_row(
                            "SELECT status FROM tasks WHERE run_id = ?1 AND id = ?2",
                            params![disp.run_id, dep_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match dep_st_str.and_then(|s| TaskStatus::from_str(&s).ok()) {
                        Some(TaskStatus::Completed) => {}
                        Some(
                            TaskStatus::Failed | TaskStatus::CircuitBroken | TaskStatus::Blocked,
                        ) => {
                            all_completed = false;
                            any_failed_or_broken = true;
                        }
                        _ => {
                            all_completed = false;
                        }
                    }
                }
                if any_failed_or_broken {
                    TaskStatus::Blocked
                } else if all_completed {
                    TaskStatus::Ready
                } else {
                    TaskStatus::Pending
                }
            }
        };

        tx.execute(
            "UPDATE tasks SET status = ?3, updated_at_ms = ?4 WHERE run_id = ?1 AND id = ?2",
            params![disp.run_id, disp.task_id, next_task_status.as_str(), now_ms],
        )?;

        Self::promote_or_block_dependent_tasks_tx(&tx, &disp.run_id, now_ms)?;

        let updated_disp = Self::get_dispatch_tx(&tx, &disp.id)?
            .with_context(|| format!("dispatch '{}' disappeared after update", disp.id))?;
        let final_task_status = tx
            .query_row(
                "SELECT status FROM tasks WHERE run_id = ?1 AND id = ?2",
                params![disp.run_id, disp.task_id],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| TaskStatus::from_str(&s).ok())
            .unwrap_or(next_task_status);

        tx.commit()?;
        Ok((updated_disp, final_task_status))
    }

    fn insert_message_tx(tx: &Transaction<'_>, msg: &OrchMessage) -> Result<()> {
        tx.execute(
            "INSERT INTO messages (
                id, run_id, task_id, dispatch_id, from_handle, to_handle, target_terminal,
                msg_type, outcome, body, pointer_state, pointer_terminal, delivery_attempts,
                next_retry_at_ms, delivered_at_ms, acknowledged_at_ms, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                msg.id,
                msg.run_id,
                msg.task_id,
                msg.dispatch_id,
                msg.from_handle,
                msg.to_handle,
                msg.target_terminal,
                msg.msg_type.as_str(),
                msg.outcome,
                msg.body,
                msg.pointer_state.as_str(),
                msg.pointer_terminal,
                msg.delivery_attempts,
                msg.next_retry_at_ms,
                msg.delivered_at_ms,
                msg.acknowledged_at_ms,
                msg.created_at_ms,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_run(run_id: &str, coord: &str) -> OrchRun {
        OrchRun {
            id: run_id.to_string(),
            title: format!("Run {run_id}"),
            coordinator_terminal: coord.to_string(),
            coordinator_created_ms: 1_000,
            status: RunStatus::Active,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        }
    }

    fn sample_task(run_id: &str, task_id: &str, deps: Vec<&str>) -> OrchTask {
        OrchTask {
            id: task_id.to_string(),
            run_id: run_id.to_string(),
            title: format!("Task {task_id}"),
            prompt: format!("Do {task_id}"),
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
    ) -> DispatchContext {
        DispatchContext {
            id: disp_id.to_string(),
            run_id: run_id.to_string(),
            task_id: task_id.to_string(),
            parent_terminal: parent.to_string(),
            assignee_terminal: assignee.to_string(),
            terminal_created_ms: created_ms,
            turn_at_dispatch: 0,
            agent_kind: "claude".to_string(),
            worktree_mode: "current".to_string(),
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
    fn test_composite_task_keys_across_runs_and_dag_promotion() {
        let store = OrchStore::open_in_memory().unwrap();
        store.create_run(&sample_run("run_1", "t1")).unwrap();
        store.create_run(&sample_run("run_2", "t1")).unwrap();

        // Reuse task_a across run_1 and run_2 via composite PRIMARY KEY (run_id, id)
        let t1_a = store
            .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
            .unwrap();
        let t2_a = store
            .create_or_update_task(&sample_task("run_2", "task_a", vec![]))
            .unwrap();
        assert_eq!(t1_a.status, TaskStatus::Ready);
        assert_eq!(t2_a.status, TaskStatus::Ready);

        // Create dependent task_b -> task_a and task_c -> task_b in run_1
        let t1_b = store
            .create_or_update_task(&sample_task("run_1", "task_b", vec!["task_a"]))
            .unwrap();
        let t1_c = store
            .create_or_update_task(&sample_task("run_1", "task_c", vec!["task_b"]))
            .unwrap();
        assert_eq!(t1_b.status, TaskStatus::Pending);
        assert_eq!(t1_c.status, TaskStatus::Pending);

        // Dispatch task_a in run_1
        let disp_a = sample_dispatch("disp_1", "run_1", "task_a", "t1", "t2", 2_000);
        store.create_dispatch(&disp_a).unwrap();
        assert_eq!(
            store.get_task("run_1", "task_a").unwrap().unwrap().status,
            TaskStatus::Dispatched
        );

        // Complete task_a -> task_b promotes to Ready, task_c stays Pending
        store
            .record_worker_done("disp_1", "t2", 2_000, "succeeded", "Done A", 2_500)
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
        // run_2's task_a is untouched
        assert_eq!(
            store.get_task("run_2", "task_a").unwrap().unwrap().status,
            TaskStatus::Ready
        );

        // If task_b fails, task_c transitions to Blocked
        let disp_b = sample_dispatch("disp_2", "run_1", "task_b", "t1", "t3", 3_000);
        store.create_dispatch(&disp_b).unwrap();
        store
            .record_worker_done("disp_2", "t3", 3_000, "failed", "Broken B", 3_500)
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
    fn test_attempt_fencing_rejects_mismatched_terminal_or_created_ms() {
        let store = OrchStore::open_in_memory().unwrap();
        store.create_run(&sample_run("run_1", "t1")).unwrap();
        store
            .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
            .unwrap();
        store
            .create_dispatch(&sample_dispatch(
                "disp_1", "run_1", "task_a", "t1", "t2", 5_000,
            ))
            .unwrap();

        // Wrong terminal ID
        let err1 = store
            .record_worker_done("disp_1", "t9", 5_000, "succeeded", "spoof", 5_100)
            .unwrap_err();
        assert!(err1.to_string().contains("attempt fencing rejected"));

        // Same terminal ID (t2), but recycled with a newer created_ms
        let err2 = store
            .record_worker_done("disp_1", "t2", 9_999, "succeeded", "stale", 5_200)
            .unwrap_err();
        assert!(err2.to_string().contains("attempt fencing rejected"));

        // Exact match succeeds
        let msg = store
            .record_worker_done("disp_1", "t2", 5_000, "succeeded", "valid", 5_300)
            .unwrap();
        assert_eq!(msg.msg_type, MessageType::WorkerDone);
    }

    #[test]
    fn test_circuit_breaker_trips_at_three_failures() {
        let store = OrchStore::open_in_memory().unwrap();
        store.create_run(&sample_run("run_1", "t1")).unwrap();
        store
            .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
            .unwrap();
        store
            .create_or_update_task(&sample_task("run_1", "task_b", vec!["task_a"]))
            .unwrap();

        for i in 1..=2 {
            let st = store
                .record_task_failure("run_1", "task_a", None, &format!("fail {i}"), 1_000 + i)
                .unwrap();
            assert_eq!(st, TaskStatus::Failed);
        }

        // 3rd failure trips circuit_broken
        let st3 = store
            .record_task_failure("run_1", "task_a", None, "fail 3", 2_000)
            .unwrap();
        assert_eq!(st3, TaskStatus::CircuitBroken);

        let task_a = store.get_task("run_1", "task_a").unwrap().unwrap();
        assert_eq!(task_a.failure_count, 3);
        assert_eq!(task_a.status, TaskStatus::CircuitBroken);

        // Dependent task_b is blocked
        let task_b = store.get_task("run_1", "task_b").unwrap().unwrap();
        assert_eq!(task_b.status, TaskStatus::Blocked);

        // Further dispatch attempts on task_a are rejected by circuit breaker
        let err = store
            .create_dispatch(&sample_dispatch(
                "disp_blocked",
                "run_1",
                "task_a",
                "t1",
                "t2",
                3_000,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("circuit breaker tripped"));
    }

    #[test]
    fn test_deterministic_escalations_and_target_terminal_scoped_inbox() {
        let store = OrchStore::open_in_memory().unwrap();
        store.create_run(&sample_run("run_1", "t1")).unwrap();
        store
            .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
            .unwrap();
        store
            .create_dispatch(&sample_dispatch(
                "disp_1", "run_1", "task_a", "t1", "t2", 2_000,
            ))
            .unwrap();

        // First blocked escalation on turn 1 inserts msg_block_disp_1_1
        let esc1 = store
            .record_blocked_escalation("disp_1", 1, Some("Allow bash?"), "[y/N]", 3_000)
            .unwrap();
        assert!(esc1.is_some());
        assert_eq!(esc1.unwrap().id, "msg_block_disp_1_1");
        assert!(store.has_message("msg_block_disp_1_1").unwrap());

        // Second call for same (dispatch_id, turn) is ignored via INSERT OR IGNORE
        let esc_dup = store
            .record_blocked_escalation("disp_1", 1, Some("Allow bash?"), "[y/N]", 3_500)
            .unwrap();
        assert!(esc_dup.is_none());

        // Turn-stop escalation transitions dispatch to NeedsReview
        let stop1 = store
            .record_turn_stop_escalation(
                "disp_1",
                1,
                "worker_returned_to_prompt_without_worker_done",
                "prompt$",
                4_000,
            )
            .unwrap();
        assert!(stop1.is_some());
        assert_eq!(stop1.unwrap().id, "msg_stop_disp_1_1");
        assert_eq!(
            store.get_dispatch("disp_1").unwrap().unwrap().status,
            DispatchStatus::NeedsReview
        );

        // Child t2 calling claim_inbox_messages for run_1 must NOT see or claim parent t1's messages
        let child_inbox = store
            .claim_inbox_messages("t2", Some("run_1"), false, 4_100)
            .unwrap();
        assert!(child_inbox.messages.is_empty());

        // Parent t1 sees both messages and acknowledges them
        let parent_inbox = store
            .claim_inbox_messages("t1", Some("run_1"), false, 4_200)
            .unwrap();
        assert_eq!(parent_inbox.messages.len(), 2);
        assert!(parent_inbox
            .messages
            .iter()
            .all(|m| m.pointer_state == PointerState::Acknowledged));

        // Subsequent check by t1 has 0 unacknowledged messages
        let parent_inbox_after = store
            .claim_inbox_messages("t1", Some("run_1"), false, 4_300)
            .unwrap();
        assert!(parent_inbox_after.messages.is_empty());
    }

    #[test]
    fn test_question_reply_and_pointer_lifecycle() {
        let store = OrchStore::open_in_memory().unwrap();
        store.create_run(&sample_run("run_1", "t1")).unwrap();
        store
            .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
            .unwrap();
        store
            .create_dispatch(&sample_dispatch(
                "disp_1", "run_1", "task_a", "t1", "t2", 2_000,
            ))
            .unwrap();

        // Worker t2 asks a question
        let (q, q_msg) = store
            .record_question("disp_1", "t2", 2_000, "15m or 1h?", 2_100)
            .unwrap();
        assert_eq!(q.status, "open");
        assert_eq!(q_msg.target_terminal, "t1");
        assert!(store.has_open_question_for_dispatch("disp_1").unwrap());
        assert_eq!(
            store.get_dispatch("disp_1").unwrap().unwrap().status,
            DispatchStatus::WaitingForReply
        );

        // Pending pointer target for t1
        let targets = store.pending_pointer_targets(2_200).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target_terminal, "t1");
        assert_eq!(targets[0].pending_count, 1);
        assert!(!targets[0].has_active_unexpired_pointer);

        // Mark pointer delivered
        store.mark_pointer_delivered("t1", "run_1", 2_300).unwrap();
        // Before 30s retry backoff expires, requeue does nothing
        assert_eq!(store.requeue_expired_pointers(10_000).unwrap(), 0);
        // After 30s retry backoff, requeues back to pending
        assert_eq!(store.requeue_expired_pointers(33_000).unwrap(), 1);

        // Parent t1 replies to question
        let (answered_q, reply_msg) = store.record_reply(&q.id, "Use 15m", 2, 34_000).unwrap();
        assert_eq!(answered_q.status, "answered");
        assert_eq!(reply_msg.target_terminal, "t2");
        let disp = store.get_dispatch("disp_1").unwrap().unwrap();
        assert_eq!(disp.status, DispatchStatus::Dispatched);
        assert_eq!(disp.turn_at_dispatch, 2);
    }

    #[test]
    fn test_coordinator_message_supersede_and_stop_lifecycle() {
        let store = OrchStore::open_in_memory().unwrap();
        store.create_run(&sample_run("run_1", "t1")).unwrap();
        store
            .create_or_update_task(&sample_task("run_1", "task_a", vec![]))
            .unwrap();
        store
            .create_dispatch(&sample_dispatch(
                "disp_1", "run_1", "task_a", "t1", "t2", 2_000,
            ))
            .unwrap();

        // Non-parent or wrong created_ms rejected
        let err_wrong_term = store
            .record_coordinator_message("disp_1", "t9", 1_000, "hi", true, 0, 2_100)
            .unwrap_err();
        assert!(err_wrong_term
            .to_string()
            .contains("attempt fencing rejected"));

        let err_wrong_ms = store
            .record_coordinator_message("disp_1", "t1", 9_999, "hi", true, 0, 2_150)
            .unwrap_err();
        assert!(err_wrong_ms
            .to_string()
            .contains("attempt fencing rejected"));

        // Immediate directive recorded as acknowledged
        let (msg1, st1) = store
            .record_coordinator_message("disp_1", "t1", 1_000, "Focus on tests", true, 1, 2_200)
            .unwrap();
        assert_eq!(msg1.msg_type, MessageType::Directive);
        assert_eq!(msg1.pointer_state, PointerState::Acknowledged);
        assert_eq!(msg1.target_terminal, "t2");
        assert_eq!(st1, DispatchStatus::Dispatched);

        // Deferred directive recorded as pending
        let (msg2, _) = store
            .record_coordinator_message("disp_1", "t1", 1_000, "Follow up", false, 1, 2_300)
            .unwrap();
        assert_eq!(msg2.pointer_state, PointerState::Pending);
        assert_eq!(store.list_messages_for_run("run_1").unwrap().len(), 2);

        // Open question cancelled when stopped, task restored to Ready without failure_count increment
        let (q, _) = store
            .record_question("disp_1", "t2", 2_000, "Which port?", 2_400)
            .unwrap();
        let (stopped_disp, stopped_task_st) = store.stop_dispatch("disp_1", 2_500).unwrap();
        assert_eq!(stopped_disp.status, DispatchStatus::Stopped);
        assert_eq!(stopped_task_st, TaskStatus::Ready);
        assert_eq!(
            store.get_question_thread(&q.id).unwrap().unwrap().status,
            "cancelled"
        );
        assert_eq!(
            store
                .get_task("run_1", "task_a")
                .unwrap()
                .unwrap()
                .failure_count,
            0
        );

        // Stopped dispatch cannot receive coordinator messages (suggests orch retry)
        let err_stopped = store
            .record_coordinator_message("disp_1", "t1", 1_000, "Too late", true, 1, 2_600)
            .unwrap_err();
        assert!(err_stopped.to_string().contains("orch retry"));

        // Supersede marks stopped dispatch as Superseded and rejects repeat supersede
        let sup = store.supersede_dispatch("disp_1", 2_700).unwrap();
        assert_eq!(sup.status, DispatchStatus::Superseded);
        assert!(store.supersede_dispatch("disp_1", 2_800).is_err());
    }
}
