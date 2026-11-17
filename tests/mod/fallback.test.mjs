import { test } from "node:test";
import assert from "node:assert/strict";
import {
  fallbackPill,
  fallbackSection,
  primaryRowBadge,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("pill only when fallback is active", () => {
  assert.equal(fallbackPill({ fallback_active: false }), null);
  assert.deepEqual(
    fallbackPill({ fallback_active: true, fallback_model: "openai/gpt-4.1-mini" }),
    { text: "● LLM: openai/gpt-4.1-mini (Fallback)", tone: "working" }
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
