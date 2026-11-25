# Loop `tool_stall_secs` (Delivery A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax. Do the tasks in order, one at a time.

**Goal:** A silent stage with an open tool lives up to `tool_stall_secs` instead of `stall_secs`, for `claude`, `agy` and `pi`.

**Architecture:** A pure `StallTracker` in `process.rs` decides when a silent stage is stalled. `EventParser::tool_view()` tells it whether a tool is open. The engine connects both, and records the applied limit, the signal and the tolerated silence in `state.json` and `report.md`.

**Tech Stack:** Rust 2021, `serde`. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-25-loop-tool-stall-secs-design.md` (decides behaviour; read it first). Only delivery **A**.

## Global Constraints

- Out of scope (delivery B): `tool_candidates`, process scanning as a signal, step boundary ticks. `opencode` returns `ToolView::Unseen`, and the monitor treats it exactly as "no tool".
- `run_process` and `Limits` do not change. `stall_secs`, `stage_timeout_secs`, `max_duration_secs` defaults do not change.
- No stage ends earlier than today. Timeout and cancel are tested before the stall verdict.
- Default: `max(1200, stall_secs)`. Present and equal to `stall_secs`: the feature is off (no `tool_stall` limit, no `tool_signal`, no `tool_silence`).
- New state fields are optional: `#[serde(default, skip_serializing_if = "Option::is_none")]`. `LOOP_STATE_SCHEMA_VERSION` stays 1.
- Names only, through `sanitize_stop`: at most 10 (`MAX_STOP_TOOLS`), 64 chars each. Never a command line or path.
- User-visible text in Portuguese, exact strings below. Code, comments, CHANGELOG in English.
- Existing tests keep their assertions. The only edits allowed in them: a new field in a struct literal, or the new `None` argument of `run_process_diagnosed`.
- Tests: no network, no `sleep` in Rust code as synchronisation.
- Do not commit. At the end of each task run its tests, then `cargo fmt` and `cargo clippy --all-targets -- -D warnings`; a task is done only when all pass.

## Stop and report (do not improvise) when

- an existing test fails for a reason other than a missing field or argument;
- a name, signature or line in this plan does not exist in the code as described;
- the borrow checker rejects the wiring of Task 5 or 6 after two attempts;
- a new end-to-end test passes only sometimes, or needs a longer timeout or a Rust `sleep` to pass;
- the same command fails three times for the same cause.

Before Task 1 run `cargo test --locked` once. If it fails on the untouched tree, report it and stop.

## Review Focus

- The end event of a tool is lost (an output line over 1 MiB is cut and ignored): the tool stays "open", but the ceiling still ends the stage. Tasks 4 and 5.
- Several long tools in sequence: each event restarts both limits, so each tool gets the whole ceiling. Task 3.
- An existing workflow with `stall_secs = 3000` and no new key must load, and an invalid `stall_secs` must not add a second, misleading error. Task 1.
- `opencode` behaves exactly as today. Task 6.
- A stage that ends `ERROR`, `CANCELLED` or `TIMEOUT` after a tolerated silence still records `tool_silence`. Tasks 5 and 6.

---

### Task 1: `tool_stall_secs` in the workflow

**Files:** Modify and test `src/loops/workflow.rs`, `src/loops/cli.rs`. Modify `src/loops/models.rs` (literal in the resolver), `src/loops/preflight.rs` (test literal), the 4 files in `assets/loops/workflows/`.

**Produces:** `pub const DEFAULT_TOOL_STALL_SECS: u64 = 1200;` and `pub tool_stall_secs: u64` right after `stall_secs` in `UnresolvedWorkflow` and `Workflow`.

Rules (in `parse_workflow_inner`, right after `stall_secs` is read):
- Add `"tool_stall_secs"` to `WORKFLOW_KEYS` after `"stall_secs"`.
- `read_positive_u64(table, "tool_stall_secs", DEFAULT_TOOL_STALL_SECS.max(stall_secs), LIMIT_SECS, &mut errors)`.
- `read_positive_u64` returns 0 after an error. Only when both values are greater than 0 and `tool_stall_secs < stall_secs`, push exactly: `limite 'tool_stall_secs' ({tool_stall_secs}) não pode ser menor que 'stall_secs' ({stall_secs})`.
- Copy the field in `parse_workflow` and in `models.rs`. Test literals (`preflight.rs`, two in `cli.rs`) get `tool_stall_secs: 1200,`.
- Presets: line `tool_stall_secs = 1200` right after `stall_secs = 600`. `cli.rs`: one `writeln!` with `DEFAULT_TOOL_STALL_SECS` after the `stall_secs` one; import the constant.

- [ ] **Step 1: failing tests** in `workflow.rs` (use the existing `two_stage`, `GATE_OK`, `errors`):

```rust
fn with_limits(limits: &str) -> String {
    two_stage("w", GATE_OK).replacen("name", &format!("{limits}\nname"), 1)
}

#[test]
fn tool_stall_default_follows_stall_secs() {
    for (limits, expected) in [
        ("", 1200),
        ("stall_secs = 300", 1200),
        ("stall_secs = 1200", 1200),
        ("stall_secs = 3000", 3000),
        ("stall_secs = 600\ntool_stall_secs = 900", 900),
        ("stall_secs = 600\ntool_stall_secs = 600", 600),
        ("tool_stall_secs = 604800", 604800),
        ("stage_timeout_secs = 60\ntool_stall_secs = 1200", 1200),
    ] {
        let wf = parse_workflow(&with_limits(limits), &RoleDefaults::new())
            .unwrap_or_else(|e| panic!("{limits}: {e:?}"));
        assert_eq!(wf.tool_stall_secs, expected, "{limits}");
    }
}

#[test]
fn tool_stall_invalid_values_are_rejected() {
    for limits in [
        "tool_stall_secs = 0",
        "tool_stall_secs = -1",
        "tool_stall_secs = \"1200\"",
        "tool_stall_secs = 604801",
    ] {
        let text = errors(&with_limits(limits));
        assert!(text.contains("tool_stall_secs"), "{limits}: {text}");
        assert!(!text.contains("menor que 'stall_secs'"), "{limits}: {text}");
    }
    let text = errors(&with_limits("stall_secs = 600\ntool_stall_secs = 599"));
    assert!(text.contains("limite 'tool_stall_secs' (599) não pode ser menor que 'stall_secs' (600)"), "{text}");
    let text = errors(&with_limits("stall_secs = 0\ntool_stall_secs = 900"));
    assert!(!text.contains("tool_stall_secs"), "{text}");
}
```

  In `presets_validate_with_defaults` add inside the loop `assert_eq!(wf.tool_stall_secs, 1200);` and `assert!(raw.contains("tool_stall_secs = 1200"));`. In `cli.rs`, `loop_new_generates_preset_shapes_repeated_roles_and_unknown_roles_that_validate`, add inside the loop `assert!(toml.contains("tool_stall_secs = 1200"));` and `assert_eq!(res.workflow.tool_stall_secs, 1200);`.
- [ ] **Step 2:** `cargo test --locked --lib tool_stall` → does not compile (no field).
- [ ] **Step 3:** implement.
- [ ] **Step 4:** `cargo test --locked --lib loops::` → all pass. Ready to commit.

### Task 2: state types and hint

**Files:** Modify and test `src/loops/state.rs`. Modify `src/loops/engine.rs` (`StageResult`, `sanitize_stop`, literals), `src/loops/report.rs` (test literals).

**Produces** in `state.rs`, before `StopDiagnostic`:

```rust
/// Which silence limit ended a stalled stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopLimit { Stall, ToolStall }

/// How an open tool was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource { Events, Processes }

impl ToolSource {
    /// Text of the report: "eventos do CLI" or "processos do passo atual".
    pub fn label_pt(self) -> &'static str;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSignal {
    pub source: ToolSource,
    #[serde(default)]
    pub names: Vec<String>,
}

/// The longest silence past `stall_secs` that an open tool made acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSilence { pub longest_ms: u64, pub source: ToolSource }
```

- `StopDiagnostic` gains, at the end: `limit: Option<StopLimit>`, `limit_secs: Option<u64>` (only with `ToolStall`; the report needs the value), `tool_signal: Option<ToolSignal>`.
- `Execution` and `engine::StageResult` gain, after `stop`: `tool_silence: Option<ToolSilence>`.
- `hint()`: after the `running_tools` case and before the `after_last_output` case, when `tool_signal` has source `Processes` and at least one non-blank name: `possível ferramenta em execução: <nomes> (processos iniciados no passo atual)` (names through `unique_names`).
- `sanitize_stop` keeps `limit` and `limit_secs`, and filters `tool_signal.names` the same way as `running_tools` (take `MAX_STOP_TOOLS`, `text`, drop empty). The signal itself stays even with no name left.

Literals (mechanical): run
`sed -i -E 's/^( *)stop: None,$/&\n\1tool_silence: None,/' src/loops/engine.rs src/loops/state.rs src/loops/report.rs`, then fix by hand what the compiler lists: the two `Execution` literals of `engine.rs` get `tool_silence: result.tool_silence,`; the `StopDiagnostic` literals (`engine.rs` runner, `engine.rs` test, `state.rs` `wedged_stop`, `report.rs` test) get the three new fields as `None`; literals with `stop: Some(...)` get `tool_silence: None,`. Task 6 fills the real values.

- [ ] **Step 1: failing tests** in `state.rs`:
  - `hint_names_processes_of_the_current_step`: `wedged_stop()` with `running_tools = None` and `tool_signal = Some(ToolSignal { source: ToolSource::Processes, names: vec!["run_tests.sh".into(), " ".into(), "run_tests.sh".into()] })` → `"possível ferramenta em execução: run_tests.sh (processos iniciados no passo atual)"`; with `running_tools = Some(vec!["bash".into()])` → `"ferramenta em execução: bash"`; with names `[]` → the old `late_tool.sh (processos iniciados depois da última saída)` text.
  - `tool_stall_fields_round_trip_and_are_absent_when_unset`: a state whose only execution has `stop` with `limit: Some(ToolStall)`, `limit_secs: Some(1200)`, `tool_signal` (`Events`, `["bash"]`) and `tool_silence: Some(ToolSilence { longest_ms: 700_000, source: Events })`: JSON contains `"limit":"tool_stall"`, `"limit_secs":1200`, `"source":"events"`, `"longest_ms":700000`, and `parse_state` returns the same state. With all of them `None`: JSON contains none of `"limit"`, `"limit_secs"`, `"tool_signal"`, `"tool_silence"`, and parses back equal.
- [ ] **Step 2: failing test** in `engine.rs`, `sanitize_stop_cleans_the_tool_signal`: call `sanitize_stop` with 12 names, one of them containing `sk-signal-secret-1` (passed as extra secret) and one being `"\n"` → at most 10 names, none contains the secret, none is empty, `limit`, `limit_secs` and `source` unchanged.
- [ ] **Step 3:** `cargo test --locked --lib tool_stall` → does not compile. Implement.
- [ ] **Step 4:** `cargo test --locked --lib loops::` → all pass. Ready to commit.

### Task 3: `StallTracker`

**Files:** Modify and test `src/loops/process.rs` (after `StopSnapshot`; import `StopLimit`).

**Produces:**

```rust
pub struct StallTracker { /* private */ }
impl StallTracker {
    pub fn new(stall: Duration, tool_stall: Duration, now_ms: u64) -> Self;
    /// False when `tool_stall <= stall`: then `tool_seen` does nothing.
    pub fn tool_enabled(&self) -> bool;
    pub fn activity(&mut self, now_ms: u64);
    pub fn tool_seen(&mut self, now_ms: u64);
    pub fn verdict(&mut self, now_ms: u64) -> Option<StopLimit>;
    pub fn longest_tolerated_ms(&self) -> Option<u64>;
}
```

Rules. `A` = last activity (starts at `now_ms` of `new`), `F` = last `tool_seen`, cleared by `activity`. All subtractions saturate. No I/O, no clock.
- `verdict`, in this order: `now − max(A, F) ≥ stall` → `Stall` (with no `F`, `max` is `A`); else `F` is set and `now − A ≥ tool_stall` → `ToolStall`; else `None`.
- When the result is not `Some(Stall)` and `now − A ≥ stall`, `verdict` updates the longest tolerated silence with `now − A` (keep the maximum). `activity` does not reset it.

- [ ] **Step 1: failing tests**, one function per line, all with `StallTracker::new(600 ms, 1200 ms, 0)` unless said:
  - `tracker_without_tool_stalls_at_stall`: `verdict(599) == None`, `verdict(600) == Some(Stall)`, longest `None`.
  - `tracker_open_tool_lives_to_the_ceiling`: `tool_seen` + `verdict` at 0, 600, 1199 → `None`; at 1200 → `Some(ToolStall)`; longest `Some(1200)`.
  - `tracker_tool_gone_late_dies_at_the_ceiling`: last `tool_seen(700)`; `verdict(1199) == None`, `verdict(1200) == Some(ToolStall)`.
  - `tracker_tool_gone_early_dies_at_stall`: last `tool_seen(300)`; `verdict(899) == None`, `verdict(900) == Some(Stall)`, longest `Some(899)`.
  - `tracker_tie_is_a_plain_stall`: last `tool_seen(600)`; `verdict(1200) == Some(Stall)`.
  - `tracker_activity_restarts_both_limits`: `tool_seen(700)`, `verdict(700) == None`, `activity(700)`, `verdict(1299) == None`, `verdict(1300) == Some(Stall)`, longest `Some(700)`.
  - `tracker_gives_each_tool_the_whole_ceiling`: `tool_seen(1100)`, `verdict(1100)`, `activity(1100)`, `tool_seen(2299)`, `verdict(2299) == None`, `tool_seen(2300)`, `verdict(2300) == Some(ToolStall)`.
  - `tracker_equal_or_smaller_tool_limit_is_off`: for tool limits 600 and 500: `!tool_enabled()`, `tool_seen(599)`, `verdict(599) == None`, `verdict(600) == Some(Stall)`, longest `None`.
- [ ] **Step 2:** `cargo test --locked --lib tracker_` → does not compile. Implement.
- [ ] **Step 3:** `cargo test --locked --lib tracker_` → 8 pass. Ready to commit.

### Task 4: `EventParser::tool_view`

**Files:** Modify and test `src/loops/adapters.rs`.

**Produces:**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolView {
    Open(Vec<String>),
    NoneOpen,
    /// The CLI never says when a tool starts (`opencode`); `step` counts step boundaries.
    Unseen { step: u64 },
}
impl EventParser { pub fn tool_view(&self) -> ToolView }
```

Rules: `opencode` → always `Unseen`, where `step` counts lines with `type` `step_start` or `step_finish` (new private counter `opencode_boundaries`, incremented in `record_event`; do not reuse `opencode_steps`). Other providers: names of `open_tools` in order, `NoneOpen` when empty. `finish_interrupted` does not change.

- [ ] **Step 1: failing tests** (reuse `fixture`):
  - `tool_view_follows_open_tools`: feed the first N lines: `claude_ok.jsonl` 1 → `NoneOpen`, 2 → `Open(["Read"])`, 3 → `NoneOpen`; `agy_ok.jsonl` 4 → `Open(["read_file"])`, 5 → `NoneOpen`. Calling it twice gives the same result, and `finish_interrupted` still works after it.
  - `tool_view_keeps_parallel_tools_with_one_name`: pi starts `a`, `b` (bash) → `Open(["bash","bash"])`; end `a` → `Open(["bash"])`; end `b` → `NoneOpen`.
  - `tool_view_ignores_bad_lines`: pi: not JSON, start without `toolCallId`, start with `toolName` renamed to `name` → `NoneOpen` each; claude `tool_use` without `id` → `NoneOpen`; agy `ACTIVE` without `step_index` → `NoneOpen`. Then a valid pi start and a **cut** end line (`{"type":"tool_execution_end","toolCallId":"c1"` without the closing brace) → still `Open(["bash"])`.
  - `tool_view_counts_opencode_boundaries`: `opencode_ok.jsonl` after 0..=6 lines → `step` 0, 1, 1, 2, 3, 3, 4.
- [ ] **Step 2:** `cargo test --locked --lib tool_view` → does not compile. Implement.
- [ ] **Step 3:** `cargo test --locked --lib loops::adapters` → all pass. Ready to commit.

### Task 5: the monitor uses the tracker

**Files:** Modify and test `src/loops/process.rs`. Modify the call in `src/loops/engine.rs` (pass `None` for now).

**Consumes:** Tasks 2, 3, 4. **Produces:**

```rust
/// Lets the monitor tolerate a silent open tool. `None` in `run_process_diagnosed` turns it off.
pub struct ToolWatch<'a> {
    pub limit: Duration,
    pub view: &'a mut dyn FnMut() -> ToolView,
    /// Filled by the monitor, whatever the outcome.
    pub silence: Option<ToolSilence>,
}
pub fn run_process_diagnosed(/* same */ probe_activity: &mut dyn FnMut() -> bool,
    mut tool: Option<&mut ToolWatch<'_>>, stop: &mut Option<StopSnapshot>) -> Result<Outcome>
```

- `StopSnapshot` gains `limit: Option<StopLimit>` and `tool_signal: Option<ToolSignal>`; `stop_snapshot` leaves both `None` and the caller sets them. `limit` only for `Stalled`; `tool_signal` (`Events`) for `Stalled` and `TimedOut`, when a tool is open in that same iteration.
- `run_process` and the 4 existing test calls pass `None`.

Wiring (order matters):
1. Create `start_ms` and the tracker **before** `let mut watch = || …`; move the existing `clock.monotonic_ms()` call, do not add another one (`StepClock` advances on every call). Without `tool`, use `limits.stall` as the tool limit. `last_activity_ms` and `stall_ms` disappear.
2. After `now` is read, replacing the activity code:

```rust
if saw_io_bytes || probed_activity {
    tracker.activity(now);
}
let open_tools = if tracker.tool_enabled() {
    tool.as_deref_mut().and_then(|t| match (t.view)() {
        ToolView::Open(names) => Some(names),
        ToolView::NoneOpen | ToolView::Unseen { .. } => None,
    })
} else {
    None
};
if open_tools.is_some() {
    tracker.tool_seen(now);
}
// Asked before the checks below, so a silence ended by a timeout or a cancel is recorded too.
let verdict = tracker.verdict(now);
```

3. The cancel check, then the timeout check, then `if let Some(limit) = verdict` → snapshot with `limit`, `Outcome::Stalled`. Same order as today.
4. Right after `let watched = watch();` (before `watched?`): when `tool` is `Some`, set `silence` from `tracker.longest_tolerated_ms()` with source `Events`.

- [ ] **Step 1: failing tests** with `StepClock::new(50)` (each probe is 50 ms of fake time), `stall` 150 ms. Scripts and `PidGuard` as in `stall_ends_process_tree_including_setsid_grandchild`; `probe_activity` returns `true` until the pid file is parsed and `false` after. `timeout` 60 s unless said. `open` = `|| ToolView::Open(vec!["bash".into()])`.
  - `open_tool_ends_at_the_ceiling_with_its_tree`: limit 400 ms, `open` → `Stalled`; `stop.limit == Some(ToolStall)`; `stop.tool_signal` = `Events`, `["bash"]`; `silence.longest_ms >= 400`; the `setsid` grandchild is gone.
  - `silence_with_an_open_tool_still_obeys_cancel`: limit 10 s, `open`; the probe stores `cancel` on its 6th silent call → `Cancelled`, no snapshot, `silence.longest_ms >= 150`, grandchild gone.
  - `silence_with_an_open_tool_still_obeys_the_timeout`: script `sleep 30`, probe `false`, `timeout` 1 s, limit 10 s, `open` → `TimedOut`, `limit == None`, `tool_signal.is_some()`, `silence.longest_ms >= 150`.
  - `no_open_tool_stalls_as_before`: script `sleep 30`, probe `false`, limit 10 s, for views `NoneOpen` and `Unseen { step: 3 }`; plus `open` with limit 150 ms (off); plus `tool = None` → always `Stalled`, `limit == Some(Stall)`, `tool_signal == None`, `silence == None`.
- [ ] **Step 2:** `cargo test --locked --lib loops::process` → does not compile. Implement.
- [ ] **Step 3:** `cargo test --locked --lib loops::` → all pass; run the four new tests 5 times: `for i in 1 2 3 4 5; do cargo test --locked --lib open_tool && cargo test --locked --lib silence_with || break; done`. Ready to commit.

### Task 6: engine wiring

**Files:** Modify and test `src/loops/engine.rs`.

**Consumes:** `Workflow.tool_stall_secs`, `ToolWatch`, `tool_view`, Task 2 types. **Produces:** `pub tool_stall: Duration` in `StageRequest`, after `limits`.

- Run loop: `tool_stall: Duration::from_millis(self.config.workflow.tool_stall_secs.saturating_mul(1000))`. The existing test request gets `tool_stall: Duration::from_millis(800)` (equal to its `stall`, so off).
- `ProcessStageRunner::run`: the parser is used by two closures, so `RefCell`:

```rust
let parser = RefCell::new(EventParser::new(req.stage.provider));
let mut view = || parser.borrow().tool_view();
let mut tool_watch = ToolWatch { limit: req.tool_stall, view: &mut view, silence: None };
// run_process_diagnosed(…, on_line: parser.borrow_mut().line(line), …, Some(&mut tool_watch), &mut stop_snapshot)
let tool_silence = tool_watch.silence;
let parser = parser.into_inner();
```

- Every `StageResult` in `match proc_res` gets `tool_silence`. The `StopDiagnostic` takes `limit` and `tool_signal` from the snapshot; `limit_secs: (snapshot.limit == Some(StopLimit::ToolStall)).then(|| req.tool_stall.as_secs())`.
- `StageOutcome::Stalled`: the base text is `etapa travada sem atividade com ferramenta aberta (STALLED, tool_stall_secs)` when `result.stop` has `limit == Some(StopLimit::ToolStall)`; otherwise today's text. `with_hint` stays.

- [ ] **Step 1: test helper** (`GO` is outside the workspace, so creating it is not activity; the clock stops after `steps`, and the fake CLI gives up after about 10 s):

```rust
/// Runs one stage with a fake CLI. Once `lines` lines were read, each probe advances the clock by
/// 100 s, `steps` times; on the last one the file `@GO@` appears. `stall` 600 s, `tool_stall` 1200 s.
fn run_silent_stage(provider: Provider, lines: u32, steps: u32, body: &str) -> StageResult {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let go = tmp.path().join("go");
    let wait = "i=0; while [ ! -f \"@GO@\" ]; do i=$((i+1)); [ $i -gt 500 ] && exit 9; sleep 0.02; done";
    let script = format!("#!/usr/bin/env sh\n{}\n", body.replace("@WAIT@", wait))
        .replace("@GO@", &go.display().to_string());
    crate::test_support::write_executable(bin_dir.join(provider.as_str()), script);

    let mut stage = wrc_workflow(1).stages[0].clone();
    stage.provider = provider;
    stage.model = (provider == Provider::Pi).then(|| "opencode-go/glm-5.3".to_string());

    let ws = FakeWorkspace::new(tmp.path().join("ws"));
    let sb = PassthroughSandbox { bin_dir };
    let clock = FakeClock::new();
    let (seen, stepped) = (std::cell::Cell::new(0u32), std::cell::Cell::new(0u32));
    let pulse = |active: bool| {
        if active {
            seen.set(seen.get() + 1);
        } else if seen.get() >= lines && stepped.get() < steps {
            clock.advance(100_000);
            stepped.set(stepped.get() + 1);
            if stepped.get() == steps {
                fs::write(&go, "").unwrap();
            }
        }
    };
    let cancel = AtomicBool::new(false);
    let log_path = tmp.path().join("stage.log");
    ProcessStageRunner::new(&sb, &clock).run(StageRequest {
        run_id: "20261125-000000-tst0", cycle: 1, exec_seq: 1, stage: &stage,
        workspace: ws.path(), prompt: "faça algo", log_path: &log_path,
        limits: Limits { timeout: Duration::from_secs(100_000), stall: Duration::from_secs(600) },
        tool_stall: Duration::from_secs(1200), cancel: &cancel, pulse: &pulse,
    })
}
```

  The lines of each fake CLI (print with `printf '%s\n' '<line>' …` in **one** `printf`):

| Provider | start | end | result |
|---|---|---|---|
| `Pi` | `{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash"}` | `{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash"}` | `{"type":"agent_settled"}` |
| `Claude` | `{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}` | `{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}` | `{"type":"result","is_error":false,"num_turns":1}` |
| `Agy` | `{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"tool","tool_name":"shell"}}` | same with `"state":"DONE"` | `{"event":"result","result":{"status":"SUCCESS"}}` |

  The first advance happens in the iteration that read the last line, so it is the activity instant: `steps = 8` gives 700 s of silence.
- [ ] **Step 2: failing tests**
  - `silent_open_tool_survives_stall_and_is_recorded` (3 providers): print start; `@WAIT@`; print end and result. `lines` 1, `steps` 8 → `Ok`, `error == None`, `tool_silence == Some(ToolSilence { longest_ms: 700_000, source: ToolSource::Events })`, `stop == None`.
  - `closed_tool_with_a_background_shell_stalls_at_stall` (3 providers): `sleep 60 >/dev/null 2>&1 &`; print start and end together; `@WAIT@`. `lines` 2, `steps` 30 → `Stalled`, `stop.limit == Some(Stall)`, `limit_secs == None`, `tool_signal == None`, `tool_silence == None`, `stop.silent_ms == 700_000` (600 s after the activity instant).
  - `silent_opencode_stage_stalls_as_before`: print `{"type":"step_start"}`; `sleep 60 >/dev/null 2>&1 &`; `@WAIT@`. `lines` 1, `steps` 30 → same assertions.
  - `open_tool_that_never_ends_stops_at_the_ceiling` (`Pi`): print start; `@WAIT@`. `lines` 1, `steps` 30 → `Stalled`, `limit == Some(ToolStall)`, `limit_secs == Some(1200)`, `tool_signal` = `Events`, `["bash"]`, `tool_silence.longest_ms == 1_200_000`.
  - `failed_stage_keeps_its_tool_silence` (`Pi`): print start; `@WAIT@`; `exit 3`. `lines` 1, `steps` 8 → `NonZeroExit(3)`, `tool_silence.longest_ms == 700_000`.
  - `stage_request_carries_tool_stall_and_max_duration_still_wins` (`ScriptedRunner`, `Arc<FakeClock>`, `wrc_workflow(3)`: stage 60 s, stall 30 s, max 300 s): 1st action advances 250 s and returns `StageResult::default()`; 2nd asserts `req.tool_stall == Duration::from_secs(1200)`, `req.limits.stall == Duration::from_secs(30)`, `req.limits.timeout == Duration::from_secs(50)`, advances 50 s, returns `TimedOut` → reason contains `MAX_DURATION`.
  - `tool_stall_reason_and_silence_reach_the_state_without_a_secret` (`ScriptedRunner`, `with_extra_secrets`): `Stalled` with `limit: Some(ToolStall)`, `limit_secs: Some(1200)`, `running_tools: Some(["bash"])`, a `tool_signal` name containing the secret, and `tool_silence` 1_200_000 → reason contains `etapa travada sem atividade com ferramenta aberta (STALLED, tool_stall_secs): ferramenta em execução: bash`; `state.json` loaded with `load_state` has the same `limit` and `tool_silence`; the secret is in neither `state.json` nor `report.md`. A second run with `limit: Some(Stall)` keeps `etapa travada sem atividade (STALLED): ferramenta em execução: bash`.
- [ ] **Step 3:** `cargo test --locked --lib loops::engine` → fails. Implement.
- [ ] **Step 4:** `cargo test --locked --lib loops::` → all pass; run `cargo test --locked --lib loops::engine` 5 times, all green. Ready to commit.

### Task 7: report and changelog

**Files:** Modify and test `src/loops/report.rs`; modify `CHANGELOG.md`.

Rules:
- In `## Etapas interrompidas`, only with `limit == Some(ToolStall)`, right after the `Indício` line: `- **Limite aplicado:** tool_stall_secs (1200 s); ferramenta vista por eventos do CLI`. Without `limit_secs` drop ` (1200 s)`; without `tool_signal` drop `; ferramenta vista por …`. The last part is `ToolSource::label_pt()`.
- After that section (and also when it is absent) and before the artifacts, when some execution has `tool_silence`: `\n## Silêncio longo de ferramenta\n` and one line per execution: `- implement (ciclo 1): 11m 40s; fonte: eventos do CLI` (stage through `sanitize_line`, time through `format_duration_ms`).
- Nothing else changes: with `limit` `Stall` or `None` and no `tool_silence`, the report is byte-identical to today's.

- [ ] **Step 1: failing tests**
  - `report_shows_the_applied_tool_limit`: `rich_state()` with a `stop` on `executions[0]`: `ToolStall`/1200/`Events` → the exact line above; source `Processes` → ends with `ferramenta vista por processos do passo atual`; no `limit_secs` and no signal → the line is exactly `- **Limite aplicado:** tool_stall_secs`.
  - `report_lists_long_tool_silence`: `tool_silence` 700_000 on `executions[0]` with no `stop` → contains `## Silêncio longo de ferramenta` and the exact line above; does not contain `## Etapas interrompidas`.
  - `report_without_long_silence_is_unchanged`: the same `stop` with `limit` `None` and with `Some(Stall)` renders equal strings, without `Limite aplicado` and without `Silêncio longo`.
- [ ] **Step 2:** `cargo test --locked --lib loops::report` → fails. Implement; all pass.
- [ ] **Step 3: CHANGELOG**, under `[Unreleased]` → `### Added`, one entry **Loop `tool_stall_secs`**: the optional workflow key and its default `max(1200, stall_secs)`; lower than `stall_secs` is an error, equal turns it off; signal from `claude`/`agy`/`pi` events, `opencode` unchanged; `stop.limit`, `stop.limit_secs`, `stop.tool_signal`, `tool_silence` and the two report additions; presets and `loop new` write the key, so an older binary rejects those files with "chave desconhecida".
- [ ] **Step 4: final validation**, and paste the results in the return: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`, `git status --short` (only the files of this plan).

Not automated (coordinator, after merge): spec criterion 11 on the host with `pi` inside `bwrap`.
