import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ICONS,
  drawerHeaderTitle,
  logKindTone,
  logEventTimeLabel,
  logEventRow,
  logFilterEvents,
  LOG_FILTERS,
  LOG_KIND_LABELS,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("drawerHeaderTitle maps the log view to the supervisor log header", () => {
  const log = drawerHeaderTitle("log", {});
  assert.deepEqual(log, {
    icon: "log",
    title: "Log do supervisor",
    subtitle: null,
  });
  assert.ok(Object.hasOwn(ICONS, log.icon));
});

test("logKindTone maps every kind to its badge tone", () => {
  assert.equal(logKindTone("auto_reply"), "done");
  assert.equal(logKindTone("auto_reply_blocked"), "working");
  assert.equal(logKindTone("alert"), "working");
  assert.equal(logKindTone("alert", "critical"), "blocked");
  assert.equal(logKindTone("eval_error"), "blocked");
  assert.equal(logKindTone("respond"), "accent");
  assert.equal(logKindTone("evaluation"), "muted");
  assert.equal(logKindTone("mode_change"), "muted");
  assert.equal(logKindTone("llm_load"), "muted");
  assert.equal(logKindTone("llm_unload"), "muted");
  assert.equal(logKindTone("gpu_toggle"), "muted");
  assert.equal(logKindTone("unknown"), "muted");
});

test("logEventTimeLabel shows clock today and date when older", () => {
  const nowMs = new Date(2026, 10, 19, 12, 0, 0).getTime();
  const today = new Date(2026, 10, 19, 14, 30, 45).getTime();
  const yesterday = new Date(2026, 10, 18, 9, 5, 7).getTime();

  assert.equal(logEventTimeLabel(today, nowMs), "14:30:45");
  assert.equal(logEventTimeLabel(yesterday, nowMs), "18/11 09:05:07");
  assert.equal(logEventTimeLabel(0, nowMs), "");
  assert.equal(logEventTimeLabel("nope", nowMs), "");
});

test("logEventRow maps a raw event to a compact row view model", () => {
  const nowMs = new Date(2026, 10, 19, 12, 0, 0).getTime();
  const ts = new Date(2026, 10, 19, 13, 1, 2).getTime();

  const auto = logEventRow(
    {
      ts_ms: ts,
      kind: "auto_reply",
      agent_id: "t1",
      session_name: "refactor-auth",
      source: "llm",
      model: "qwen2.5:1.5b",
      summary: "Auto-resposta (LLM · qwen2.5:1.5b) 'y'",
      detail: "Autorizando operação.",
    },
    nowMs
  );
  assert.equal(auto.kindLabel, "Auto-resposta");
  assert.equal(auto.tone, "done");
  assert.equal(auto.nameLabel, "refactor-auth (t1)");
  assert.equal(auto.time, "13:01:02");
  assert.equal(auto.detail, "Autorizando operação.");

  const noAgent = logEventRow(
    { ts_ms: ts, kind: "mode_change", from: "SemiAuto", to: "AlertOnly", summary: "Modo alterado" },
    nowMs
  );
  assert.equal(noAgent.nameLabel, "");
  assert.equal(noAgent.kindLabel, "Modo");
  assert.equal(noAgent.tone, "muted");
  assert.equal(noAgent.detail, null);

  const agentOnly = logEventRow(
    { ts_ms: ts, kind: "llm_load", agent_id: "t2", summary: "LLM carregado" },
    nowMs
  );
  assert.equal(agentOnly.nameLabel, "t2");

  assert.equal(LOG_KIND_LABELS.auto_reply, "Auto-resposta");
});

test("logFilterEvents groups kinds into the five filter chips", () => {
  const events = [
    { kind: "auto_reply" },
    { kind: "alert" },
    { kind: "evaluation" },
    { kind: "eval_error" },
    { kind: "auto_reply_blocked" },
    { kind: "respond" },
    { kind: "mode_change" },
  ];

  assert.equal(logFilterEvents(events, "all").length, 7);
  assert.deepEqual(
    logFilterEvents(events, "actions").map((e) => e.kind).sort(),
    ["auto_reply", "auto_reply_blocked", "respond"]
  );
  assert.deepEqual(logFilterEvents(events, "alerts").map((e) => e.kind), [
    "alert",
  ]);
  assert.deepEqual(logFilterEvents(events, "evaluations").map((e) => e.kind), [
    "evaluation",
  ]);
  assert.deepEqual(logFilterEvents(events, "errors").map((e) => e.kind), [
    "eval_error",
  ]);
  // Unknown filter keys fall back to "Tudo".
  assert.equal(logFilterEvents(events, "nope").length, 7);

  // Non-array input is treated as empty, matching the empty-state path.
  assert.deepEqual(logFilterEvents(null, "all"), []);
  assert.deepEqual(logFilterEvents(undefined, "alerts"), []);
});

test("LOG_FILTERS declares the five stable chips in order", () => {
  assert.deepEqual(
    LOG_FILTERS.map((f) => f.key),
    ["all", "actions", "alerts", "evaluations", "errors"]
  );
  assert.ok(LOG_FILTERS[0].kinds === null);
});

test("mod source calls no element methods missing from gpui-kit", async () => {
  // gpui-kit has no `font_mono`; calling it throws at render time
  // ("unknown element method") and blanks the Log view. `herdr-app check`
  // does not render the view, so guard the source directly.
  const fs = await import("node:fs");
  const src = fs.readFileSync(
    new URL("../../assets/herdr-app/mods/tchelvis/main.js", import.meta.url),
    "utf8"
  );
  for (const method of ["font_mono"]) {
    assert.ok(!src.includes(`.${method}(`), `main.js calls unknown method .${method}()`);
  }
});

test("logEventRow shows the rule that decided", () => {
  const row = (ev) => logEventRow({ ts_ms: 1, kind: "auto_reply", summary: "s", ...ev });
  assert.equal(
    row({ detail: "d", rule_id: "allow_command:just test" }).detail,
    "d\nRegra: allow_command:just test"
  );
  assert.equal(row({ rule_id: "builtin_read_only" }).detail, "Regra: builtin_read_only");
  assert.equal(row({ detail: "d" }).detail, "d");
  assert.equal(row({}).detail, null);
});
