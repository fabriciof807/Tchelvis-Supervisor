import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  duplicateFleetAddresses,
  fleetCardView,
  loopProgressTitle,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

const row = (over = {}) => ({
  address: "t1",
  name: "demo / 1",
  project_name: "demo",
  program_label: "claude",
  program: { icon: "claude.svg" },
  status: "idle",
  branch: "main",
  labels: {},
  ...over,
});

test("an ordinary card has two lines, only the icon of the agent and no address", () => {
  const view = fleetCardView(
    row({ rolling_summary: "Monitorando sessão ativa do agente." })
  );
  assert.deepEqual(view, {
    title: "demo",
    agentName: null,
    detail: "⎇ main",
    notice: null,
    address: null,
    tierBadge: null,
  });
});

test("a real summary follows the branch, and makes room for a badge", () => {
  const summary = "Corrigindo os testes do loop que ficaram instáveis hoje";
  const plain = fleetCardView(row({ rolling_summary: summary }));
  assert.ok(plain.detail.startsWith("⎇ main · Corrigindo os testes"));
  assert.ok(plain.detail.length <= 42);
  const withPr = fleetCardView(
    row({ rolling_summary: summary, context: { pull_request: { number: 12 } } })
  );
  assert.ok(withPr.detail.length < plain.detail.length);
  assert.equal(withPr.tierBadge, null);
});

test("what needs the user keeps a line of its own", () => {
  const msg = "approve Bash: rm -rf build && cargo build --release --locked";
  for (const over of [
    { status: "blocked", message: msg },
    { status: "working", message: msg, action_card: { reason_for_operator: "x" } },
  ]) {
    const view = fleetCardView(row(over));
    assert.ok(view.notice.startsWith("approve Bash: rm -rf build"));
    assert.equal(view.detail, "⎇ main");
  }
  const finished = fleetCardView(
    row({ status: "blocked", message: msg, lifecycle: { state: "finished" } })
  );
  assert.equal(finished.notice, null);
});

test("a blocked card without a message shows no placeholder", () => {
  const view = fleetCardView(
    row({
      status: "blocked",
      rolling_summary: "Monitorando sessão ativa do agente.",
    })
  );
  assert.equal(view.notice, null);
});

test("only an ambiguous correlation is worth a badge", () => {
  const badge = (tier) => fleetCardView(row({ correlation_tier: tier })).tierBadge;
  for (const tier of ["exact", "inferred", "local_only", null]) {
    assert.equal(badge(tier), null);
  }
  assert.equal(badge("ambiguous"), "Ambiguous");
});

test("an agent without a known icon keeps its short name", () => {
  const name = (label, program = null) =>
    fleetCardView(row({ program, program_label: label })).agentName;
  assert.equal(name("my-own-agent-wrapper"), "my-own-agent-…");
  assert.equal(name("claude"), null);
  assert.equal(name("agy"), null);
  assert.equal(name("pi"), null);
  assert.equal(name("my-own-agent-wrapper", { icon: "x.svg" }), null);
  assert.equal(name(""), "agent");
});

test("a loop shows its run id instead of the project, and no repeated text", () => {
  const view = fleetCardView(
    row({
      name: "loop-20261124-193000-cafe",
      program_label: "tchelvis",
      program: null,
      status: "working",
      message: "loop 20261124-193000-cafe",
      labels: { "cockpit.loop_run": "20261124-193000-cafe" },
    })
  );
  assert.equal(view.title, "loop-20261124-193000-cafe");
  assert.equal(view.agentName, null);
  assert.equal(view.detail, "⎇ main");
});

test("without a branch the summary stands alone", () => {
  assert.equal(fleetCardView(row({ branch: null })).detail, "Local / No branch");
  assert.equal(
    fleetCardView(row({ branch: null, message: "Lendo a spec" })).detail,
    "Lendo a spec"
  );
});

test("text is cut to fit beside the status, and more in a child card", () => {
  const long = row({ project_name: "a-very-long-project-name-that-never-ends" });
  const title = (opts) => fleetCardView(long, new Set(), opts).title;
  const done = title({ statusLabel: "DONE" });
  const permission = title({ statusLabel: "PERMISSION" });
  const waiting = title({ statusLabel: "Waiting on children (2)" });
  assert.ok(done.length <= 26 && done.endsWith("…"));
  assert.ok(permission.length < done.length);
  assert.ok(waiting.length >= 10 && waiting.length < permission.length);
  assert.ok(title({ statusLabel: "DONE", isChild: true }).length < done.length);
  const detail = (isChild) =>
    fleetCardView(row({ message: "x".repeat(80) }), new Set(), { isChild }).detail;
  assert.ok(detail(true).length < detail(false).length);
});

test("a loop that does not fit keeps the end of its run id", () => {
  const loop = row({
    name: "loop-20261124-193000-cafe",
    program: null,
    labels: { "cockpit.loop_run": "20261124-193000-cafe" },
  });
  const { title } = fleetCardView(loop, new Set(), {
    statusLabel: "WORKING",
    isChild: true,
  });
  assert.ok(title.startsWith("…") && title.endsWith("193000-cafe"));
});

test("cards that would look the same show the address, and only those", () => {
  const rows = [
    row({ address: "t1" }),
    row({ address: "t2" }),
    row({ address: "t3", branch: "other" }),
    row({ address: "t4", project_name: "another" }),
    row({ address: "t5", program_label: "agy" }),
  ];
  const dup = duplicateFleetAddresses(rows);
  assert.deepEqual([...dup].sort(), ["t1", "t2"]);
  assert.equal(fleetCardView(rows[0], dup).address, "t1");
  assert.equal(fleetCardView(rows[2], dup).address, null);
  assert.equal(duplicateFleetAddresses([]).size, 0);
});

test("the card is drawn from the view, without the old rows", () => {
  const src = readFileSync(
    new URL("../../assets/herdr-app/mods/tchelvis/main.js", import.meta.url),
    "utf8"
  );
  const start = src.indexOf("const renderAgentCard =");
  const body = src.slice(start, src.indexOf("const renderGroup =", start));
  assert.ok(body.includes("fleetCardView(a, duplicates, {"));
  assert.ok(!body.includes(".child(a.address)"));
  assert.ok(!body.includes("tierLabel"));
});

test("loop progress title mirrors the Rust label", () => {
  const p = (status, cycle, role, provider) => ({
    status,
    cycle,
    max_cycles: 3,
    role,
    provider,
  });
  assert.equal(
    loopProgressTitle(p("RUNNING", 1, "worker", "pi")),
    "worker: pi · 1/3"
  );
  assert.equal(loopProgressTitle(p("RUNNING", 2, null, null)), "loop · 2/3");
  assert.equal(loopProgressTitle(p("PASS", 2, null, null)), "PASS · 2/3");
  assert.equal(
    loopProgressTitle(p("CANCELLED", 1, "worker", "pi")),
    "CANCELLED · 1/3"
  );
});

test("a loop with progress is titled by its stage, not its run id", () => {
  const loop = row({
    name: "loop-20261124-193000-cafe",
    program: null,
    status: "working",
    labels: { "cockpit.loop_run": "20261124-193000-cafe" },
    loop_progress: {
      status: "RUNNING",
      cycle: 1,
      max_cycles: 3,
      role: "worker",
      provider: "pi",
    },
  });
  assert.equal(
    fleetCardView(loop, new Set(), { statusLabel: "WORKING", isChild: true })
      .title,
    "worker: pi · 1/3"
  );
});

test("a long stage title is cut at the end", () => {
  const loop = row({
    name: "loop-20261124-193000-cafe",
    program: null,
    status: "working",
    labels: { "cockpit.loop_run": "20261124-193000-cafe" },
    loop_progress: {
      status: "RUNNING",
      cycle: 1,
      max_cycles: 3,
      role: "implementador-principal",
      provider: "opencode",
    },
  });
  const { title } = fleetCardView(loop, new Set(), {
    statusLabel: "WORKING",
    isChild: true,
  });
  assert.ok(title.startsWith("implementador"), title);
  assert.ok(title.endsWith("…"), title);
});

test("two loops at the same stage show their addresses", () => {
  const progress = {
    status: "RUNNING",
    cycle: 1,
    max_cycles: 3,
    role: "worker",
    provider: "pi",
  };
  const loop = (address) =>
    row({
      address,
      name: "loop-20261124-193000-cafe",
      program: null,
      status: "working",
      labels: { "cockpit.loop_run": "20261124-193000-cafe" },
      loop_progress: progress,
    });
  const duplicates = duplicateFleetAddresses([loop("t2"), loop("t3")]);
  assert.ok(duplicates.has("t2"), "t2");
  assert.ok(duplicates.has("t3"), "t3");
});
