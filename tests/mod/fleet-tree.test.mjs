import { test } from "node:test";
import assert from "node:assert/strict";
import {
  mergeFleetRows,
  groupFleetRows,
  partitionFleetGroups,
  childConnector,
  blockedChildrenBadge,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("children_nested_under_parent", () => {
  const p = { address: "p", parent: null, status: "working" };
  const c1 = { address: "c1", parent: "p", status: "working" };
  const c2 = { address: "c2", parent: "p", status: "idle" };

  const groups = groupFleetRows([p, c1, c2]);
  assert.equal(groups.length, 1);
  assert.equal(groups[0].row.address, "p");
  assert.deepEqual(
    groups[0].children.map((c) => c.address),
    ["c1", "c2"]
  );
});

test("orphan_child_is_top_level", () => {
  const c1 = { address: "c1", parent: "missing-parent", status: "working" };
  const c2 = { address: "c2", parent: null, status: "idle" };

  const groups = groupFleetRows([c1, c2]);
  assert.equal(groups.length, 2);
  assert.deepEqual(
    groups.map((g) => g.row.address),
    ["c1", "c2"]
  );
  assert.deepEqual(groups[0].children, []);
  assert.deepEqual(groups[1].children, []);
});

test("cycle_does_not_hang_or_duplicate", () => {
  const a = { address: "a", parent: "b", status: "working" };
  const b = { address: "b", parent: "a", status: "working" };
  const x = { address: "x", parent: "x", status: "idle" };

  const groups = groupFleetRows([a, b, x]);
  const allRows = groups.flatMap((g) => [g.row, ...g.children]);
  assert.equal(allRows.length, 3);
  assert.deepEqual(
    allRows.map((r) => r.address).sort(),
    ["a", "b", "x"]
  );
});

test("grandchild_under_nearest_listed_ancestor", () => {
  const p = { address: "p", parent: null, status: "working" };
  const c1 = { address: "c1", parent: "p", status: "working" };
  const c2 = { address: "c2", parent: "c1", status: "working" };

  const groups = groupFleetRows([p, c1, c2]);
  assert.equal(groups.length, 1);
  assert.equal(groups[0].row.address, "p");
  assert.deepEqual(
    groups[0].children.map((c) => c.address),
    ["c1", "c2"]
  );

  // Also when p is absent, c1 is the nearest listed ancestor of c2.
  const orphanChain = groupFleetRows([c1, c2]);
  assert.equal(orphanChain.length, 1);
  assert.equal(orphanChain[0].row.address, "c1");
  assert.deepEqual(
    orphanChain[0].children.map((c) => c.address),
    ["c2"]
  );
});

test("blocked_child_moves_group_to_blocked", () => {
  const p = { address: "p", parent: null, status: "working", seq: 1 };
  const c1 = { address: "c1", parent: "p", status: "working", seq: 2 };
  const c2 = {
    address: "c2",
    parent: "p",
    status: "blocked",
    kind: "permission",
    seq: 3,
  };
  const other = { address: "o", parent: null, status: "working", seq: 4 };

  const parts = partitionFleetGroups(groupFleetRows([p, c1, c2, other]));
  assert.equal(parts.blocked.length, 1);
  assert.equal(parts.active.length, 1);
  assert.equal(parts.finished.length, 0);

  // Parent is still the group's row; blocked child is sorted first among children.
  assert.equal(parts.blocked[0].row.address, "p");
  assert.deepEqual(
    parts.blocked[0].children.map((c) => c.address),
    ["c2", "c1"]
  );
  assert.equal(parts.active[0].row.address, "o");
});

test("finished_only_when_all_finished", () => {
  const pFin = {
    address: "p1",
    parent: null,
    status: "done",
    lifecycle: { state: "finished" },
  };
  const cWork = {
    address: "c1",
    parent: "p1",
    status: "working",
    lifecycle: { state: "live" },
  };
  const pFin2 = {
    address: "p2",
    parent: null,
    status: "done",
    lifecycle: { state: "finished" },
  };
  const cFin2 = {
    address: "c2",
    parent: "p2",
    status: "done",
    lifecycle: { state: "finished" },
  };

  const parts = partitionFleetGroups(
    groupFleetRows([pFin, cWork, pFin2, cFin2])
  );
  assert.equal(parts.blocked.length, 0);
  assert.deepEqual(
    parts.active.map((g) => g.row.address),
    ["p1"]
  );
  assert.deepEqual(
    parts.finished.map((g) => g.row.address),
    ["p2"]
  );
});

test("connectors", () => {
  assert.equal(childConnector(0, 2), "├");
  assert.equal(childConnector(1, 2), "└");
  assert.equal(childConnector(0, 1), "└");
});

test("badge_copy", () => {
  assert.equal(
    blockedChildrenBadge({
      row: { address: "p", status: "working" },
      children: [{ address: "c1", status: "working" }],
    }),
    null
  );
  assert.equal(
    blockedChildrenBadge({
      row: { address: "p", status: "working" },
      children: [{ address: "c1", status: "blocked" }],
    }),
    "1 filho"
  );
  assert.equal(
    blockedChildrenBadge({
      row: { address: "p", status: "working" },
      children: [
        { address: "c1", status: "blocked" },
        { address: "c2", status: "working" },
        { address: "c3", attention_state: { state: "blocked" } },
      ],
    }),
    "2 filhos"
  );
});

test("merge_rows_carry_parent", () => {
  const terms = [
    { address: "t1", state: "live", program: "claude" },
    { address: "t2", state: "live", program: "claude" },
  ];
  const snap = {
    agents: [
      { address: "t1", name: "parent-agent" },
      { address: "t2", name: "child-agent", parent: "t1" },
    ],
  };

  const rows = mergeFleetRows([], terms, snap, null);
  const r1 = rows.find((r) => r.address === "t1");
  const r2 = rows.find((r) => r.address === "t2");
  assert.ok(r1, "row for t1");
  assert.ok(r2, "row for t2");
  assert.equal(r1.parent, null);
  assert.equal(r2.parent, "t1");
});

test("merge_rows_carry_active_children_and_cockpit_parent_fallback", () => {
  const terms = [
    { address: "t1", state: "live", program: "claude" },
    {
      address: "t2",
      state: "live",
      program: "claude",
      labels: { "cockpit.parent": "t1" },
    },
    {
      address: "t3",
      state: "live",
      program: "claude",
    },
  ];
  const snap = {
    agents: [
      {
        address: "t1",
        name: "delegator-agent",
        active_children: ["t2", "t3"],
      },
      {
        address: "t3",
        name: "worker-agent",
        labels: { "cockpit.parent": "t1" },
      },
    ],
  };

  const rows = mergeFleetRows(
    [{ address: "t2", program: "claude", status: "working" }],
    terms,
    snap,
    null
  );
  const r1 = rows.find((r) => r.address === "t1");
  const r2 = rows.find((r) => r.address === "t2");
  const r3 = rows.find((r) => r.address === "t3");
  assert.ok(r1, "row for t1");
  assert.ok(r2, "row for t2");
  assert.ok(r3, "row for t3");
  assert.deepEqual(r1.active_children, ["t2", "t3"]);
  assert.deepEqual(r2.active_children, []);
  assert.deepEqual(r3.active_children, []);
  assert.equal(r2.parent, "t1");
  assert.equal(r3.parent, "t1");

  const groups = groupFleetRows(rows);
  assert.equal(groups.length, 1);
  assert.equal(groups[0].row.address, "t1");
  assert.deepEqual(
    groups[0].children.map((c) => c.address).sort(),
    ["t2", "t3"]
  );
});

test("merge_rows_shows_live_loop_run_as_working_with_run_id_and_excludes_attach_client", () => {
  const runId = "20261124-193000-dead";
  const terms = [
    {
      address: "t5",
      state: "live",
      program: "tchelvis",
      cwd: "/tmp/repo",
      foreground: { name: "tchelvis", argv: ["tchelvis", "loop", "engine", runId] },
      labels: { "cockpit.loop_run": runId },
    },
    {
      address: "t6",
      state: "live",
      program: "hc",
      argv: ["hc", "attach", "t5"],
      foreground: { name: "hc", argv: ["hc", "attach", "t5"] },
      labels: { "cockpit.loop_run": runId },
    },
  ];
  const liveAgents = [
    { address: "t5", program: "tchelvis", status: "idle" },
    { address: "t6", program: "hc", status: "idle" },
  ];

  const rows = mergeFleetRows(liveAgents, terms, null, null);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].address, "t5");
  assert.equal(rows[0].status, "working");
  assert.ok(rows[0].name.includes(runId), `expected RUN_ID in name, got ${rows[0].name}`);
  assert.equal(rows[0].attach_target, "t5");
});



test("merge_rows_carry_loop_progress", () => {
  const terms = [
    { address: "t2", state: "live", labels: { "cockpit.loop_run": "r2" } },
    { address: "t3", state: "live", labels: { "cockpit.loop_run": "r3" } },
    { address: "t4", state: "live", labels: { "cockpit.loop_run": "r4" } },
  ];
  const progress = {
    status: "RUNNING",
    cycle: 1,
    max_cycles: 3,
    role: "worker",
    provider: "pi",
  };
  const snap = {
    agents: [
      { address: "t2", name: "loop-r2", loop_progress: progress },
      { address: "t3", name: "loop-r3", loop_progress: { ...progress } },
    ],
  };

  const rows = mergeFleetRows(
    [{ address: "t2", status: "working" }],
    terms,
    snap,
    null
  );
  const r2 = rows.find((r) => r.address === "t2");
  const r3 = rows.find((r) => r.address === "t3");
  const r4 = rows.find((r) => r.address === "t4");
  assert.deepEqual(r2.loop_progress, progress);
  assert.deepEqual(r3.loop_progress, progress);
  assert.ok(!r4.loop_progress, "t4 has no loop progress");
});
