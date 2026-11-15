import { test } from "node:test";
import assert from "node:assert/strict";
import { attachTabArgv, mergeFleetRows } from "../../assets/herdr-app/mods/tchelvis/main.js";

test("attach tab targets the terminal address, never a display name", () => {
  assert.deepEqual(attachTabArgv("t11g"), ["hc", "attach", "t11g"]);
});

test("fleet rows for unnamed core terminals attach by address", () => {
  // Agent-spawned sessions have no core name; the old code fell back to the
  // tab/agent name ("agy"), which `hc attach` rejects with not_found.
  const terms = [
    { address: "t11g", name: null, state: "live", program: "agy", labels: { "cockpit.agent": "agy" } },
  ];
  const rows = mergeFleetRows([{ address: "t11g", tab_name: "agy", status: "working" }], terms, null, null);
  const row = rows.find((r) => r.address === "t11g");
  assert.ok(row, "row for t11g");
  assert.equal(row.attach_target, "t11g");
});

import { planFleetClick } from "../../assets/herdr-app/mods/tchelvis/main.js";

test("card click on a background terminal (no pane) attaches directly", () => {
  const plan = planFleetClick([], [{ address: "t12k", state: "live" }], "t12k", null, null);
  assert.deepEqual(plan, { action: "attach" });
});

test("a stale pane id from the fleet row is not trusted: attach instead of a silent focus", () => {
  const panes = [{ pane: "p9", terminal: "t99" }];
  const plan = planFleetClick(panes, [{ address: "t12k", state: "live" }], "t12k", null, "p-gone");
  assert.deepEqual(plan, { action: "attach" });
});

test("a live pane showing the terminal is focused", () => {
  const panes = [{ pane: "p1", terminal: "t12k" }];
  const plan = planFleetClick(panes, [{ address: "t12k", state: "live" }], "t12k", null, null);
  assert.deepEqual(plan, { action: "focus-pane", pane: "p1" });
});

test("an existing hc attach tab for the terminal is focused, not duplicated", () => {
  const terms = [
    { address: "t12k", state: "live" },
    { address: "t20a", state: "live", argv: ["hc", "attach", "t12k"] },
  ];
  const panes = [{ pane: "p7", terminal: "t20a" }];
  assert.deepEqual(planFleetClick(panes, terms, "t12k", null, null), { action: "focus-pane", pane: "p7" });
});
