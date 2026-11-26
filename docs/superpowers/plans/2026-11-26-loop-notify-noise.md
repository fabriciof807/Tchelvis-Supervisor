# Loop end-of-run notices only when unread — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax. Do the tasks in order, one at a time.

**Goal:** The coordinator gets a `[tchelvis]` pointer for a finished loop only when it has not already read the result; late pointers say how late they are; `loop run` stops printing the pi credentials note.

**Architecture:** A store method acknowledges a loop's notice for its caller terminal (A). `loop status|result|report` call it when the run is final and they run in the caller terminal, recording the final first if the watcher has not. `pending_pointer_targets` holds a loop notice back for 30 s (B) and reports the age of the oldest pending message, which the pointer line shows past 5 min (C). `cmd_run` keeps the pi refusal but drops the note (D).

**Tech Stack:** Rust 2021, `rusqlite`, `tracing`. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-26-loop-notify-noise-design.md` (decides behaviour; read it first).

## Global Constraints

- `pub const LOOP_NOTIFY_GRACE_MS: i64 = 30_000;` (in `src/orch/store.rs`). Pointer age shown only when `>= 300_000` ms, in whole minutes (`age_ms / 60_000`).
- Pointer suffix, exact, English: ` (oldest message {N} min old)`, appended to today's line; with no suffix the line is byte-identical to today.
- A loop notice is a message whose id starts with `msg_loop_`. In SQL use `substr(id, 1, 9) = 'msg_loop_'` (not `LIKE`: `_` is a wildcard there).
- Terminal identity is the address only: `HC_TERMINAL_ID` (trimmed, non-empty) compared to `loop_runs.caller_terminal`. `loop status|result|report` never call Herdr.
- The ack on read never changes a command's stdout, stderr or exit code. Errors → `tracing::debug!`. No `HC_TERMINAL_ID`, or `orch.db` does not exist → no-op; never create `orch.db` from these commands.
- Only `msg_loop_*` notices are affected. `orch` dispatch messages (worker_done, escalation, question) keep today's timing and text apart from the age suffix.
- `loop check` output unchanged. `loop run` still fails on an unrecognised managed pi install under `bwrap`.
- Code, comments, CHANGELOG in English. User-visible loop CLI text stays Portuguese (unchanged).
- Existing tests keep their assertions. The only edits allowed in them: the new `None` third argument of `format_pointer_line`, a new field in a `PendingPointerTarget` literal.
- Tests: no network, no `sleep` as synchronisation; time is passed as `now_ms`.
- Do not commit. At the end of each task run its tests, then `cargo fmt` and `cargo clippy --all-targets -- -D warnings`; a task is done only when all pass.

## Stop and report (do not improvise) when

- an existing test fails because a loop notice is now inside the grace window (it means a behaviour the spec did not foresee);
- a name, signature or line in this plan does not exist in the code as described;
- the same command fails three times for the same cause.

Before Task 1 run `cargo test --locked` once. If it fails on the untouched tree, report it and stop.

## Review Focus

- A notice whose pointer was already sent (`delivered`) or gave up (`delivery_stalled`, loop `undelivered`) is still acknowledged by reading the result, and the loop's `notify_state` follows. Task 1.
- `loop status` polled while the run is still RUNNING acknowledges nothing and creates no message; the first poll that sees the final does. Task 3.
- A terminal that is not the caller (another coordinator, a user shell) reading the result leaves the notice pending for the real caller. Task 3.
- `orch status` counts unacknowledged messages through `pending_pointer_targets` (`src/orch/cli.rs:1204`): a loop notice younger than 30 s is not counted there. Accepted side effect of B; pinned by the grace test so a change is deliberate. Task 2.
- A run that ended without a caller (`notify_state = 'none'`, no message): reading it is a no-op and does not fail. Task 3.

---

### Task 1: `OrchStore::ack_loop_notice`

**Files:** Modify `src/orch/store.rs`. Test in `src/loops/watch.rs` tests (where loop store behaviour is tested; reuse `sample_loop_rec`).

**Produces:**
```rust
impl OrchStore {
    /// Acknowledges the end notice of a loop for its caller terminal.
    /// Returns true when the message changed.
    pub fn ack_loop_notice(&self, run_id: &str, caller_terminal: &str, now_ms: i64) -> Result<bool>
}
```
One `Immediate` transaction:
`UPDATE messages SET pointer_state = 'acknowledged', acknowledged_at_ms = ?3 WHERE id = 'msg_loop_' || ?1 AND target_terminal = ?2 AND pointer_state IN ('pending', 'delivered', 'delivery_stalled')`;
when it changed a row, `UPDATE loop_runs SET notify_state = 'acknowledged', updated_at_ms = ?2 WHERE run_id = ?1 AND notify_state != 'acknowledged'`.

- [ ] **Step 1: failing tests** in `watch.rs` tests. Setup for each: `register_loop(&sample_loop_rec(id, dir, 42, 999, Some(("t1", 1_000))))`, then `record_loop_final(id, "PASS", 2_000)`.

```rust
#[test]
fn ack_loop_notice_acks_pending_delivered_and_stalled_for_the_caller_only() {
    // "a-pending": ack_loop_notice("a-pending", "t9", 3_000) == false, message still Pending,
    //              notify_state "pending"; then ack_loop_notice("a-pending", "t1", 3_000) == true,
    //              message pointer_state Acknowledged (list_messages_for_run("loop_a-pending")),
    //              get_loop(..).notify_state == "acknowledged"; a second call returns false.
    // "a-delivered": mark_pointer_delivered("t1", "loop_a-delivered", 2_500) then ack → true, Acknowledged.
    // "a-stalled":  mark_loop_undelivered("a-stalled", 2_500) then ack → true, Acknowledged,
    //               notify_state "acknowledged".
    // Afterwards pending_pointer_targets(10_000_000) has no target in any of these runs.
}

#[test]
fn ack_loop_notice_without_a_message_is_a_no_op() {
    // registered with caller None and record_loop_final → no message; ack_loop_notice(.., "t1", ..) == Ok(false)
    // unknown run id → Ok(false)
}
```

- [ ] **Step 2: run, expect FAIL:** `cargo test --locked --lib loops::watch::tests::ack_loop_notice`
- [ ] **Step 3: implement** `ack_loop_notice` next to `sync_loop_notify_state`.
- [ ] **Step 4: run, expect PASS:** `cargo test --locked --lib loops::watch orch::`, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`.

---

### Task 2: Grace before a loop pointer, and the age of a late pointer

**Files:** Modify `src/orch/mod.rs` (`PendingPointerTarget`), `src/orch/store.rs` (`pending_pointer_targets`, constant), `src/orch/delivery.rs` (`format_pointer_line`, its call at ~line 256, the wrapper at ~line 420). Tests in `src/loops/watch.rs` and `src/orch/delivery.rs`.

**Produces:**
```rust
pub const LOOP_NOTIFY_GRACE_MS: i64 = 30_000;          // store.rs
pub struct PendingPointerTarget { /* existing fields */ pub oldest_pending_created_ms: i64 }
pub fn format_pointer_line(run_id: &str, count: usize, age_ms: Option<i64>) -> String  // delivery.rs
```
- Query: a `pending` row counts (in `pending_cnt` and in the new `MIN(CASE .. THEN created_at_ms END)` column) only if it is not a loop notice with `created_at_ms > now_ms - LOOP_NOTIFY_GRACE_MS`. Bind the cutoff as a parameter. `HAVING pending_cnt > 0`, `active_delivered_cnt` and `ORDER BY` stay as they are.
- Delivery passes `Some(now_ms - target.oldest_pending_created_ms)`. The `DeliveryScheduler::format_pointer_line` wrapper takes and forwards the same three arguments.

- [ ] **Step 1: failing tests.**

In `watch.rs` tests:
```rust
#[test]
fn loop_notice_waits_out_the_grace_but_other_messages_do_not() {
    // register "g1" with caller ("t1", 1_000); record_loop_final("g1", "PASS", 100_000)
    // pending_pointer_targets(110_000): no target with run_id "loop_g1"
    // pending_pointer_targets(131_000): exactly one, run_id "loop_g1", pending_count 1,
    //                                   oldest_pending_created_ms == 100_000
    // A non-loop message for t1 created at 130_000 (create it the way the store test
    // `pending pointer target for t1` near store.rs:2746 does, via record_question on a dispatch)
    // is listed by pending_pointer_targets(131_000) with oldest_pending_created_ms == 130_000.
}
```
In `delivery.rs` tests:
```rust
#[test]
fn pointer_line_shows_the_age_only_when_late() {
    let base = "[tchelvis] You have 2 orchestration message(s) in run run_abc. Run: tchelvis orch check --run run_abc";
    assert_eq!(format_pointer_line("run_abc", 2, None), base);
    assert_eq!(format_pointer_line("run_abc", 2, Some(4 * 60_000)), base);
    assert_eq!(format_pointer_line("run_abc", 2, Some(5 * 60_000)), format!("{base} (oldest message 5 min old)"));
    assert_eq!(format_pointer_line("run_abc", 2, Some(58 * 60_000 + 59_000)), format!("{base} (oldest message 58 min old)"));
}
```
`test_format_pointer_line_canonical` gets the `None` argument only.

- [ ] **Step 2: run, expect FAIL:** `cargo test --locked --lib loops::watch orch::delivery`
- [ ] **Step 3: implement** as described under **Produces**.
- [ ] **Step 4: run, expect PASS:** `cargo test --locked` (whole suite: daemon and orch integration tests exercise pointers), `cargo fmt`, `cargo clippy --all-targets -- -D warnings`.

---

### Task 3: Reading a final result from the caller acknowledges its notice

**Files:** Modify and unit-test `src/loops/cli.rs`. Integration test in `tests/loop_integration.rs`.

**Consumes:** `OrchStore::ack_loop_notice` (Task 1), `OrchStore::record_loop_final` (returns `Ok(None)` when the row is no longer RUNNING), `OrchStore::get_loop`, `RunStatus::is_final`.

**Produces (private to `cli.rs`):**
```rust
/// Core, testable with a temp store.
fn ack_read_notice(store: &OrchStore, run_id: &str, status: RunStatus, terminal: &str, now_ms: i64) -> anyhow::Result<bool>
/// Env + db wrapper used by the commands; never fails.
fn ack_notice_on_read(paths: &CockpitPaths, state: &RunState)
```
- `ack_read_notice`: not final → `Ok(false)`; no `loop_runs` record, or `caller_terminal != Some(terminal)` → `Ok(false)`; else `record_loop_final(run_id, status.as_str(), now_ms)?` then `ack_loop_notice(run_id, terminal, now_ms)`.
- `ack_notice_on_read`: terminal from `HC_TERMINAL_ID` (trimmed, non-empty) else return; `paths.orch_db_file()` must exist else return; `OrchStore::open`, call `ack_read_notice(.., &state.run_id, state.status, .., unix_now_ms())`; any error → `tracing::debug!(run_id = %state.run_id, error = %e, "loop notice ack on read failed")`.
- Call sites: `cmd_status` and `cmd_result` in their `Ok(state)` branch, after printing. `cmd_report`: after printing, `if let Ok(state) = load_state(&run_dir) { ack_notice_on_read(paths, &state) }` in the `report.md` branch, and with the already-loaded state in the render branch. The branches without `state.json` do nothing new.

- [ ] **Step 1: failing unit tests** in `cli.rs` tests (temp dir, `OrchStore::open(tmp/orch.db)`, a `LoopRunRecord` literal with `status "RUNNING"`, `caller_terminal Some("t1")`, `caller_created_ms Some(1_000)`, `notify_state "pending"`):

```rust
#[test]
fn reading_a_final_run_from_the_caller_records_and_acks_the_notice() {
    // ack_read_notice(&store, "r1", RunStatus::Pass, "t1", 5_000) == true
    // get_loop("r1"): status "PASS", notify_state "acknowledged"
    // list_messages_for_run("loop_r1"): one message, pointer_state Acknowledged
    // pending_pointer_targets(10_000_000) empty  (a later watcher pass sends nothing:
    //   record_loop_final("r1", "PASS", 6_000) returns Ok(None))
}

#[test]
fn reading_from_another_terminal_or_while_running_changes_nothing() {
    // ack_read_notice(.., "r2", RunStatus::Pass, "t9", ..) == false; list_messages_for_run("loop_r2") empty;
    //   then record_loop_final("r2", "PASS", 6_000) (the watcher) → message Pending,
    //   and pending_pointer_targets(6_000 + LOOP_NOTIFY_GRACE_MS + 1) lists "loop_r2"
    // ack_read_notice(.., "r3", RunStatus::Running, "t1", ..) == false; get_loop("r3").status "RUNNING"
    // ack_read_notice(.., "unknown", RunStatus::Pass, "t1", ..) == false
}

#[test]
fn reading_after_the_watcher_still_acks() {
    // record_loop_final("r4", "ESCALATE", 2_000), mark_pointer_delivered("t1", "loop_r4", 2_500);
    // ack_read_notice(.., "r4", RunStatus::Escalate, "t1", 3_000) == true; message Acknowledged
}
```

- [ ] **Step 2: failing integration test** in `tests/loop_integration.rs`:

```rust
#[test]
fn reading_the_result_from_the_caller_terminal_acknowledges_the_end_notice() {
    // h = LoopHarness::new(); for (id, verb) in [("s1","status"), ("r1","result"), ("p1","report")]:
    //   dir = h.repo.join(".loops/runs").join(id); write state.json as the minimal legacy JSON
    //   (shape of the legacy test in src/loops/state.rs) with "run_id": id, "status": "PASS";
    //   for "p1" also write dir/report.md ("# relatório\n");
    //   register a LoopRunRecord (fields as `register`, but status "RUNNING",
    //   caller_terminal Some("t1"), caller_created_ms Some(1_000), notify_state "pending").
    //   h.cmd().env("HC_TERMINAL_ID", "t1").args(["loop", verb, id]) → exit 0.
    //   store.list_messages_for_run(&format!("loop_{id}")): one message, Acknowledged;
    //   store.get_loop(id).notify_state == "acknowledged".
    // Same setup "x1", `loop result x1` with HC_TERMINAL_ID "t9": exit 0, stdout equal to the
    //   output of the same command without HC_TERMINAL_ID, and no message for "loop_x1".
}
```

- [ ] **Step 3: run, expect FAIL:** `cargo test --locked --lib loops::cli::tests::reading` and `cargo test --locked --test loop_integration reading_the_result`
- [ ] **Step 4: implement** as described under **Produces**.
- [ ] **Step 5: run, expect PASS:** `cargo test --locked --lib loops::cli`, `cargo test --locked --test loop_integration`, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`.

---

### Task 4: The pi credentials note only in `loop check`

**Files:** Modify and unit-test `src/loops/cli.rs`; `CHANGELOG.md`.

**Produces:** `fn pi_run_gate(workflow: &Workflow, install: &Result<PiInstall, String>, sandbox_kind: &str) -> Result<(), String>` = `pi_check_notes(..).map(|_| ())`. `cmd_run` (~line 2971) replaces the `for note in pi_check_notes(..)? { eprint_redacted(..) }` loop with `pi_run_gate(&resolution.workflow, &pi_install, sandbox.kind()).map_err(LoopError::err)?;`. `cmd_check` (~line 2542) unchanged. (There is no fake `pi` in the integration harness, so the gate is unit-tested and the print removal is checked by review.)

- [ ] **Step 1: failing test** in `cli.rs` tests, next to `pi_check_notes_requires_the_managed_install_only_under_bwrap` (reuse its stage/workflow and install values):

```rust
#[test]
fn pi_run_gate_refuses_like_check_but_carries_no_note() {
    // recognised install, "bwrap": Ok(())
    // unrecognised install, "bwrap": Err equal to pi_check_notes(..).unwrap_err()
    // unrecognised install, "inherited" and "none": Ok(())
    // pi_check_notes(.., recognised, "bwrap") still returns the credentials note (unchanged)
}
```
- [ ] **Step 2: run, expect FAIL:** `cargo test --locked --lib loops::cli::tests::pi_run_gate`
- [ ] **Step 3: implement** as under **Produces**. `grep -n "credenciais" src/loops/cli.rs` must show the note only in `pi_check_notes` and its tests.
- [ ] **Step 4: CHANGELOG** — under `## [Unreleased]` → `### Changed`:
  - `- \`loop status\`, \`loop result\` and \`loop report\` run from the caller terminal acknowledge a finished loop's end notice, so no pointer follows a result already read.`
  - `- A loop's end notice waits 30 s before its pointer is sent; a pointer whose oldest message is 5 min or older says how old it is.`
  - `- \`loop run\` no longer prints the pi credentials note (\`loop check\` still does).`
- [ ] **Step 5: run, expect PASS:** `cargo test --locked`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`.

---

## After the tasks (coordinator)

- Host check (spec acceptance 6): a coordinator that waits with `loop status` in the background gets no `[tchelvis]` line; one busy at the end gets none if it ran `loop result` first; `loop run` with a pi stage prints no credentials note.
