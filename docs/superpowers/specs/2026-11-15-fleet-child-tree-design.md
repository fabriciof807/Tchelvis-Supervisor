# Fleet: child terminals nested under their parent — design

**Date:** 2026-11-15 · **Status:** approach A chosen by the user · **Size:** small change

## Intent
Show at a glance which terminals are subagents of which, like the default Herdr Desktop sidebar:
the child sits right under its parent, indented, joined by an "L" connector.

## Goals / non-goals / anti-goals
- Goals: nest children under their parent in COCKPIT FLEET; a blocked child stays visible at the top.
- Non-goals: collapsing/expanding groups; drag-and-drop; changing the Inspector.
- Anti-goals: a child shown twice, or a blocked child hidden inside a section the user is not looking at.

## Approaches
- **A — child always under its parent (chosen).** If any child is blocked, the parent's group moves
  to the "needs you" section and the parent row shows a badge (`⚠ 1 filho`).
- B — a blocked child leaves its group and shows alone at the top with "↳ de <pai>". Rejected: it
  splits child from parent exactly when the context matters.

## Design
- **Data:** the daemon snapshot already carries `parent` per terminal (from `hc`), and
  `blocked_children` per parent. `mergeFleetRows` copies `parent` into each fleet row.
- **Grouping (pure, exported, unit-tested):** `groupFleetRows(rows)` returns top-level groups
  `{ row, children: [row…] }`. A row is a child when its `parent` is the address of another row in
  the list; otherwise (no parent, or parent not listed — e.g. finished and hidden) it is top-level.
  Nesting is one level deep: a grandchild is shown under its nearest listed ancestor.
- **Sections:** `partitionAgents` sorts groups, not rows. A group goes to "needs you" when the
  parent or any child is blocked; to "finished" only when the parent and all children are
  finished; otherwise "active". Inside a group, children keep their own order (blocked first).
- **Rendering:** child rows are indented with an "└" connector (last child) / "├" (others) in the
  muted color, same row component as today. The parent row shows `⚠ N filho(s)` in the blocked
  color when N children are blocked.
- **Click/selection:** unchanged; each row (parent or child) is its own clickable card.

## Left to the builder
Exact indent width, connector glyphs vs. drawn lines, and badge styling, matching the existing
row component.
