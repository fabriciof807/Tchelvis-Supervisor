import { test } from "node:test";
import assert from "node:assert/strict";
import {
  applyTextKey,
  textWithCaret,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("inserts at the cursor", () => {
  assert.deepEqual(applyTextKey("ac", 1, "x", { keyChar: "b" }), {
    text: "abc",
    cursor: 2,
    handled: true,
  });
});

test("left right clamp at the edges", () => {
  assert.deepEqual(applyTextKey("abc", 0, "left"), {
    text: "abc",
    cursor: 0,
    handled: true,
  });
  assert.deepEqual(applyTextKey("abc", 2, "left"), {
    text: "abc",
    cursor: 1,
    handled: true,
  });
  assert.deepEqual(applyTextKey("abc", 3, "right"), {
    text: "abc",
    cursor: 3,
    handled: true,
  });
  assert.deepEqual(applyTextKey("abc", 1, "right"), {
    text: "abc",
    cursor: 2,
    handled: true,
  });
});

test("backspace and delete around the cursor", () => {
  assert.deepEqual(applyTextKey("abc", 1, "backspace"), {
    text: "bc",
    cursor: 0,
    handled: true,
  });
  assert.deepEqual(applyTextKey("abc", 1, "delete"), {
    text: "ac",
    cursor: 1,
    handled: true,
  });
  assert.deepEqual(applyTextKey("abc", 0, "backspace"), {
    text: "abc",
    cursor: 0,
    handled: true,
  });
  assert.deepEqual(applyTextKey("abc", 3, "delete"), {
    text: "abc",
    cursor: 3,
    handled: true,
  });
});

test("home and end use the current line in multiline", () => {
  assert.deepEqual(applyTextKey("um\ndois", 5, "home", { multiline: true }), {
    text: "um\ndois",
    cursor: 3,
    handled: true,
  });
  assert.deepEqual(applyTextKey("um\ndois", 5, "end", { multiline: true }), {
    text: "um\ndois",
    cursor: 7,
    handled: true,
  });
  assert.deepEqual(applyTextKey("um\ndois", 5, "home", { multiline: false }), {
    text: "um\ndois",
    cursor: 0,
    handled: true,
  });
  assert.deepEqual(applyTextKey("um\ndois", 5, "end", { multiline: false }), {
    text: "um\ndois",
    cursor: 7,
    handled: true,
  });
});

test("shift enter inserts a newline only in multiline", () => {
  assert.deepEqual(
    applyTextKey("ab", 1, "enter", { shift: true, multiline: true }),
    {
      text: "a\nb",
      cursor: 2,
      handled: true,
    }
  );
  assert.deepEqual(
    applyTextKey("ab", 1, "enter", { shift: true, multiline: false }),
    {
      text: "ab",
      cursor: 1,
      handled: false,
    }
  );
});

test("unicode characters move and delete as one", () => {
  assert.deepEqual(applyTextKey("ação 👍", null, "backspace"), {
    text: "ação ",
    cursor: 5,
    handled: true,
  });
  assert.deepEqual(applyTextKey("ação", 3, "backspace"), {
    text: "aço",
    cursor: 2,
    handled: true,
  });
  assert.deepEqual(applyTextKey("ação 👍", 6, "left"), {
    text: "ação 👍",
    cursor: 5,
    handled: true,
  });
});

test("null or out-of-range cursor is clamped to the end", () => {
  assert.equal(textWithCaret("abc", 99), "abc▏");
  assert.equal(textWithCaret("abc", null), "abc▏");
  assert.equal(textWithCaret("abc", 1), "a▏bc");
  assert.equal(textWithCaret("", 5), "▏");
  assert.deepEqual(applyTextKey("", 5, "x", { keyChar: "a" }), {
    text: "a",
    cursor: 1,
    handled: true,
  });
});

test("unknown keys are not handled", () => {
  assert.deepEqual(applyTextKey("abc", 1, "up"), {
    text: "abc",
    cursor: 1,
    handled: false,
  });
});
