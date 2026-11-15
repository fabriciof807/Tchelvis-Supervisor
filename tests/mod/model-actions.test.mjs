import { test } from "node:test";
import assert from "node:assert/strict";
import { modelRowActions, deleteClick } from "../../assets/herdr-app/mods/tchelvis/main.js";

test("in_use_row_has_no_actions", () => {
  assert.deepEqual(
    modelRowActions({ id: "ollama:a", in_use: true, hidden: false }, "ollama:a"),
    { canHide: false, canUnhide: false, canDelete: false }
  );
  assert.deepEqual(
    modelRowActions({ id: "ollama:b", in_use: false, hidden: false }, "ollama:b"),
    { canHide: false, canUnhide: false, canDelete: false }
  );
  assert.deepEqual(
    modelRowActions({ id: "ollama:c", in_use: false, hidden: false }, "ollama:a"),
    { canHide: true, canUnhide: false, canDelete: true }
  );
});

test("hidden_row_can_unhide", () => {
  assert.deepEqual(
    modelRowActions({ id: "file:/m/a.gguf", in_use: false, hidden: true }, "ollama:other"),
    { canHide: false, canUnhide: true, canDelete: true }
  );
});

test("delete_needs_second_click", () => {
  const step1 = deleteClick(null, "file:/m/a.gguf", 1000);
  assert.equal(step1.confirmed, false);
  assert.deepEqual(step1.pending, { id: "file:/m/a.gguf", at: 1000 });

  const step2 = deleteClick(step1.pending, "file:/m/a.gguf", 2000);
  assert.equal(step2.confirmed, true);
  assert.equal(step2.pending, null);
});

test("delete_expires", () => {
  const step1 = deleteClick(null, "file:/m/a.gguf", 1000);
  assert.equal(step1.confirmed, false);

  const step2 = deleteClick(step1.pending, "file:/m/a.gguf", 6000);
  assert.equal(step2.confirmed, false);
  assert.deepEqual(step2.pending, { id: "file:/m/a.gguf", at: 6000 });
});

test("delete_other_id_resets", () => {
  const step1 = deleteClick(null, "file:/m/a.gguf", 1000);
  const step2 = deleteClick(step1.pending, "file:/m/b.gguf", 1500);
  assert.equal(step2.confirmed, false);
  assert.deepEqual(step2.pending, { id: "file:/m/b.gguf", at: 1500 });
});
