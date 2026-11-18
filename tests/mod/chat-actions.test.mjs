import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CHAT_STARTERS,
  extractMentionedAgents,
  clipboardArgv,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("starters are the four agreed questions", () => {
  assert.deepEqual(CHAT_STARTERS, [
    "Quem está esperando por mim?",
    "Quais tarefas estão abertas?",
    "Resumo dos agentes",
    "Qual tarefa devo fazer primeiro?",
  ]);
});

test("mentions match whole tokens only", () => {
  const rows = [
    { address: "t1", name: "api" },
    { address: "t10", name: "web" },
  ];
  assert.deepEqual(
    extractMentionedAgents("veja t10 e web", rows).map((r) => r.address),
    ["t10"]
  );
  assert.deepEqual(
    extractMentionedAgents("t1 está bloqueado", rows).map((r) => r.address),
    ["t1"]
  );
  assert.deepEqual(
    extractMentionedAgents("API caiu", rows).map((r) => r.address),
    ["t1"]
  );
});

test("mentions are unique and capped at 4", () => {
  const rows = [
    { address: "t1", name: "alpha" },
    { address: "t2", name: "beta" },
    { address: "t3", name: "gamma" },
    { address: "t4", name: "delta" },
    { address: "t5", name: "epsilon" },
  ];
  const mentioned = extractMentionedAgents(
    "t5 aparece antes, depois beta e t1, alpha de novo, gamma, delta",
    rows
  );
  assert.deepEqual(
    mentioned.map((r) => r.address),
    ["t5", "t2", "t1", "t3"]
  );
});

test("clipboard argv keeps the text as one argument", () => {
  const text = 'a "b" $HOME\nc';
  const argv = clipboardArgv(text);
  assert.equal(argv[0], "sh");
  assert.equal(argv[1], "-c");
  assert.equal(argv[argv.length - 1], text);
  assert.ok(!argv[2].includes(text));
  assert.ok(argv[2].includes("wl-copy"));
  assert.ok(argv[2].includes("xclip"));
  assert.ok(argv[2].includes("xsel"));
});
