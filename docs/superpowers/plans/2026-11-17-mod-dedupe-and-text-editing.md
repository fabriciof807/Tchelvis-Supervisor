# Mod Dedupe and Text Editing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep only the top-bar view switcher, and give every mod text field a real cursor (←/→, Home/End, Delete, insert/backspace at the cursor).

**Architecture:** Two pure exported helpers in `main.js` (`applyTextKey`, `textWithCaret`) plus a field table that maps `editingField` to its instance property, so `handleKeyDown` has one editing path; renderers draw the caret with `textWithCaret`.

**Tech Stack:** Herdr Desktop mod JS (GPUI), Node tests with `tests/mod/stub-hooks.mjs`.

**Spec:** `docs/superpowers/specs/2026-11-17-mod-dedupe-and-text-editing-design.md`

## Global Constraints

- Only `assets/herdr-app/mods/tchelvis/main.js` and `tests/mod/text-edit.test.mjs` change.
- Cursor unit: Unicode code points (`Array.from(text)`), never UTF-16 indexes.
- Key names: `left`, `right`, `home`, `end`, `delete`, `backspace`, `enter`, `escape`; typed text arrives in `e.key_char` (existing behaviour). Ignore `key_char` with `control`/`platform` modifiers (existing).
- Multiline fields: `task`, `chat`. Shift+Enter inserts `"\n"` at the cursor there; plain Enter keeps today's submit behaviour for every field.
- Existing user-visible behaviour of submit/escape/pending (`chatPending` blocks keys) is unchanged.

## Review Focus

- Cursor beyond the text after the draft is cleared by a submit → clamped to the end, no crash. Test in Task 2.
- Accented/emoji text (`"ação 👍"`) → left/backspace remove whole characters. Test in Task 2.
- Home/End on a multiline draft go to the current line's edges, not the whole text. Test in Task 2.
- Focusing a different field resets the cursor to that field's end. Covered by Task 2 implementation step (reset on every `editingField` assignment).
- Status bar still shows `Uso` and `CTRL-SHIFT-P commands` after removing the links. Task 1 manual check.

---

### Task 1: Single view switcher

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js`
  - `renderInspector(cx)` (~line 7000): the drawer header row with the `🏠 Home` / `🔍 Inspector` / `🩺 Doctor` / `💬 Chat` buttons (~lines 7180-7245) — remove those four buttons and their container if it becomes empty; keep the `↻` and `×` buttons (right-aligned).
  - `renderStatus(cx)` (~line 8486): remove `viewLink("ck-status-home" …)`, `"ck-status-inspector"`, `"ck-status-doctor"`, `"ck-status-chat"` (~lines 8628-8636) and the `·` separators between/after them; remove the `viewLink` helper if unused. Keep `Uso` and `CTRL-SHIFT-P commands`, with one `·` between them.
  - Do not touch `renderPill` (`ck-pill-view-*`, ~lines 3300-3335).

- [ ] **Step 1: Implement** the removals.
- [ ] **Step 2: Verify:** `node --check assets/herdr-app/mods/tchelvis/main.js`; `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` all pass; `grep -c 'ck-status-home\|ck-status-chat' assets/herdr-app/mods/tchelvis/main.js` → `0`; `grep -c 'ck-pill-view-' …` unchanged (4).
- [ ] **Step 3: Commit** `refactor(mod): keep only the top-bar view switcher`

---

### Task 2: Cursor-aware text editing for all fields

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` (exports near `isBlankDraft`, ~line 537; `handleKeyDown`, ~line 2418; caret sites at ~lines 4059/4070 (task), 5474/5487 (chat), 6050/6054 (fallbackHost), 6236/6241 (modelFilter), 7522/7523 (prompt), 7717/7718 (command), 8446/8449 (label); every `this.editingField = "<name>"` assignment)
- Create: `tests/mod/text-edit.test.mjs`

**Interfaces (exports):**
- `applyTextKey(text, cursor, key, { keyChar = null, shift = false, multiline = false } = {})` → `{ text, cursor, handled }`. `cursor` is clamped to `[0, len]` first (`null`/`undefined` → `len`). Keys: `left`/`right` ±1; `home`/`end` → start/end of the current line when `multiline`, else of the text; `backspace` deletes before the cursor; `delete` deletes at the cursor; `enter` with `shift && multiline` inserts `"\n"`; `keyChar` (when `key` is none of the above) inserts at the cursor. Anything else → `handled: false`, text/cursor unchanged.
- `textWithCaret(text, cursor, caret = "▏")` → the text with `caret` inserted at the clamped cursor (code points).
- `const TEXT_FIELDS = { task: "taskDraft", chat: "chatDraft", fallbackHost: "fallbackHostDraft", modelFilter: "modelFilterText", command: "commandDraft", prompt: "promptText", label: "labelValue" }` and `MULTILINE_FIELDS = new Set(["task", "chat"])`.

Instance state: `this.caret = null` (null = end of text). Reset `this.caret = null` wherever `this.editingField` is assigned (add a small `setEditingField(name)` method and use it at every assignment site). `handleKeyDown`: keep the existing `escape` and plain `enter` (submit) branches; for every other key, if `TEXT_FIELDS[this.editingField]` exists, call `applyTextKey` on `this[prop]` with `this.caret`, write back `this[prop]` and `this.caret`, and stop propagation only when `handled`. Renderers: replace `value + (isEditing ? "▏" : "")` and the multiline last-line caret with `textWithCaret(value, this.caret)` (only when editing; split into lines after inserting the caret so it lands on the right line).

- [ ] **Step 1: Write the failing tests** in `tests/mod/text-edit.test.mjs`:
  - `inserts at the cursor` — `applyTextKey("ac", 1, "x", {keyChar: "b"})` → `{text:"abc", cursor:2, handled:true}`.
  - `left right clamp at the edges` — from 0 `left` stays 0; from len `right` stays len.
  - `backspace and delete around the cursor` — `("abc",1,"backspace")` → `"bc"`,0; `("abc",1,"delete")` → `"ac"`,1; backspace at 0 and delete at len are no-ops with `handled:true`.
  - `home and end use the current line in multiline` — text `"um\ndois"`, cursor 5 (inside `dois`): `home` → 3, `end` → 7; with `multiline:false`: `home` → 0, `end` → 7.
  - `shift enter inserts a newline only in multiline` — `("ab",1,"enter",{shift:true,multiline:true})` → `"a\nb"`,2; without `multiline` → `handled:false`.
  - `unicode characters move and delete as one` — `("ação 👍", null, "backspace")` → `"ação "`; `("ação", 3, "backspace")` → `"aço"`.
  - `null or out-of-range cursor is clamped to the end` — `textWithCaret("abc", 99)` → `"abc▏"`; `textWithCaret("abc", null)` → `"abc▏"`; `textWithCaret("abc", 1)` → `"a▏bc"`.
  - `unknown keys are not handled` — `("abc",1,"up")` → `handled:false`, unchanged.
- [ ] **Step 2: Run** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/text-edit.test.mjs` — Expected: FAIL (exports missing).
- [ ] **Step 3: Implement** the exports, `TEXT_FIELDS`/`MULTILINE_FIELDS`, `setEditingField`, the `handleKeyDown` rewrite and the caret rendering at every site listed above.
- [ ] **Step 4: Verify:** `node --check assets/herdr-app/mods/tchelvis/main.js`; `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` — all pass; `grep -n '+= e.key_char\|slice(0, -1)' assets/herdr-app/mods/tchelvis/main.js` → no matches in `handleKeyDown`.
- [ ] **Step 5: Commit** `feat(mod): cursor-aware editing (arrows, Home/End, Delete) in every text field`

---

## Coordinator host checklist (after the nightly)

Drawer has no tab row (only ↻ ×); status bar shows only `Uso · CTRL-SHIFT-P commands`; in the chat and the Daily Tasks input: ←/→ move the caret, Home/End go to the line edges, Delete/backspace act at the caret, typing inserts mid-text, Shift+Enter breaks the line at the caret; same for the fallback host, model filter, prompt, command and label fields.
