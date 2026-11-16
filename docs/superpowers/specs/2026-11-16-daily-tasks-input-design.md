# Daily tasks — long text and tag picker (short design)

**Date:** 2026-11-16 · **Size:** small change · **Area:** `assets/herdr-app/mods/tchelvis/main.js`
(card "TAREFAS DO DIA"), `src/main.rs` (`task add`), `src/home.rs`

## Intent

The user writes both one-liners ("Verificar funcionalidade no app") and multi-line tasks. Today the
input shows at most 56 chars on one line, the list cuts titles at 54 chars and project tags at 14,
and choosing a project means clicking the `📁` pill until it cycles to the right one.

## Decisions (user)

- **All tasks are general.** One list, no per-project filter. The project is only a **visual tag**.
- Tag choices = the projects the Home already lists, plus **Geral** (no tag).
- Long text: **approach C** — better inline field **and** an external-editor button.
- Later (not now): a **kanban board mod** that reads these tasks and groups them by tag
  (recorded in `ROADMAP.md` as an idea).

## Approaches (for long text)

| | Trade-off | Outcome |
| :-- | :-- | :-- |
| A. Inline field grows and wraps; Shift+Enter = newline, Enter = save | Fast for short/medium; the hand-rolled field still has no cursor movement or paste | part of C |
| B. `✎` opens a terminal tab with `$EDITOR` via `tchelvis task add --edit` | Real editing (cursor, paste, undo); leaves the card | part of C |
| **C. A + B** | Common case inline, long/pasted text in the editor; B is little code (CLI already adds tasks) | **Chosen** |

## Design

1. **Inline field (A).** No `ellipsize` on the draft; the box wraps and grows with the text
   (my call: cap its height at ~6 lines, then it scrolls or keeps growing — builder's call within
   GPUI limits). `Shift+Enter` inserts `\n`; `Enter` saves; `Esc` leaves. Placeholder hints
   "Shift+Enter: nova linha".
2. **Editor (B).** `✎` button next to `+ Add` opens a new tab running
   `tchelvis task add --edit [--project <tag>] --priority <p>`. `--edit` makes `title` optional,
   opens `$VISUAL`/`$EDITOR` (fallback `nvi`/`vi`) on a temp file pre-filled with the current draft,
   and adds the task with the saved text; empty file = cancelled, nothing added. The mod refreshes the
   list afterwards (its existing refresh is enough if it already polls; otherwise refresh on tab close
   — builder's call). The draft is cleared once the task appears.
3. **Full text in the list.** Task titles and tags render whole and wrap; no `ellipsize` on
   `tk.title` or the tag. Newlines in titles are preserved. Long tags may wrap under the title.
4. **Tag picker.** Clicking `📁 <tag>` opens a small menu (same pattern as the Uso popover): **Geral**
   first, then the Home projects; picking one sets the tag for the next task. The `+ Tarefa` button
   on a project card keeps pre-selecting that project.
5. **Bug fix.** When nothing is chosen, the pill shows the active Space (`main.js:3427`) but the task
   is saved with no project (`main.js:1989`). The pill and the saved tag must always match. (My call:
   default to the active Space's project when it is one of the Home projects, otherwise Geral, and
   save exactly what the pill shows.)
6. **CLI human list** (`print_human_tasks`): show the first line of multi-line titles plus `…(+N
   linhas)` so the table stays aligned. `--json` returns the full title.

## Non-goals

- Filtering the list by project; editing an existing task's text; the kanban board.
- Rich text / markdown rendering of task bodies.

## Tests

- `task add --edit` with an injected editor command (a script that writes a fixed text): adds the
  task with that text; empty file → nothing added; `title` optional only with `--edit`.
- `add_task` preserves inner newlines (trims only the ends).
- `print_human_tasks` multi-line formatting.
- Mod: `node --check`; manual check on the host (wrap, Shift+Enter, `✎`, picker, pill/tag match).
