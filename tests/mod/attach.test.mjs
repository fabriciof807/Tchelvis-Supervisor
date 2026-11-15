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
