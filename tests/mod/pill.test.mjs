import { test } from "node:test";
import assert from "node:assert/strict";
import { pillModelName } from "../../assets/herdr-app/mods/tchelvis/main.js";

test("short model names are untouched", () => {
  assert.equal(pillModelName("lfm3:1.7b"), "lfm3:1.7b");
});

test("long names are cut in the middle to a fixed width, keeping the quant", () => {
  const out = pillModelName("LiquidAI-LFM3-1.7B-Instruct-GGUF-Q4_K_M");
  assert.equal(out.length, 28);
  assert.ok(out.startsWith("LiquidAI-LFM"));
  assert.ok(out.endsWith("Q4_K_M"));
  assert.ok(out.includes("…"));
});
