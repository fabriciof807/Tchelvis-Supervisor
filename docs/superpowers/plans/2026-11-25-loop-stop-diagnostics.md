# Loop Stop Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** When the engine ends a stage (`STALLED`/`TIMEOUT`), record what the stage was doing, so a hung model can be told from a long silent tool.

**Architecture:** `run_process` gains a diagnosed variant that snapshots the process tree before ending it. `EventParser` remembers the last event and the open tools. The engine joins both into an additive `Execution.stop`, appends a hint to the reason, and `report.md` renders it.

**Tech Stack:** Rust, `libc`, `serde`. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-25-loop-stop-diagnostics-design.md` (decides behaviour)

## Global Constraints

- Never store a process command line, argument, environment or path; only `comm`. Every stored string goes through the engine's existing sanitising/redaction (`sanitize_text`/`sanitize_reason` with `extra_secrets`), bounded to 64 chars per name.
- At most 20 processes and 10 tools stored; `processes_total` keeps the real count.
- `state.json` stays readable both ways: new fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- Signature of `run_process` does not change. Existing tests stay unchanged.
- Failing to collect the diagnostic never changes a stage outcome.
- User-visible text in Portuguese, exact strings from the spec. Code, docs, CHANGELOG in English.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`.

## Review Focus

- Process start must be compared in boot-time ticks read by `clock_gettime(CLOCK_BOOTTIME)`, not `/proc/uptime` (lxcfs fakes it inside Incus). Task 2.
- A shell left over from an earlier tool (started before the last output) must not count as a running tool. Task 2.
- Two tools open in parallel with the same name, and one finishing: the other must remain. Task 1.
- A `comm` with spaces, `)` or control characters must not break parsing or the report. Tasks 2 and 3.
- A tool id closed but never opened, or events without ids, must not panic or leave a wrong open tool. Task 1.

---

### Task 1: `EventParser` remembers the last event and open tools

**Files:** Modify and test `src/loops/adapters.rs`.

**Produces:**
- `pub struct StreamState { pub last_event: Option<String>, pub running_tools: Option<Vec<String>>, pub usage: Usage, pub error: Option<String> }`
- `EventParser::finish_interrupted(self) -> StreamState`

Rules:
- `last_event`: `type` (`event` for `agy`) of the last JSON object line. Append ` (<tool>)` when known: opencode `tool_use` → `part.tool`; pi `tool_execution_*` → `toolName`; agy `step_update` → `step_type`, plus `tool_name` when present, as `step_update tool (read_file)`; claude `assistant` with a `tool_use` block → its `name`.
- `running_tools`: opencode always `None`. Others `Some`, keyed by id, in start order. pi: `tool_execution_start`/`_end` by `toolCallId`. claude: `tool_use` block `id` in `assistant.message.content`, closed by `tool_result` `tool_use_id` in `user.message.content`. agy: `step_type == "tool"` with `state == "ACTIVE"` opens `step_index`; any other state closes it.
- `error`: only explicit stream errors (claude/agy/opencode `self.error`; pi `auto_retry_end` failure, or last `stopReason` `error`/`aborted`). Never the "terminou sem evento" messages. `usage` is the same as `finish()`; share the code, do not copy it.

- [ ] Failing tests:
  - `interrupted_pi_reports_the_open_tool`: start `c1` bash, update `c1` → `last_event == Some("tool_execution_update (bash)")`, `running_tools == Some(vec!["bash"])`, `error == None`; after `tool_execution_end` `c1` → `Some(vec![])`.
  - `interrupted_claude_and_agy_track_open_tools`: cut `claude_ok.jsonl` after line 2 → `Some(["Read"])`, after line 3 → `Some([])`; cut `agy_ok.jsonl` after line 4 → `Some(["read_file"])`, after line 5 → `Some([])`.
  - `interrupted_opencode_does_not_know_tools`: cut `opencode_ok.jsonl` after line 1 → `last_event == Some("step_start")`, `running_tools == None`; after line 2 → `Some("tool_use (read)")`.
  - `parallel_tools_with_one_name_close_by_id`: two pi starts `a`,`b` bash, end `a` → `Some(["bash"])`; an end for unknown `z` changes nothing; a start without id is ignored.
  - `interrupted_keeps_explicit_errors`: pi `auto_retry_end` `success:false finalError:"x"` → `error == Some("x")`.
- [ ] Implement; all tests pass. Ready to commit.

### Task 2: process snapshot before the tree is ended

**Files:** Modify and test `src/loops/process.rs`.

**Produces:**
- `pub struct StopProcess { pub name: String, pub age_ms: u64, pub cpu_ms: u64, pub after_last_output: bool }`
- `pub struct StopSnapshot { pub silent_ms: u64, pub last_output_at_ms: Option<i64>, pub cli_alive: bool, pub processes: Vec<StopProcess>, pub processes_total: usize }`
- `pub fn run_process_diagnosed(<same parameters as run_process>, stop: &mut Option<StopSnapshot>) -> Result<Outcome>`; `run_process` calls it and drops the snapshot.

Rules: filled only for `Stalled` and `TimedOut`, inside the watch path and before `terminate_process_tree`. `silent_ms` = clock time since the last `stdout` byte (stage start when none); `last_output_at_ms` = `clock.now_ms()` then. `after_last_output` = `start_ticks` greater than the boot-time ticks recorded at the last `stdout` byte; `false` for every process when there was no output. Processes come from `collect_tree_pids_marked` and are ordered `after_last_output` first, then `cpu_ms` descending, then pid. `cpu_ms` = utime + stime (stat fields 14 and 15), ticks from `sysconf(_SC_CLK_TCK)`. `cli_alive` = the root pid with the same start ticks is alive and not a zombie. `comm` is taken between the first `(` and the last `)`.

- [ ] Failing tests (executables through `crate::test_support::write_executable`, `SystemClock`, stall 1500 ms):
  - `stall_snapshot_separates_processes_by_last_output`: `main.sh` starts `./early_tool.sh &`, waits for its ready file, prints one line, waits 0.2 s, starts `./late_tool.sh &`, then `wait`. Assert `Outcome::Stalled`, `cli_alive`, `late_tool.sh` `after_last_output == true`, `early_tool.sh` `false`, `silent_ms >= 1500`, `last_output_at_ms.is_some()`.
  - `stall_snapshot_without_output_marks_no_process`: no output → `last_output_at_ms == None`, every `after_last_output == false`.
  - `normal_exit_and_cancel_leave_no_snapshot`.
  - `stat_with_odd_names_is_parsed`: unit test of the pure stat parser with `7 (a) b (x)) S 1 …`.
- [ ] Extract the stat line parser as a pure function. Implement; all tests pass. Ready to commit.

### Task 3: `Execution.stop`, hint and report

**Files:** Modify and test `src/loops/state.rs`, `src/loops/engine.rs`, `src/loops/report.rs`; `CHANGELOG.md`; the paragraph on stage limits in `README.md` only if one exists.

**Produces:** in `state.rs`:
```rust
pub struct StopProcess { pub name: String, pub age_ms: u64, pub cpu_ms: u64, pub after_last_output: bool }
pub struct StopDiagnostic {
    pub last_event: Option<String>, pub last_output_at_ms: Option<i64>, pub silent_ms: u64, pub cli_alive: bool,
    pub running_tools: Option<Vec<String>>, pub processes: Vec<StopProcess>, pub processes_total: usize,
}
impl StopDiagnostic { pub fn hint(&self) -> String }
```
`Execution.stop: Option<StopDiagnostic>` and `StageResult.stop: Option<StopDiagnostic>`. Use the `state.rs` `StopProcess` in `process.rs` too if that avoids a duplicate type; one definition only (`StopSnapshot` stays in `process.rs`).

Rules: the reason becomes `etapa travada sem atividade (STALLED): <hint>` and `tempo limite da etapa atingido (STAGE_TIMEOUT): <hint>`; `MAX_DURATION` gets `: <hint>` only when a stage was cut. With no `stop`, the current texts stay exactly. In `hint()`, "nenhum evento" means `last_event == None`; list at most 3 names, de-duplicated. `ProcessStageRunner` uses `finish_interrupted` for `Stalled`, `TimedOut` and `Cancelled` (`Cancelled` has no `stop`), `finish` otherwise.
`report.md` section `## Etapas interrompidas`, after the executions table, one item per `stop`: stage, outcome, last event and its age, CLI alive, running tools (`o CLI não informa` for `None`, `nenhuma` for empty), hint, and the processes as `nome (idade, CPU, depois da última saída)`.

- [ ] Failing tests:
  - `hint_follows_the_spec_order`: five cases with exact strings from the spec (tool open; new process; no event; nothing; tool open and new process → tool wins).
  - `execution_stop_round_trips_and_old_state_loads`: state without `stop` loads; `stop: None` is not serialised.
  - `stalled_stage_records_stop_and_hint` (engine, `ScriptedRunner`): `state.reason` ends with the hint and contains `STALLED`; `executions[0].stop.is_some()`; a secret from `extra_secrets` in a tool name and in a process name does not reach `state.json` or `report.md`.
  - `interrupted_pi_stage_hides_the_missing_settled_error` (`ProcessStageRunner` with a fake `pi` that prints `tool_execution_start` and sleeps; follow `pi_bad_model_with_exit_zero_fails_stage`): outcome `Stalled`, `error == None`, `running_tools == Some(["bash"])`.
  - `report_lists_interrupted_stages`: contains `## Etapas interrompidas` with the fields; absent without `stop`; a name with `|`, backtick or newline does not break the Markdown.
- [ ] Implement, CHANGELOG entry under `[Unreleased]` → `### Added`. Whole-workspace fmt, clippy, tests. Ready to commit.
