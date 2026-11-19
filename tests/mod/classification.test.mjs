import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as mod from "../../assets/herdr-app/mods/tchelvis/main.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../..");
const fixturePath = path.join(repoRoot, "tests/fixtures/agent_classification.json");

test("JS agent, plain-shell, and supervisor lists match Rust canonical fixture", () => {
  const fixture = JSON.parse(fs.readFileSync(fixturePath, "utf8"));
  assert.deepEqual([...mod.KNOWN_AGENTS].sort(), [...fixture.known_agents].sort());
  assert.deepEqual([...mod.PLAIN_SHELLS].sort(), [...fixture.plain_shells].sort());
  assert.deepEqual(
    [...mod.SUPERVISOR_PROGRAMS].sort(),
    [...fixture.supervisor_programs].sort()
  );
});

test("pip, ampere, and opencodex are not matched as agents (exact-token matching only)", () => {
  for (const nonAgent of ["pip", "ampere", "opencodex", "champ"]) {
    assert.equal(mod.isKnownAgent(nonAgent), false, `${nonAgent} must not be a known agent`);
    assert.equal(mod.resolveProgramName(nonAgent, {}, null), nonAgent);

    const shellRunningNonAgent = {
      address: "t99",
      state: "live",
      foreground: { name: "zsh", argv: ["zsh", "-c", nonAgent] },
      status: { state: "working", app: "zsh" },
    };
    assert.equal(
      mod.isPlainShellRow("shell", "zsh", {}, shellRunningNonAgent),
      true,
      `shell running ${nonAgent} must remain filtered as a plain shell`
    );
  }
});

test("-zsh, /bin/bash, and ksh are plain shells", () => {
  for (const shell of ["-zsh", "/bin/bash", "ksh", "csh", "tcsh", "screen", "login"]) {
    assert.equal(mod.isPlainShell(shell), true, `${shell} must be a plain shell`);
    assert.equal(
      mod.isPlainShellRow(shell, shell, {}, { address: "t1", foreground: { name: shell, argv: [shell] } }),
      true,
      `row for ${shell} must be treated as plain shell`
    );
  }
});

test("herdr-core attach x is detected as an attach client and filtered", () => {
  assert.equal(mod.extractAttachTargetFromArgs(["herdr-core", "attach", "x"]), "x");
  assert.equal(
    mod.isPlainShellRow("x", null, {}, { address: "t2", argv: ["herdr-core", "attach", "x"] }),
    true
  );
  assert.equal(
    mod.extractAttachTargetFromArgs(["hc", "run", "--", "attach", "x"]),
    null
  );
});

test("agy, antigravity, gemini, cursor-agent, and claude-code are recognized as agents", () => {
  for (const agent of ["agy", "antigravity", "gemini", "cursor-agent", "claude-code", "claudecode"]) {
    assert.equal(mod.isKnownAgent(agent), true, `${agent} must be a known agent`);
    assert.equal(mod.resolveProgramName(agent, {}, null), agent);
    assert.equal(
      mod.isPlainShellRow(agent, agent, {}, { address: "t3", foreground: { name: agent, argv: [agent] } }),
      false,
      `${agent} must not be filtered as a plain shell`
    );
  }
});

test("supervisor programs are excluded by isPlainShellRow", () => {
  for (const sup of ["tchelvis", "herdr-supervisor", "tchelvis-supervisor"]) {
    assert.equal(
      mod.isPlainShellRow(sup, sup, {}, { address: "t4", foreground: { name: sup, argv: [sup] } }),
      true
    );
  }
});
