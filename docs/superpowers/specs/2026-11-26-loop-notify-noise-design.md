# Loop: end-of-run notices only when the coordinator has not read the result

Date: 2026-11-26. Size: small change (`src/orch/store.rs`, `src/orch/delivery.rs`,
`src/loops/cli.rs`). Origin: host report `relatorio-tchelvis-avisos.md` (session of 2026-11-25,
nightly `727b5fc`, 4 loop runs): no orchestration message the coordinator received needed action,
and each one cost a model turn.

## Current behaviour (verified in code)

- When the watcher sees a loop final, `record_loop_final` inserts `msg_loop_<run_id>` (type
  `status`, `pointer_state = pending`) for the caller terminal, in orch run `loop_<run_id>`.
- The daemon sends `[tchelvis] You have N orchestration message(s) in run loop_<id>. Run: tchelvis
  orch check --run loop_<id>` as soon as the caller terminal is safe for a pointer (idle); while
  it is working the pointer waits, with no age limit.
- Only `orch check` acknowledges. `loop status|result|report` do not. `orch check --run X` clears
  only run X, so messages of runs the coordinator handled with `loop result` pile up until a
  general `orch check` (confirmed by the user on the host).
- `pi_check_notes` prints `aviso: pi: a etapa vê todas as credenciais de ~/.pi/agent/auth.json`
  on every `loop check` and every `loop run`.

| Report case | Cause | Fixed by |
| --- | --- | --- |
| 1, 2: notice arrives with the end of the coordinator's background wait | caller idle, pointer sent at once | B (+ A) |
| 3: `ESCALATE` notice 1 h late, after the fix was integrated | pointer waited while caller worked; reading the result did not ack | A, C |
| 4: other runs' handled notices accumulate | `--run` clears one run; `loop result` clears none | A |
| 5: pi credentials warning on every run | by design | D |

## Goals

- **A. Reading the result acknowledges.** `loop status`, `loop result` and `loop report` on a
  run whose `state.json` is final, run from the caller terminal (`HC_TERMINAL_ID` equals
  `loop_runs.caller_terminal`), acknowledge `msg_loop_<run_id>`. If the watcher has not recorded
  the final yet, the command records it first (`record_loop_final` is idempotent and already
  safe against the watcher), so the notice is born acknowledged instead of pending.
- **B. Grace before the loop notice.** A `msg_loop_*` message is not eligible for a pointer
  until `LOOP_NOTIFY_GRACE_MS` (30 s) after `created_at_ms`. A coordinator polling
  `loop status` in the background acks within the grace and never gets the pointer.
- **C. Late notice says so.** When the oldest pending message of the target is older than
  5 min at send time, the pointer line ends with ` (oldest message N min old)` (the line is
  English, like the rest of it).
- **D. pi warning only in `loop check`.** `loop run` keeps the refusal of an unrecognised
  managed install under `bwrap`, but no longer prints the note.

## Non-goals

- No LLM triage (the Supervisor model stays for blocked prompts; these rules are deterministic).
- No `--no-notify` flag, no merging of several runs into one pointer, no "superseded run" rule
  (needs guessing branch relations; A covers the observed case). Revisit after a host session.
- Messages from `orch` dispatches (worker_done, escalation, question) are unchanged.

## Design

- **Store.** `ack_loop_notice(run_id, caller_terminal, now_ms) -> Result<bool>`: one
  transaction, `UPDATE messages SET pointer_state='acknowledged', acknowledged_at_ms=?
  WHERE id='msg_loop_'||run_id AND target_terminal=? AND pointer_state IN
  ('pending','delivered','delivery_stalled')`, then the same `loop_runs.notify_state` update as
  `sync_loop_notify_state`. Terminal identity is the address only (`loop status` must not need
  Herdr); an address reused by another terminal can at worst ack a notice nobody would read.
- **CLI.** A helper called by `cmd_status`, `cmd_result`, `cmd_report` after resolving the run:
  final state + caller match → `record_loop_final` (ignore "not RUNNING") then
  `ack_loop_notice`. Errors are logged at `debug` and never change the command's output or exit
  code. Without `HC_TERMINAL_ID` or without an orch db: no-op.
- **Grace.** `pending_pointer_targets` excludes `pending` rows with `id LIKE 'msg_loop_%'` and
  `created_at_ms > now_ms - LOOP_NOTIFY_GRACE_MS`. The target becomes eligible on a later tick
  (the daemon already ticks every 5 s).
- **Age.** `PendingPointerTarget` gains `oldest_pending_created_ms` (`MIN(created_at_ms)` of
  pending rows). `format_pointer_line(run_id, count, age_ms: Option<i64>)`; age shown only
  `>= 5 min`, in whole minutes.
- **pi.** `cmd_run` calls `pi_check_notes(..)?` for the refusal and discards the notes.

## Acceptance

1. Store: `ack_loop_notice` acks a pending/delivered/stalled loop message for the matching
   terminal, does nothing for another terminal, and moves `notify_state` to `acknowledged`.
2. CLI (temp HOME, in-memory/temp orch db): `loop result` from the caller terminal on a final
   run with the watcher not yet run leaves `msg_loop_<id>` acknowledged and a later
   `watch_loops` sends nothing; from another terminal it stays pending.
3. Store: a loop message 10 s old is not in `pending_pointer_targets`; at 31 s it is; a
   non-loop message 1 s old is.
4. `format_pointer_line` with age `None`, 4 min and 58 min.
5. `loop run` with a pi stage prints no credentials note; `loop check` still does; the
   unrecognised-install refusal under `bwrap` still fails `loop run`.
6. Host: a coordinator that waits with `loop status` in the background gets no
   `[tchelvis]` line; one busy during the end gets no line if it ran `loop result` first.
