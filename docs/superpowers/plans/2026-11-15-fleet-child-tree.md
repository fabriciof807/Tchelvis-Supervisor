# Fleet Child Tree Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Nest child terminals under their parent in the mod's COCKPIT FLEET, Herdr-style, with a blocked child pulling its whole group into "needs you".

**Architecture:** Fleet rows gain a `parent` field. Pure exported helpers group rows into `{ row, children }` and partition groups by section; `renderFleet` renders groups (parent card, then indented child cards with a connector). Everything is in the mod; no Rust change (the daemon snapshot already exposes `parent`).

**Tech Stack:** plain JS mod (`gpui-kit`, `herdr` runtime), node:test with runtime stubs.

**Spec:** `docs/superpowers/specs/2026-11-15-fleet-child-tree-design.md`

## Global Constraints

- Only `assets/herdr-app/mods/tchelvis/main.js` and `tests/mod/*.test.mjs` change.
- Approach A: a child is always rendered under its parent; never rendered twice.
- Group section: "needs you" if parent or any child is blocked; "finished" only if parent and all children are finished; else "active".
- Nesting depth on screen is one level; a grandchild is shown under its nearest listed ancestor.
- Parent badge copy: `⚠ 1 filho` / `⚠ N filhos` (blocked children count), in the blocked color.
- Connectors: `├` for a child that is not the last, `└` for the last, muted color.
- `setAgentOrder` must receive addresses in on-screen order (parent then its children).

## Review Focus

- Parent not in the list (finished and filtered out, or unknown): the child is top-level, not dropped. (Task 1 test `orphan_child_is_top_level`.)
- Parent/child cycle or self-parent from bad data: no infinite loop, every row appears exactly once. (Task 1 test `cycle_does_not_hang_or_duplicate`.)
- Grandchild (`c2.parent = c1`, `c1.parent = p`): appears under `p`, after `c1`. (Task 1 test `grandchild_under_nearest_listed_ancestor`.)
- Working parent with a blocked child: whole group in blocked, parent first. (Task 1 test `blocked_child_moves_group_to_blocked`.)
- Selection/click on a child row still selects that child's address, not the parent. (Task 2 manual check on host.)

---

### Task 1: Data and pure grouping

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` (`mergeFleetRows` row objects: add `parent`; new exported helpers near `partitionAgents`)
- Test: `tests/mod/fleet-tree.test.mjs`

**Interfaces:**
- Produces:
  - every fleet row from `mergeFleetRows` has `parent: string | null` = `enriched?.parent ?? term?.parent ?? sa?.parent ?? null` (use what each of its loops has).
  - `export function groupFleetRows(rows) -> Array<{ row, children: row[] }>` — input order preserved for top-level groups; children in input order.
  - `export function partitionFleetGroups(groups) -> { blocked, active, finished }` (arrays of groups), each group's section per Global Constraints; within `blocked` and `active`, sort groups like `partitionAgents` sorts rows (by the group's "lead" row: the first blocked row for blocked, else the parent); children inside a group: blocked first, then input order.
  - `export function childConnector(index, count) -> "├" | "└"`.
  - `export function blockedChildrenBadge(group) -> string | null` — `null` when 0, `⚠ 1 filho`, `⚠ N filhos`.
- A row is "blocked" when `status === "blocked"` (or `attention_state.state === "blocked"`), "finished" as in `partitionAgents`.

- [ ] **Step 1: Write the failing tests** in `tests/mod/fleet-tree.test.mjs`: `children_nested_under_parent` (p, c1, c2 → one group with 2 children), `orphan_child_is_top_level`, `cycle_does_not_hang_or_duplicate` (a.parent=b, b.parent=a, and x.parent=x → 3 rows total across groups, each once), `grandchild_under_nearest_listed_ancestor`, `blocked_child_moves_group_to_blocked`, `finished_only_when_all_finished`, `connectors` (`childConnector(0,2)==="├"`, `childConnector(1,2)==="└"`), `badge_copy` (0→null, 1→`⚠ 1 filho`, 2→`⚠ 2 filhos`), and `merge_rows_carry_parent` (a `mergeFleetRows` call with a snapshot agent `{address:"t2", parent:"t1"}` yields row `t2.parent === "t1"`).
- [ ] **Step 2: Run** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` — new tests FAIL.
- [ ] **Step 3: Implement** the field and helpers. Cycle safety: resolve each row's nearest listed ancestor by walking `parent` with a visited set; a row whose walk revisits itself is top-level.
- [ ] **Step 4: Run** the same command — all pass; `node --check assets/herdr-app/mods/tchelvis/main.js` exits 0.
- [ ] **Step 5: Commit** `feat(mod): fleet rows carry parent; pure child-tree grouping`.

### Task 2: Render the tree

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` (`renderFleet`)

**Interfaces:**
- Consumes: `groupFleetRows`, `partitionFleetGroups`, `childConnector`, `blockedChildrenBadge` from Task 1.

- [ ] **Step 1: Switch `renderFleet`** from `partitionAgents(merged)` to `partitionFleetGroups(groupFleetRows(merged))`. Counts (`liveCount`, section counts) count rows, not groups. `setAgentOrder` gets parent-then-children addresses in on-screen order.
- [ ] **Step 2: Render each group** as the existing row card for the parent (plus `blockedChildrenBadge` when non-null) followed by each child's existing row card inside an `h_flex` with a fixed left indent and the `childConnector` glyph in the muted color. Keep each card's own `.id(...)`, click handler and selection highlight unchanged (ids must stay unique per address).
- [ ] **Step 3: Run** `node --check …main.js` and the node tests — pass; `cargo test --quiet` — pass (untouched, sanity).
- [ ] **Step 4: Commit** `feat(mod): render child terminals nested under their parent in the fleet`; add a HANDOFF.md line under the current session's updates: fleet child tree, host validation pending (click a child selects the child; blocked child pulls the group up).
