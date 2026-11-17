import { test } from "node:test";
import assert from "node:assert/strict";
import {
  effectiveTaskTag,
  taskProjectArgs,
  isBlankDraft,
  draftDisplayLines,
  taskEditorArgv,
  hasNewTask,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

const choices = ["Geral", "cockpit", "site"];

test("pill defaults to the active Space only when it is a Home project", () => {
  assert.equal(effectiveTaskTag("", "cockpit", choices), "cockpit");
  assert.equal(effectiveTaskTag("", "scratch", choices), "Geral");
  assert.equal(effectiveTaskTag("", undefined, choices), "Geral");
});

test("explicit choice wins; a vanished project falls back", () => {
  assert.equal(effectiveTaskTag("site", "cockpit", choices), "site");
  assert.equal(effectiveTaskTag("gone", "scratch", choices), "Geral");
  assert.equal(effectiveTaskTag("Geral", "cockpit", choices), "Geral");
});

test("Geral never becomes --project", () => {
  assert.deepEqual(taskProjectArgs("Geral"), []);
  assert.deepEqual(taskProjectArgs(""), []);
  assert.deepEqual(taskProjectArgs("site"), ["--project", "site"]);
});

test("whitespace/newline-only draft is blank", () => {
  assert.equal(isBlankDraft("\n \n"), true);
  assert.equal(isBlankDraft("a\n"), false);
});

test("draft display keeps the last lines under a cap", () => {
  assert.deepEqual(draftDisplayLines("a\nb"), ["a", "b"]);
  assert.deepEqual(draftDisplayLines("1\n2\n3\n4\n5\n6\n7\n8"), [
    "…",
    "4",
    "5",
    "6",
    "7",
    "8",
  ]);
});

test("editor argv carries priority, tag and draft", () => {
  assert.deepEqual(taskEditorArgv("x\ny", "high", "site"), [
    "tchelvis",
    "task",
    "add",
    "--edit",
    "--priority",
    "high",
    "--project",
    "site",
    "--",
    "x\ny",
  ]);
  assert.deepEqual(taskEditorArgv(" ", "normal", "Geral"), [
    "tchelvis",
    "task",
    "add",
    "--edit",
    "--priority",
    "normal",
  ]);
});

test("hasNewTask detects an added id", () => {
  assert.equal(hasNewTask(["t-1"], [{ id: "t-1" }]), false);
  assert.equal(hasNewTask(["t-1"], [{ id: "t-1" }, { id: "t-2" }]), true);
});
