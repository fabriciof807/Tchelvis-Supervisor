import { test } from "node:test";
import assert from "node:assert/strict";
import * as base from "gpui-base";
import { chatElapsedLabel } from "../../assets/herdr-app/mods/tchelvis/main.js";

test("elapsed label shows whole seconds", () => {
  assert.equal(chatElapsedLabel(0, 6400), "◌ Gerando… 6s");
  assert.equal(chatElapsedLabel(0, 400), "◌ Gerando…");
  assert.equal(chatElapsedLabel(1000, 1999), "◌ Gerando…");
  assert.equal(chatElapsedLabel(1000, 2000), "◌ Gerando… 1s");
  assert.equal(chatElapsedLabel(5000, 3000), "◌ Gerando…");
});

test("mod imports without ScrollHandle and TextView", () => {
  assert.equal(base.ScrollHandle, undefined);
  assert.equal(base.TextView, undefined);
  assert.equal(typeof chatElapsedLabel, "function");
});
