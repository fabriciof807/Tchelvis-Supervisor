# Loop Fleet card: active stage and cycle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax. Do the tasks in order, one at a time.

**Goal:** A loop's child card in the Fleet says what the loop is doing (`worker: pi · 1/3`, `loop · 2/3`, `PASS · 2/3`) instead of `…<run_id>`, and so does the parent's `↳ Active children:` line.

**Architecture:** The engine writes the running stage into `state.json` (`active_stage`). The daemon, while building the snapshot, reads `state.json` of each loop terminal into `AgentRow.loop_progress` and rewrites the parent's `active_children` entries with `LoopProgress::short_label()`. The mod titles the card with a JS mirror of that label.

**Tech Stack:** Rust 2021 (`serde`, `tracing`), plain JS mod tested with `node --test`. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-26-loop-fleet-stage-label-design.md` (decides behaviour; read it first).

## Global Constraints

- Label formats, exact (` · ` is space, U+00B7, space):
  - running inside a stage: `<role>: <provider> · <cycle>/<max_cycles>`
  - running between stages: `loop · <cycle>/<max_cycles>`
  - final: `<STATUS> · <cycle>/<max_cycles>` with `PASS`, `ESCALATE`, `ERROR`, `CANCELLED`.
- The final status wins: a final run never shows a role, even if `active_stage` was left set by a crash.
- New fields are optional: `#[serde(default, skip_serializing_if = "Option::is_none")]` on `RunState.active_stage` and on `AgentRow.loop_progress` (the skip keeps every non-loop snapshot row byte-identical to today, which the fixture tests rely on). `LOOP_STATE_SCHEMA_VERSION` stays 1; `SNAPSHOT_SCHEMA_VERSION` does not change.
- No new Herdr labels, no `hc` call from the engine. `AgentSummaryRow`, `tchelvis status` table, `loop status`, `report.md` unchanged. No model name on the card.
- A failure to read a loop's state (no `loop_runs` record, missing dir, unreadable or unknown-schema `state.json`) gives `loop_progress = None`, logged with `tracing::debug!`, and never fails the snapshot.
- Code, comments, CHANGELOG in English.
- Existing tests keep their assertions. The only edits allowed in them: `active_stage: None` / `loop_progress: None` in struct literals.
- Tests: no network, no `sleep` as synchronisation.
- Do not commit. At the end of each task run its tests, then `cargo fmt` and `cargo clippy --all-targets -- -D warnings` (and the node suite for Task 4); a task is done only when all pass.

## Stop and report (do not improvise) when

- an existing test fails for a reason other than a missing struct field;
- a name, signature or line in this plan does not exist in the code as described;
- a mod fixture/contract test changes output for a row that has no `loop_progress`;
- the same command fails three times for the same cause.

Before Task 1 run `cargo test --locked` and the node suite once. If either fails on the untouched tree, report it and stop.

## Review Focus

- A run started by an older engine (no `active_stage` in `state.json`) while it is RUNNING shows `loop · c/m`, not an error. Task 2 (`progress_without_active_stage_is_between_stages`).
- `loop clean` removed the run dir while the terminal is still listed: the row has no `loop_progress`, the card keeps the run id, the snapshot still builds. Task 3.
- Two loops of the same workflow at the same stage give the same title; `duplicateFleetAddresses` then shows their addresses. That is the intended disambiguation. Task 4.
- A long role name (`implementador-principal: opencode · 1/3`) is cut on the right by `ellipsize`, not from the left like a run id. Task 4.
- `active_children` now holds labels for loop children and addresses for the rest; every consumer only joins or counts it (`main.rs:1646`, `main.rs:1702`, mod `↳ Active children:` / `Waiting on children (N)`). A loop child filtered out of the snapshot keeps its address. Task 3.

---

### Task 1: The engine records the running stage in `state.json`

**Files:** Modify `src/loops/state.rs`, `src/loops/engine.rs`. Add `active_stage: None,` to every other `RunState { .. }` literal (`grep -rn "RunState {" src tests`; today in `src/loops/watch.rs`, `src/daemon.rs` tests, `src/loops/report.rs` tests, `src/loops/state.rs` tests).

**Produces:**
```rust
// state.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveStage {
    pub stage_id: String,
    pub role: String,
    pub provider: Provider,
    pub started_at_ms: i64,
}
// RunState, last field, after `summary`:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub active_stage: Option<ActiveStage>,
```

- [ ] **Step 1: failing state tests** in `state.rs` tests:

```rust
#[test]
fn active_stage_round_trips_and_is_absent_when_unset() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = sample_state(RunStatus::Running);
    state.active_stage = Some(ActiveStage {
        stage_id: "implement".to_string(),
        role: "worker".to_string(),
        provider: Provider::Pi,
        started_at_ms: 1500,
    });
    save_state(dir.path(), &state).unwrap();
    assert_eq!(load_state(dir.path()).unwrap(), state);
    let raw = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
    assert!(raw.contains("\"provider\": \"pi\""), "{raw}");

    state.active_stage = None;
    save_state(dir.path(), &state).unwrap();
    let raw = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
    assert!(!raw.contains("active_stage"), "{raw}");
    // A file written before this field existed loads as "no stage".
    assert_eq!(parse_state(&raw).unwrap().active_stage, None);
}
```

- [ ] **Step 2: failing engine tests** in `engine.rs` tests (pattern of `heartbeat_and_activity_are_updated`):

```rust
#[test]
fn active_stage_is_on_disk_only_while_its_stage_runs() {
    // wrc_workflow(3); each ScriptedRunner action loads state.json from run_dir and asserts:
    //   action 1: active_stage == Some { stage_id "implement", role "worker", provider Claude }, current_cycle == 1
    //   action 2: stage_id "review", role "reviewer"; writes review.md
    //   action 3: stage_id "audit", role "critic"; writes critic.md with "VERDICT: PASS"
    // after run(): state.status == Pass, state.active_stage == None,
    //              load_state(&run_dir).unwrap().active_stage == None
}

#[test]
fn active_stage_is_cleared_when_a_stage_fails() {
    // action 1 returns StageResult { outcome: StageOutcome::NonZeroExit(2), ..Default::default() }
    // after run(): status == Error, state.active_stage == None, on disk None
}
```

- [ ] **Step 3: run, expect FAIL** (compile error on `active_stage`):
`cargo test --locked --lib loops::state loops::engine::tests::active_stage`

- [ ] **Step 4: implement.**
  - `state.rs`: the struct and field above; `active_stage: None` in every literal listed under **Files**.
  - `engine.rs` `run()`: `active_stage: None` in `initial_state`. In the block that sets `guard.state.current_cycle = current_cycle;` right before the `persist_now()` that precedes `self.runner.run(..)` (~line 896), also set `guard.state.active_stage = Some(ActiveStage { stage_id: stage.id.clone(), role: stage.role.clone(), provider: stage.provider, started_at_ms: self.clock.now_ms() })`.
  - Clear it (`guard.state.active_stage = None;`) under the same lock that pushes the stage's `Execution`: in the `if let Some(err) = write_error()` branch and in the `push_exec` closure. Also clear it in `end_run`.

- [ ] **Step 5: run, expect PASS:** `cargo test --locked --lib loops::` then `cargo fmt`, `cargo clippy --all-targets -- -D warnings`.

---

### Task 2: `LoopProgress`, its label, and the mapping from `RunState`

**Files:** Modify `src/model.rs` (type + `AgentRow` field + 5 literals), `src/loops/state.rs` (mapping), `src/correlate.rs` (2 literals, incl. `build_initial_agent_row` ~line 505), `src/main.rs:2107`, `tests/integration_tests.rs` (2 literals): add `loop_progress: None,`.

**Consumes:** `RunState.active_stage` (Task 1).

**Produces:**
```rust
// model.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopProgress {
    pub status: String,          // RunStatus::as_str(): "RUNNING", "PASS", ...
    pub cycle: u32,
    pub max_cycles: u32,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub provider: Option<String>, // Provider::as_str()
}
impl LoopProgress { pub fn short_label(&self) -> String }
// AgentRow, after `active_children`:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub loop_progress: Option<LoopProgress>,

// state.rs
pub fn loop_progress_from_state(s: &RunState) -> LoopProgress
```
Mapping: `status = s.status.as_str()`, `cycle = s.current_cycle`, `max_cycles = s.max_cycles`; `role`/`provider` from `active_stage` only when `s.status == RunStatus::Running`, otherwise `None`.
`short_label`: status other than `"RUNNING"` → `"{status} · {cycle}/{max}"`; `RUNNING` with both `role` and `provider` → `"{role}: {provider} · {cycle}/{max}"`; otherwise `"loop · {cycle}/{max}"`.

- [ ] **Step 1: failing tests.** In `model.rs` tests:

```rust
#[test]
fn loop_progress_short_label() {
    let p = |status: &str, cycle, role: Option<&str>, provider: Option<&str>| LoopProgress {
        status: status.to_string(), cycle, max_cycles: 3,
        role: role.map(str::to_string), provider: provider.map(str::to_string),
    };
    assert_eq!(p("RUNNING", 1, Some("worker"), Some("pi")).short_label(), "worker: pi · 1/3");
    assert_eq!(p("RUNNING", 2, None, None).short_label(), "loop · 2/3");
    assert_eq!(p("PASS", 2, None, None).short_label(), "PASS · 2/3");
    assert_eq!(p("ESCALATE", 3, Some("worker"), Some("pi")).short_label(), "ESCALATE · 3/3");
}

#[test]
fn agent_row_without_loop_progress_serializes_as_before() {
    let json = serde_json::to_string(&AgentRow::default()).unwrap();
    assert!(!json.contains("loop_progress"), "{json}");
    let back: AgentRow = serde_json::from_str(&json).unwrap();
    assert_eq!(back.loop_progress, None);
}
```
In `state.rs` tests, with `sample_state` (cycle 2 of 3):

```rust
#[test]
fn progress_in_a_stage_names_role_and_provider() {
    // Running + active_stage { role "worker", provider Pi } → role Some("worker"), provider Some("pi"), cycle 2, max 3
}
#[test]
fn progress_without_active_stage_is_between_stages() {
    // Running + None → role None, provider None, status "RUNNING"
}
#[test]
fn progress_of_a_final_run_ignores_a_leftover_stage() {
    // for each of Pass, Escalate, Error, Cancelled with active_stage Some(..):
    //   status == as_str(), role None, provider None
}
```

- [ ] **Step 2: run, expect FAIL:** `cargo test --locked --lib model:: loops::state`
- [ ] **Step 3: implement** the types, function and literals above.
- [ ] **Step 4: run, expect PASS:** `cargo test --locked` (whole suite: literals changed in several crates' tests), `cargo fmt`, `cargo clippy --all-targets -- -D warnings`.

---

### Task 3: The daemon fills `loop_progress` and labels active children

**Files:** Modify and test `src/daemon.rs`.

**Consumes:** `loop_progress_from_state` (Task 2), `LoopProgress::short_label`, `HcTerminal::loop_run_id()` (`src/herdr.rs:253`), `OrchStore::get_loop` (`src/orch/store.rs:2236`), `crate::loops::state::load_state`.

**Produces (private to `daemon.rs`):**
```rust
fn loop_progress_for(store: Option<&OrchStore>, run_id: &str) -> Option<LoopProgress>
fn label_active_children(rows: &mut [AgentRow])
```
- `loop_progress_for`: `get_loop(run_id)` → `load_state(Path::new(&rec.dir))` → `loop_progress_from_state`. Any `Err`/`None` → `None` with `tracing::debug!(run_id, error = %e, "loop progress unavailable")`.
- `label_active_children`: build `address → short_label()` from rows that have `loop_progress`; replace each matching entry of every row's `active_children`; keep order and non-matching entries.
- `materialize_snapshot`: open the store once, `let orch = OrchStore::open(&paths.orch_db_file()).ok();`, compute `active_orch_children` from `orch.as_ref()` (same `unwrap_or_default` behaviour). Inside the row loop, after `active_children` is set: `if let Some(run_id) = term.loop_run_id() { row.loop_progress = loop_progress_for(orch.as_ref(), run_id); }`. After the loop and before `enrich_agents`: `label_active_children(&mut agents)`.

- [ ] **Step 1: failing tests** in `daemon.rs` tests.

```rust
#[test]
fn label_active_children_uses_loop_labels_and_keeps_other_addresses() {
    // rows: t1 { active_children: ["t2", "t3"] },
    //       t2 { loop_progress: Some(RUNNING, 1/3, worker, pi) }, t3 { loop_progress: None }
    // after: t1.active_children == ["worker: pi · 1/3", "t3"]; t2, t3 unchanged
}

#[tokio::test]
async fn test_materialize_snapshot_fills_loop_progress_from_state_json() {
    // Setup as in test_materialize_snapshot_active_children_uses_ms_not_sec (paths, cfg, CacheStore,
    // ProviderCoordinator, FleetTracker). Terminals: parent "t1" (agent), loop child "t2" with
    // labels {"cockpit.loop_run": "r-live"} and parent t1 (same parent mechanism as that test),
    // loop terminal "t3" with labels {"cockpit.loop_run": "r-gone"}.
    // Orch db at paths.orch_db_file(): register_loop "r-live" with dir <tmp>/r-live holding a
    //   state.json saved with save_state (RunState literal as in the test at daemon.rs:1516, status
    //   Running, current_cycle 1, max_cycles 3, active_stage Some { "implement", "worker", Pi, .. });
    //   register_loop "r-gone" with dir <tmp>/missing (never created).
    // Assert: t2.loop_progress == Some(LoopProgress { "RUNNING", 1, 3, Some("worker"), Some("pi") });
    //         t3.loop_progress == None; t1.active_children == ["worker: pi · 1/3"];
    //         the snapshot has all three rows.
}
```

- [ ] **Step 2: run, expect FAIL:** `cargo test --locked --lib daemon::tests`
- [ ] **Step 3: implement** as described above.
- [ ] **Step 4: run, expect PASS:** `cargo test --locked --lib daemon::`, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`.

---

### Task 4: The mod titles the loop card with its progress

**Files:** Modify `assets/herdr-app/mods/tchelvis/main.js`, `CHANGELOG.md`. Test `tests/mod/fleet-card.test.mjs`, `tests/mod/fleet-tree.test.mjs`.

**Consumes:** `loop_progress` on snapshot agents (Tasks 2–3), shape `{ status, cycle, max_cycles, role?, provider? }`.

**Produces:**
- `export function loopProgressTitle(progress)`: same rules as `LoopProgress::short_label` (Task 2), same separator ` · `.
- `fleetCardTitle(row)`: `row?.loop_progress ? loopProgressTitle(row.loop_progress)` else today's behaviour.
- `fleetCardView`: the left cut (`…${title.slice(1 - titleMax)}`) applies only when `isLoop && !row.loop_progress`; otherwise `ellipsize(title, titleMax)`.
- `mergeFleetRows`: the row built from a live agent (~line 1835) gets `loop_progress: enriched?.loop_progress ?? null`; the row built from a snapshot-only agent (~line 1932) gets `loop_progress: sa?.loop_progress ?? null`. The terminal-only builders (~lines 2018, 2096) get nothing.

- [ ] **Step 1: failing tests.** In `fleet-card.test.mjs` (import `loopProgressTitle`):

```js
test("loop progress title mirrors the Rust label", () => {
  const p = (status, cycle, role, provider) => ({ status, cycle, max_cycles: 3, role, provider });
  assert.equal(loopProgressTitle(p("RUNNING", 1, "worker", "pi")), "worker: pi · 1/3");
  assert.equal(loopProgressTitle(p("RUNNING", 2, null, null)), "loop · 2/3");
  assert.equal(loopProgressTitle(p("PASS", 2, null, null)), "PASS · 2/3");
  assert.equal(loopProgressTitle(p("CANCELLED", 1, "worker", "pi")), "CANCELLED · 1/3");
});

test("a loop with progress is titled by its stage, not its run id", () => {
  const loop = row({
    name: "loop-20261124-193000-cafe", program: null, status: "working",
    labels: { "cockpit.loop_run": "20261124-193000-cafe" },
    loop_progress: { status: "RUNNING", cycle: 1, max_cycles: 3, role: "worker", provider: "pi" },
  });
  assert.equal(fleetCardView(loop, new Set(), { statusLabel: "WORKING", isChild: true }).title,
    "worker: pi · 1/3");
});

test("a long stage title is cut at the end", () => {
  // role "implementador-principal", provider "opencode", statusLabel "WORKING", isChild true:
  // title starts with "implementador" and ends with "…"
});

test("two loops at the same stage show their addresses", () => {
  // two such loop rows (t2, t3) with equal loop_progress, program_label and branch:
  // duplicateFleetAddresses([...]) has both "t2" and "t3"
});
```
The existing loop tests (`a loop shows its run id…`, `a loop that does not fit keeps the end of its run id`) stay as they are and must still pass.

In `fleet-tree.test.mjs`:

```js
test("merge_rows_carry_loop_progress", () => {
  // terms: t2, t3, t4 live, each labels {"cockpit.loop_run": "r2"/"r3"/"r4"}
  // snap.agents: t2 and t3 with loop_progress { status "RUNNING", cycle 1, max_cycles 3, role "worker", provider "pi" }
  // liveAgents: [{ address: "t2", status: "working" }]  (t3 only in snap, t4 only in terms)
  // rows t2, t3: loop_progress deepEqual the snapshot value; row t4: loop_progress falsy
});
```

- [ ] **Step 2: run, expect FAIL:** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/fleet-card.test.mjs tests/mod/fleet-tree.test.mjs`
- [ ] **Step 3: implement** as described under **Produces**.
- [ ] **Step 4: CHANGELOG** — under `## [Unreleased]` → `### Changed`: `- The Fleet card of a loop shows its active stage and cycle (e.g. \`worker: pi · 1/3\`, then the final status) instead of the run id; the parent's active-children line uses the same text.`
- [ ] **Step 5: run, expect PASS:** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs`, then `cargo test --locked`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`.

---

## After the tasks (coordinator)

- Host check (spec acceptance 6): a `loop run --detach` shows `worker: <cli> · 1/3`, then the next role, then the final status, on the child card and on the parent's `↳ Active children:` line.
