import { test } from "node:test";
import assert from "node:assert/strict";
import {
  fallbackPill,
  fallbackSection,
  primaryRowBadge,
  fallbackModelLabel,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("pill only when fallback is active", () => {
  assert.equal(fallbackPill({ fallback_active: false }), null);
  assert.deepEqual(
    fallbackPill({ fallback_active: true, fallback_model: "openai/gpt-4.1-mini" }),
    { text: "● LLM: openai/gpt-4.1-mini (Fallback)", tone: "working" }
  );
  const hex = "c".repeat(64);
  assert.equal(
    fallbackPill(
      { fallback_active: true, fallback_model: `/models/${hex}` },
      [{ display_name: "LFM3-1.7B-Q4_K_M", path: `/blobs/${hex}` }]
    ).text,
    "● LLM: LFM3-1.7B-Q4_K_M (Fallback)"
  );
});

test("section states", () => {
  assert.equal(fallbackSection({ fallback_key_present: false }).state, "no_key");
  assert.equal(
    fallbackSection({
      fallback_key_present: true,
      fallback_host: "",
      fallback_model: "",
    }).state,
    "needs_endpoint"
  );
  assert.deepEqual(
    fallbackSection({
      fallback_key_present: true,
      fallback_configured: true,
      fallback_host: "h",
      fallback_model: "m",
    }),
    { state: "standby", label: "Em espera" }
  );
  const a = fallbackSection({
    fallback_key_present: true,
    fallback_configured: true,
    fallback_host: "h",
    fallback_model: "m",
    fallback_active: true,
    fallback_reason: "health",
    fallback_since: 0,
  });
  assert.equal(a.state, "active");
  assert.match(a.label, /^ATIVO — health falhou \d\d:\d\d$/);
});

test("primary in-use row reads falhou while fallback is active", () => {
  assert.equal(
    primaryRowBadge({ in_use: true }, { fallback_active: true }),
    "falhou"
  );
  assert.notEqual(
    primaryRowBadge({ in_use: true }, { fallback_active: false }),
    "falhou"
  );
});

test("fallback model label names llama-server blob paths from the local scan", () => {
  const hex = "3793e1e6e2266919d6e015287221b4110012345678901234567890abcdef1234";
  const local = [
    { display_name: "LFM3-1.7B-Q4_K_M", path: `/home/user/.cache/huggingface/hub/models--x/blobs/${hex}` },
    { display_name: "qwen3:1.7b", path: "/home/user/.ollama/models/blobs/sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
  ];
  assert.equal(fallbackModelLabel(`/models/${hex}`, local), "LFM3-1.7B-Q4_K_M");
  assert.equal(
    fallbackModelLabel(`/models/sha256-${"a".repeat(64)}`, local),
    "qwen3:1.7b"
  );
  // Unknown blob: short, recognizable label instead of the raw path.
  assert.equal(fallbackModelLabel(`/models/${"b".repeat(64)}`, local), "modelo sem nome · bbbbbbbb");
  // A file path keeps just the file name.
  assert.equal(fallbackModelLabel("/models/LFM2.5-350M-Q4_K_M.gguf", []), "LFM2.5-350M-Q4_K_M.gguf");
  // Real API ids stay as they are.
  assert.equal(fallbackModelLabel("deepseek/deepseek-chat", local), "deepseek/deepseek-chat");
  assert.equal(fallbackModelLabel("", local), "");
});
