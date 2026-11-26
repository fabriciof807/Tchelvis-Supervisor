# Loop: the Fleet card shows the active stage and cycle

Date: 2026-11-26. Size: small change (`src/loops/state.rs`, `src/loops/engine.rs`,
`src/daemon.rs` + `src/model.rs`, `assets/herdr-app/mods/tchelvis/main.js`). Origin: host
screenshot of a `loop run --detach` child card titled `…20261125-200014-5e4d`.

## Intent

A loop's terminal appears in the Fleet as a child of the coordinator, titled with its run id
(`loop-<run_id>`, cut from the left). The id says nothing to the user. The card should say what
the loop is doing now, e.g. `worker: pi · 1/3` (role, provider, cycle / max cycles).

Today nothing records the running stage: `Execution` entries are pushed to `state.json` only when
a stage ends, and the only Herdr label is `cockpit.loop_run`, set once at launch.

## Goals

- While a stage runs, the card title is `<role>: <provider> · <cycle>/<max_cycles>`.
- Between stages (gates, snapshot, preflight) the title is `loop · <cycle>/<max_cycles>`.
- When the run is final, the title is `<STATUS> · <cycle>/<max_cycles>` (`PASS`, `ESCALATE`,
  `ERROR`, `CANCELLED`).
- The parent's `↳ Active children:` line names the loop child by the same short text instead of
  its terminal address.
- Old `state.json` files (no new field) and rows without loop data keep today's title.

## Non-goals

- No new Herdr labels, no `hc` calls from the engine (the engine may run inside `bwrap`).
- No change to the TUI table (`tchelvis status`), `loop status` or `report.md`.
- No model name on the card (too long for the title; it stays in `loop status`).

## Approaches

1. **Engine records the active stage in `state.json`; the daemon reads it into the snapshot
   (chosen).** One source of truth, survives Herdr restarts, nothing to keep in sync, works in any
   sandbox. Cost: one small JSON read per live loop row per snapshot (every ~5 s).
2. Engine updates Herdr labels per stage. Needs `hc` reachable from the engine and leaves stale
   labels if the engine dies. Rejected.
3. Mod reads `state.json` itself. The mod has no file access to the loop dirs and would duplicate
   parsing. Rejected.

## Design

- **State (additive).** `RunState.active_stage: Option<ActiveStage>` with `stage_id`, `role`,
  `provider`, `started_at_ms`; `#[serde(default, skip_serializing_if = "Option::is_none")]`. No
  schema version bump (same rule as `stop` / `tool_silence`).
- **Engine.** Set `active_stage` just before the existing `persist_now()` that precedes
  `runner.run(...)`; clear it in every path that pushes the stage's `Execution` (the same lock
  that pushes it) and in `end_run`. A crash leaves it set; the final status wins on display.
- **Snapshot.** `AgentRow.loop_progress: Option<LoopProgress>` (`#[serde(default)]`), with
  `status`, `cycle`, `max_cycles`, `role: Option<String>`, `provider: Option<String>`. In
  `materialize_snapshot`, for a row whose terminal has `cockpit.loop_run`, find the run dir via
  the `OrchStore` already opened there (`loop_runs.dir`), `load_state`, and fill it. Any failure
  (no record, unreadable file) → `None`, logged at `debug`. Pure helper
  `loop_progress_from_state(&RunState) -> LoopProgress` so the mapping is unit-tested without I/O.
  `AgentSummaryRow` is unchanged.
- **Active children.** When a child address has `loop_progress`, the parent's
  `active_children` entry becomes the short title (same formatter as the mod, in Rust:
  `LoopProgress::short_label()`), otherwise the address as today.
- **Mod.** `fleetCardTitle(row)`: if `row.loop_progress` exists, use `loopProgressTitle()` (pure,
  exported, mirrors `short_label`); else today's behaviour. The loop title is no longer cut from
  the left (it is short); normal `ellipsize` applies. Rows merged only from `terminals()` have no
  `loop_progress` and keep the run id.

## Acceptance

1. Unit: `state.json` round-trip with and without `active_stage`; an old file loads with `None`.
2. Engine test with the fake runner: during a stage `state.json` has `active_stage` for that
   stage and cycle; after it and at the end it is `None`.
3. Unit: `loop_progress_from_state` for running-in-stage, running-between-stages and each final
   status; `short_label` gives `worker: pi · 1/3`, `loop · 2/3`, `PASS · 2/3`.
4. Daemon test: a loop terminal whose run dir has a `state.json` gets `loop_progress`; one with a
   missing dir gets `None` and the snapshot still builds.
5. Mod test (`node --test`): `fleetCardTitle` with `loop_progress` gives the short title; without
   it, today's output.
6. Host: a `--detach` run shows `worker: <cli> · 1/3`, then the next role, then the final status.
