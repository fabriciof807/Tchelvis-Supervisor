import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as mod from "../../assets/herdr-app/mods/tchelvis/main.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../..");
const fixturesDir = path.join(repoRoot, "tests/fixtures");

function readFixture(name) {
  return JSON.parse(fs.readFileSync(path.join(fixturesDir, name), "utf8"));
}

test("schemaVersion in main.js matches SNAPSHOT_SCHEMA_VERSION in src/model.rs", () => {
  const modelRs = fs.readFileSync(path.join(repoRoot, "src/model.rs"), "utf8");
  const match = modelRs.match(/pub const SNAPSHOT_SCHEMA_VERSION:\s*u32\s*=\s*(\d+)\s*;/);
  assert.ok(match, "SNAPSHOT_SCHEMA_VERSION found in src/model.rs");
  const rustVersion = Number(match[1]);
  assert.equal(mod.schemaVersion, rustVersion);
});

test("buildViewModel and extractInspectorContext surface Rust Issue, CI, Slack, and Runbook fields", () => {
  const cockpitSnap = readFixture("cockpit_snapshot.json");
  const inspectResp = readFixture("inspect_response.json");

  const vm = mod.buildViewModel(cockpitSnap, "t1", inspectResp);
  assert.equal(vm.versionMismatch, false);
  assert.equal(vm.selectedAgent?.address, "t1");
  assert.equal(typeof mod.extractInspectorContext, "function");

  const ctx = vm.inspectorContext ?? mod.extractInspectorContext(vm.inspector);
  assert.ok(ctx, "inspectorContext present on viewModel");

  assert.ok(ctx.pr, "PR present");
  assert.equal(ctx.pr.number, 42);
  assert.equal(ctx.pr.title, "Add OAuth callback");

  assert.equal(ctx.issues.length, 1);
  assert.equal(ctx.issues[0].key, "ENG-42");
  assert.equal(ctx.issues[0].title, "Implement OAuth callback");
  assert.equal(ctx.issues[0].status, "In Progress");
  assert.equal(ctx.issues[0].url, "https://linear.app/acme/issue/ENG-42");

  assert.ok(ctx.ci, "CI summary present");
  assert.equal(ctx.ci.overallState, "failing");
  assert.equal(ctx.ci.badgeText, "CI: FAILING");
  assert.equal(ctx.ci.tone, "blocked");
  assert.equal(ctx.ci.checks.length, 3);
  assert.deepEqual(ctx.ci.checks[0], {
    name: "cargo-test",
    status: "completed",
    conclusion: "failure",
    displayStatus: "failure",
    failed: true,
    detailsUrl: "https://github.com/acme/api/actions/runs/1",
  });
  assert.deepEqual(ctx.ci.checks[1], {
    name: "clippy",
    status: "completed",
    conclusion: "success",
    displayStatus: "success",
    failed: false,
    detailsUrl: null,
  });
  assert.deepEqual(ctx.ci.checks[2], {
    name: "e2e",
    status: "in_progress",
    conclusion: null,
    displayStatus: "in_progress",
    failed: false,
    detailsUrl: null,
  });

  assert.equal(ctx.slackThreads.length, 1);
  assert.equal(ctx.slackThreads[0].channel_id, "C123");
  assert.equal(ctx.slackThreads[0].reply_count, 4);

  assert.equal(ctx.runbooks.length, 1);
  assert.equal(ctx.runbooks[0].title, "Auth Troubleshooting");
  assert.equal(ctx.runbooks[0].matchReason, "matched branch keyword auth");
});

test("mergeFleetRows and extractFleetCardSummary surface Rust PR/CI, Issue, OpenCode kind, and Supervisor Evaluating state", () => {
  const cockpitSnap = readFixture("cockpit_snapshot.json");
  const supSnap = readFixture("desktop_mod_snapshot.json");

  assert.equal(typeof mod.extractFleetCardSummary, "function");
  assert.equal(typeof mod.isSupervisorRuntimeEvaluating, "function");

  const rows = mod.mergeFleetRows([], [], cockpitSnap, supSnap);
  const r1 = rows.find((r) => r.address === "t1");
  const r2 = rows.find((r) => r.address === "t2");
  assert.ok(r1, "row for t1");
  assert.ok(r2, "row for t2");

  const card1 = mod.extractFleetCardSummary(r1);
  assert.equal(card1.issueKey, "ENG-42");
  assert.equal(card1.prBadgeText, "#42 FAILING");
  assert.equal(card1.prTone, "blocked");

  const card2 = mod.extractFleetCardSummary(r2);
  assert.equal(card2.issueKey, "ENG-99");
  assert.equal(card2.prBadgeText, null);
  assert.equal(r2.program_label, "opencode");

  assert.equal(r1.action_card?.reason_for_operator, "Needs confirmation");
  assert.equal(r1.audit_badge, "Auto-assisted (1/3)");
  assert.equal(mod.isSupervisorRuntimeEvaluating(supSnap.runtime_status), true);
  assert.equal(mod.isSupervisorRuntimeEvaluating("Idle"), false);
  assert.equal(mod.isSupervisorRuntimeEvaluating({ Watching: 2 }), false);
});
