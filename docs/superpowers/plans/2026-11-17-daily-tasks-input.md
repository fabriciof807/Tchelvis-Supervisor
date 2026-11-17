# Daily Tasks — Long Text and Tag Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Daily tasks accept multi-line text (inline Shift+Enter, or `$EDITOR` via `tchelvis task add --edit`), render whole in the list, and get their tag from a picker whose pill always matches the saved tag.

**Architecture:** Rust side: pure helpers in `src/home.rs` (editor resolution, editor round-trip, table title) used by the `task add` arm in `src/main.rs`. Mod side: pure exported helpers in `main.js` (tested under Node with the existing stubs) plus render changes in the "TAREFAS DO DIA" card.

**Tech Stack:** Rust (clap 4 derive, anyhow, tempfile in dev), Herdr Desktop mod JS (GPUI kit), `node:test`.

**Spec:** `docs/superpowers/specs/2026-11-16-daily-tasks-input-design.md`

## Global Constraints

- All tasks are general; the project is only a visual tag. No filtering by project.
- Tag choices = `"Geral"` first, then the Home project names (deduplicated), same as `projectChoices` at `main.js:3422`.
- `Geral` means "no project": never pass `--project Geral`.
- `Shift+Enter` inserts `\n`; `Enter` saves; `Esc` leaves. Placeholder mentions `Shift+Enter: nova linha`.
- Editor order: `$VISUAL` → `$EDITOR` → `nvi` if on `PATH` → `vi`. Empty saved text = cancelled, nothing added (exit 0, message `Cancelado: texto vazio`).
- `title` positional is optional **only** with `--edit`; with `--edit` it pre-fills the editor.
- `add_task` trims only the ends of the title; inner newlines are kept.
- `--json` outputs keep full titles. Human table shows first line + ` …(+N linhas)` where N = remaining line count.
- Non-goals: editing existing tasks, filter by project, kanban, markdown rendering.
- Validation commands (all must pass before handoff):
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked --quiet`,
  `node --check assets/herdr-app/mods/tchelvis/main.js`,
  `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs`.

## Review Focus

1. **Active Space that is not a Home project** (e.g. a scratch Space) — the pill must show `Geral` and the task saves with no project; covered in Task 3 (`effectiveTaskTag` tests).
2. **User picked a tag, then that project disappears from Home** — pill falls back to `Geral` rather than saving a stale tag the pill no longer lists; Task 3 test.
3. **`$EDITOR` with arguments** (`code -w`, `nvim -u NONE`) — must work; the editor string is run through `sh -c '<editor> "$1"'`; Task 1 test uses an editor string with an argument.
4. **Editor exits non-zero** (`:cq` in vim) — nothing added, error returned with the exit status; Task 1 test.
5. **Draft that is only whitespace/newlines** (Shift+Enter pressed twice) — `+ Add` and Enter do nothing, same as empty; Task 3 test on `isBlankDraft`.

---

### Task 1: Editor round-trip and table title helpers (`src/home.rs`)

**Files:**
- Modify: `src/home.rs` (new pub fns after `impl DailyTaskStore`, tests in existing `mod tests` at ~L509)

**Interfaces:**
- Produces:
  - `pub fn resolve_editor(visual: Option<&str>, editor: Option<&str>, nvi_available: bool) -> String` — first non-blank of visual/editor, else `"nvi"` if available, else `"vi"`.
  - `pub fn edit_text_with_editor(editor: &str, initial: &str, dir: &std::path::Path) -> anyhow::Result<Option<String>>` — writes `initial` to a temp file `tchelvis-task-<pid>-<nanos>.txt` in `dir`, runs `sh -c "<editor> \"$1\"" sh <file>` with inherited stdio, reads the file back, deletes it, returns `Ok(None)` if the trimmed text is empty, `Ok(Some(text_trimmed_at_ends))` otherwise; non-zero exit → `Err` mentioning the status (file still removed).
  - `pub fn table_title(title: &str) -> String` — first line; if more lines, append ` …(+N linhas)`.

- [ ] **Step 1: Write the failing tests** in `src/home.rs` `mod tests`:

```rust
#[test]
fn resolve_editor_prefers_visual_then_editor_then_nvi_then_vi() {
    assert_eq!(resolve_editor(Some("hx"), Some("nano"), true), "hx");
    assert_eq!(resolve_editor(Some("  "), Some("nano"), true), "nano");
    assert_eq!(resolve_editor(None, None, true), "nvi");
    assert_eq!(resolve_editor(None, None, false), "vi");
}

#[test]
fn edit_text_returns_saved_text_from_editor_with_args() {
    let dir = tempfile::tempdir().unwrap();
    // editor string carries an argument, like `code -w`
    let script = dir.path().join("ed.sh");
    std::fs::write(&script, "#!/bin/sh\n# $1 = flag, $2 = file\nprintf 'linha 1\\nlinha 2\\n' > \"$2\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let editor = format!("{} --wait", script.display());
    let out = edit_text_with_editor(&editor, "rascunho", dir.path()).unwrap();
    assert_eq!(out.as_deref(), Some("linha 1\nlinha 2"));
    // temp file cleaned up: only ed.sh remains
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn edit_text_prefills_initial_draft() {
    let dir = tempfile::tempdir().unwrap();
    // `true` leaves the file untouched → returns the draft
    assert_eq!(edit_text_with_editor("true", "rascunho\n", dir.path()).unwrap().as_deref(), Some("rascunho"));
}

#[test]
fn edit_text_empty_file_means_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(edit_text_with_editor("truncate -s0", "x", dir.path()).unwrap(), None);
}

#[test]
fn edit_text_nonzero_exit_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = edit_text_with_editor("false", "x", dir.path()).unwrap_err();
    assert!(err.to_string().contains("editor"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn add_task_keeps_inner_newlines() {
    let mut store = DailyTaskStore::default(); // use the existing constructor the other tests use
    let t = store.add_task("  a\n\nb  \n", None, TaskPriority::Normal, 1);
    assert_eq!(t.title, "a\n\nb");
}

#[test]
fn table_title_shows_first_line_and_count() {
    assert_eq!(table_title("só uma"), "só uma");
    assert_eq!(table_title("a\nb\nc"), "a …(+2 linhas)");
}
```

(Use whatever constructor the existing `mod tests` uses for `DailyTaskStore`.)

- [ ] **Step 2: Run** `cargo test --lib home::tests` — Expected: FAIL (functions not defined).
- [ ] **Step 3: Implement** the three functions with the signatures above. Use `std::process::Command::new("sh").args(["-c", &format!("{editor} \"$1\""), "sh"]).arg(&file)`. Remove the temp file on every path (success, empty, error).
- [ ] **Step 4: Run** `cargo test --lib home::tests` — Expected: PASS.
- [ ] **Step 5: Commit** `feat(task): editor round-trip and multi-line table title helpers`.

### Task 2: `task add --edit` and human list (`src/main.rs`)

**Files:**
- Modify: `src/main.rs:196-208` (`TaskCommand::Add`), `src/main.rs:~920-955` (Add arm), `src/main.rs:1265-1290` (`print_human_tasks`)
- Test: new `#[cfg(test)] mod tests` at the end of `src/main.rs`

**Interfaces:**
- Consumes: `resolve_editor`, `edit_text_with_editor`, `table_title` from Task 1.
- Produces: CLI `tchelvis task add --edit [--project P] [--priority p] [--json] [initial text...]` (used by the mod in Task 3).

- [ ] **Step 1: Write the failing tests** (clap parsing via `Cli::try_parse_from`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn task_add_requires_title_without_edit() {
        assert!(Cli::try_parse_from(["tchelvis", "task", "add"]).is_err());
    }

    #[test]
    fn task_add_edit_makes_title_optional() {
        let cli = Cli::try_parse_from(["tchelvis", "task", "add", "--edit"]).unwrap();
        // match into Commands::Task { action: TaskCommand::Add { edit: true, title, .. } } and assert title.is_empty()
    }

    #[test]
    fn task_add_edit_accepts_prefill_text() {
        let cli = Cli::try_parse_from(["tchelvis", "task", "add", "--edit", "--", "rascunho", "longo"]).unwrap();
        // assert edit == true and title == ["rascunho", "longo"]
    }
}
```

- [ ] **Step 2: Run** `cargo test --bin tchelvis` — Expected: FAIL (no `edit` field / title required).
- [ ] **Step 3: Implement.**
  - Add `#[arg(long)] edit: bool` and change title to `#[arg(required_unless_present = "edit")] title: Vec<String>`.
  - Add arm: if `edit`, resolve editor from `VISUAL`/`EDITOR` env and `nvi` on `PATH` (check `PATH` entries for an executable `nvi`; no new crate), call `edit_text_with_editor(&editor, &title.join(" "), &std::env::temp_dir())` **outside** `spawn_blocking` is fine (it is interactive and blocks on the tty — use `spawn_blocking` like the rest for consistency). `None` → print `Cancelado: texto vazio` and return `Ok(())` without touching the store. `Some(text)` → same add path as today. Without `--edit`, behaviour unchanged.
  - `print_human_tasks`: use `table_title(&t.title)` instead of `t.title`.
- [ ] **Step 4: Run** `cargo test --locked --quiet` — Expected: PASS. Manual smoke: `EDITOR="sh -c 'printf \"a\nb\" > \$1' sh" cargo run -q -- task add --edit --json` adds a task whose `title` is `"a\nb"`; `cargo run -q -- task list` shows `a …(+1 linhas)`. Remove the smoke task afterwards (`task remove <id>`), using a temp `HOME`/config dir if the CLI supports one, so the real task list is untouched.
- [ ] **Step 5: Commit** `feat(task): add --edit opens $EDITOR; human list shows first line of multi-line titles`.

### Task 3: Mod — pure helpers (tag, draft, editor argv)

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` (new exported functions near `pillModelName`, ~L437)
- Test: create `tests/mod/daily-tasks.test.mjs`

**Interfaces:**
- Produces (all exported):
  - `effectiveTaskTag(chosen: string, activeSpaceName: string|undefined, choices: string[]) -> string` — `chosen` if it is in `choices`; else `activeSpaceName` if in `choices`; else `"Geral"`.
  - `taskProjectArgs(tag: string) -> string[]` — `[]` for `"Geral"`/empty, else `["--project", tag]`.
  - `isBlankDraft(draft: string) -> boolean` — true when `draft.trim()` is empty.
  - `draftDisplayLines(draft: string, maxLines = 6) -> string[]` — `draft.split("\n")`; if more than `maxLines`, return `["…", ...last (maxLines-1) lines]`.
  - `taskEditorArgv(draft: string, priority: string, tag: string) -> string[]` — `["tchelvis","task","add","--edit","--priority",priority, ...taskProjectArgs(tag), "--", draft]`; omit `"--", draft` when `isBlankDraft(draft)`.
  - `hasNewTask(prevIds: string[], tasks: {id:string}[]) -> boolean` — some task id not in `prevIds`.

- [ ] **Step 1: Write the failing tests** in `tests/mod/daily-tasks.test.mjs`:

```js
import { test } from "node:test";
import assert from "node:assert/strict";
import { effectiveTaskTag, taskProjectArgs, isBlankDraft, draftDisplayLines,
         taskEditorArgv, hasNewTask } from "../../assets/herdr-app/mods/tchelvis/main.js";

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
  assert.deepEqual(draftDisplayLines("1\n2\n3\n4\n5\n6\n7\n8"), ["…", "4", "5", "6", "7", "8"]);
});
test("editor argv carries priority, tag and draft", () => {
  assert.deepEqual(taskEditorArgv("x\ny", "high", "site"),
    ["tchelvis","task","add","--edit","--priority","high","--project","site","--","x\ny"]);
  assert.deepEqual(taskEditorArgv(" ", "normal", "Geral"),
    ["tchelvis","task","add","--edit","--priority","normal"]);
});
test("hasNewTask detects an added id", () => {
  assert.equal(hasNewTask(["t-1"], [{ id: "t-1" }]), false);
  assert.equal(hasNewTask(["t-1"], [{ id: "t-1" }, { id: "t-2" }]), true);
});
```

- [ ] **Step 2: Run** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/daily-tasks.test.mjs` — Expected: FAIL (exports missing).
- [ ] **Step 3: Implement** the six exported functions in `main.js`.
- [ ] **Step 4: Run** the same command — Expected: PASS.
- [ ] **Step 5: Commit** `feat(mod): pure helpers for task tag, draft and editor argv`.

### Task 4: Mod — card UI (wrap, Shift+Enter, `✎`, tag menu, pill fix)

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` — `addTaskGui` (~L1985), `handleKeyDown` (~L2015), `fetchHomeSnapshot` (~L1823), card render (~L3420-3800)

**Interfaces:**
- Consumes: all Task 3 helpers; CLI from Task 2.

- [ ] **Step 1: Pill/tag fix.** Replace `currentProjectTag` (L3427) with `effectiveTaskTag(this.taskProject, activeSpace?.name, projectChoices)`. In `addTaskGui`, the render must hand the same value over: store it as `this.currentTaskTag` during render (or recompute with the same inputs) and build args with `taskProjectArgs(this.currentTaskTag)`. Use `isBlankDraft` for the empty check. `+ Tarefa` on a project card (L3898) keeps setting `this.taskProject = proj.name`.
- [ ] **Step 2: Shift+Enter.** In `handleKeyDown`, when `editingField === "task"` and `e.key === "enter"` with `m?.shift`, append `"\n"` instead of saving. Other fields unchanged.
- [ ] **Step 3: Inline field wraps.** The input div renders `draftDisplayLines(this.taskDraft)` as a `v_flex` of line `div`s (caret `▏` appended to the last line while editing) — no `ellipsize`. Placeholder: `Nova tarefa (Shift+Enter: nova linha)…`. Align the row `items_start` so `+ Add` stays at the top.
- [ ] **Step 4: `✎` button.** Next to `+ Add` (id `ck-task-edit`, label `✎`): `run("tab.new", { space: activeSpace?.id, name: "nova tarefa", argv: taskEditorArgv(this.taskDraft, this.taskPriority || "normal", this.currentTaskTag) })`; set `this.taskEditorPrevIds = tasks.map(t => t.id)`. In `fetchHomeSnapshot` success (the home role already polls every 8 s, L1274), if `this.taskEditorPrevIds && hasNewTask(this.taskEditorPrevIds, parsed.tasks ?? [])` → clear `taskDraft`, set `taskEditorPrevIds = null`. Do this in `runTaskCommand` success too.
- [ ] **Step 5: Tag menu.** Clicking the `📁 <tag>` pill (`ck-task-project-cycle` → rename id `ck-task-project`) toggles `this.taskTagMenuOpen`. When open, render below the pill row an `h_flex().flex_wrap()` of `actionBtn`s, one per `projectChoices` entry (ids `ck-task-tag-<index>`), same pattern as the Uso `⋯` menu (`usageMenuOpen`, ~L5276); the current tag is highlighted (active variant). Picking sets `this.taskProject` and closes the menu. Pill text is the full tag, no `ellipsize`.
- [ ] **Step 6: Full text in rows.** Task title: replace `ellipsize(tk.title, 54)` with a `v_flex` of `String(tk.title).split("\n")` line divs (empty line → `" "` so it keeps height). Tag pill: `this.pillBadge(t, tk.project, t.accent)` without `ellipsize`. Row `items_start`; the tag+priority group may wrap (`flex_wrap`).
- [ ] **Step 7: Verify.** `node --check assets/herdr-app/mods/tchelvis/main.js` and `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` — Expected: PASS. Then the full Rust suite from Global Constraints.
- [ ] **Step 8: Commit** `feat(mod): multi-line task input, ✎ editor, tag menu; pill and saved tag always match`.

### Task 5: Docs

- [ ] Add a CHANGELOG `## [Unreleased]` entry (Added: `task add --edit`, multi-line tasks, tag menu; Fixed: pill vs saved tag). Mark the spec line in `ROADMAP.md` if it is listed. Commit `docs: changelog for daily tasks input`.

Manual host check (coordinator, after merge): wrap, Shift+Enter, `✎` opens the editor and the task appears within ~8 s, menu, pill/tag match in a non-project Space.
