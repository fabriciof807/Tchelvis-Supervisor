# Tickets and Loop Coordinator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a general `to-tickets` skill and teach `/orchestration-tchelvis` to run those tickets with `tchelvis loop`.

**Architecture:** `to-tickets` is a Markdown asset embedded in the binary and installed by `tchelvis setup` in `~/.agents/skills` only, asking before it replaces a user's file. The coordinator is a new section in the existing orchestration skill. The loop engine does not change.

**Tech Stack:** Rust 2021, existing setup manifest (JSON), Markdown. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-24-tickets-coordinator-design.md` (Portuguese; it decides behaviour).

## Global Constraints

- No change under `src/loops/`; no new `tchelvis` command.
- Skill texts, code and docs in English; tickets are written in the language of the spec they come from.
- `to-tickets` names no model and does not require `tchelvis`.
- Skill name, verbatim: `to-tickets`. Ticket states, verbatim: `ready`, `running`, `merged`, `failed`.
- Ticket folders, verbatim: `.loops/tickets/<feature>/NN-<slug>.md` when the project has `.loops/`, otherwise `.tickets/<feature>/NN-<slug>.md`.
- Integration branch, verbatim: `integration/<feature>`, in its own git worktree. Never `main`, never push.
- Merge command, verbatim: `git -c core.hooksPath=/dev/null merge --no-ff <branch>`; undo a conflict with `git merge --abort`.
- `orchestration-tchelvis` keeps its current install behaviour. No existing test is weakened or deleted.
- Our own wording: ideas from Matt Pocock's MIT `to-tickets`, no copied sentences.
- Public repo: no personal paths, names or accounts. Tests use temp homes only.
- Validation per task: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`.

## Review Focus

- The user already has a `to-tickets` (theirs or Matt's): `setup` asks; "no", Enter, end of input or no terminal leave it byte-identical and say so. Task 2.
- The user edited ours: `setup` asks and `uninstall` leaves it. Task 2.
- `SKILL.md` or its folder is a symlink (including a dangling one): never written through, `setup` does not fail. Task 2.
- `setup-manifest.json` written by an older version (without the new field) still loads. Task 2.
- The integration worktree has no untracked `.loops/workflows` or `models.toml`, so a custom team is missing there: the coordinator stops and asks. Task 3.

---

### Task 1: The `to-tickets` skill text

**Files:**
- Create: `assets/skills/to-tickets/SKILL.md`
- Modify: `src/assets.rs` (`pub const TO_TICKETS_SKILL: &str = include_str!(...)`, `pub const TO_TICKETS_SKILL_NAME: &str = "to-tickets"`)
- Test: `src/assets.rs`

**Interfaces:**
- Produces: `TO_TICKETS_SKILL`, `TO_TICKETS_SKILL_NAME`.

- [ ] **Step 1: Failing test** `to_tickets_skill_states_its_contract`: starts with `---\nname: to-tickets\ndescription: `; contains each of `status: ready`, `blocked_by:`, `files:`, `workflow:`, `run_id:`, `.loops/tickets/<feature>/`, `.tickets/<feature>/`, `integration`, `open decision`, `demo`, `lockfile`, `expand`, `contract`, `approve`; contains none of `claude`, `gemini`, `gpt`, `haiku`, `opus`, `sonnet` (case-insensitive); at most 120 lines.
- [ ] **Step 2: Run** `cargo test --locked --lib to_tickets` → fails to compile.
- [ ] **Step 3: Write the skill.** Sections, in this order: when not to use it (the change fits one session or loop: say so and stop); read the spec and the code it touches; open decision → ask, never invent; slice rules; dependency rules; the final integration ticket; show the list and what in the spec has no ticket, and wait for approval; write the files; the ticket template. Rules and template are section 1 of the spec, verbatim in meaning. The template's frontmatter keys are exactly those of the spec. State that acceptance criteria must fail before the work, and that each names the command or observation that proves it. After writing, print one table: id, title, blocked by, files.
- [ ] **Step 4: Run** `cargo test --locked --lib to_tickets` → PASS.

### Task 2: Install in `~/.agents/skills`, asking before replacing

**Files:**
- Modify: `src/setup.rs` (manifest entry, install, uninstall, purge), `src/main.rs` (the question)
- Test: `src/setup.rs`

**Interfaces:**
- Consumes: Task 1 constants.
- Produces: `ManifestEntry.keep_if_modified: bool` with `#[serde(default)]`; `pub fn run_setup_with(paths: &CockpitPaths, opts: &SetupOptions, confirm: &mut dyn FnMut(&Path) -> bool) -> Result<SetupReport>`; `run_setup` keeps its signature and calls it with a `confirm` that always returns `false`. The link is recorded as `ManifestActionKind::CreatedFile` with `keep_if_modified` and the SHA-256 of its target path as `post_sha256`.

Only under the `skills` target. File: `~/.agents/skills/to-tickets/SKILL.md`, mode `0o644`, folders created if missing. First match wins:

| Destination | Action | `actions` line |
|---|---|---|
| file, or its folder, is a symlink | leave | `skip <path> (symlink; not managed by tchelvis)` |
| absent | write, record | `write <path>` |
| same content as ours | record if missing | `up-to-date <path>` |
| hash equals the recorded `post_sha256` | write, record | `write <path>` |
| anything else | dry run: `would ask before replacing <path>`; else `confirm(path)`: true → write, record; false → leave | `write <path>` / `skip <path> (your own file; not replaced)` |

Link: `~/.claude/skills/to-tickets` → `~/.agents/skills/to-tickets`, only when `~/.claude` exists, the file above is ours, and `symlink_metadata` finds nothing at the link path. Otherwise leave what is there without asking.

`main.rs`: with a terminal on stdin, `confirm` prints to stderr `'<path>' já existe e não é do tchelvis. Sobrescrever? [y/N]: ` and accepts `y`, `yes`, `s`, `sim`; end of input, a read error or no terminal → `false`.

Uninstall of an entry with `keep_if_modified`: remove a regular file only if its hash equals `post_sha256`, a link only if its target is ours; otherwise `keep <path> (modified)`. Purge fallback: remove the file only if its content is the embedded one, the link only if its target is ours.

- [ ] **Step 1: Failing tests**
  - `setup_installs_to_tickets_only_in_agents_and_uninstall_removes_it`: file and Claude link exist; nothing named `to-tickets` under `.config/opencode`, `.pi`, `.codex`, `.gemini`; after uninstall both are gone.
  - `setup_asks_before_replacing_a_foreign_to_tickets`: `# mine\n`; `confirm` false → byte-identical, not recorded, report has `your own file; not replaced`, and uninstall leaves it; `confirm` true → ours, recorded.
  - `setup_updates_its_own_older_to_tickets_without_asking` (`confirm` panics if called).
  - `an_edited_to_tickets_asks_on_setup_and_survives_uninstall`.
  - `a_symlinked_to_tickets_is_left_alone`: `SKILL.md` link, folder link, dangling folder link; `Ok`, targets unchanged, links still there, `confirm` not called.
  - `an_existing_claude_entry_is_left_alone`: a folder, a file and a foreign link at `~/.claude/skills/to-tickets`.
  - `purge_without_a_manifest_removes_only_our_to_tickets`.
  - `a_manifest_without_the_new_field_loads`: JSON literal without `keep_if_modified` → `Ok`, value `false`.
  - `dry_run_writes_no_to_tickets_and_never_asks`.
- [ ] **Step 2: Run** `cargo test --locked --lib setup` → the new ones fail.
- [ ] **Step 3: Implement** per the table. Use `symlink_metadata`, never `exists()`, to decide.
- [ ] **Step 4: Run** `cargo test --locked` → PASS, existing setup tests unchanged.

### Task 3: Coordinator section

**Files:**
- Modify: `src/orch/preamble.rs` (new section at the end of `orchestration_skill_markdown`, title `## Running Tickets with Loops`)
- Test: `src/orch/preamble.rs`

- [ ] **Step 1: Failing test** `orchestration_skill_documents_running_tickets`. The text from `## Running Tickets with Loops` to the end contains each of: `integration/<feature>`, `git worktree add`, `--input-file`, `--detach`, `loop_<RUN_ID>`, `git -c core.hooksPath=/dev/null merge --no-ff`, `git merge --abort`, `status: running`, `status: merged`, `status: failed`, `end your turn`, `tchelvis loop list --active`, `max_concurrent`, `ARQUIVOS`, `never push`, `fast-worker-reviewer`, `worker-reviewer-critic`, `planner-worker-tester-critic`, `designer-worker-reviewer`, `tchelvis orch launch`; and does not contain `sleep` or `--yes`.
- [ ] **Step 2: Run** `cargo test --locked --lib preamble` → fails.
- [ ] **Step 3: Write the section** (at most 70 lines) from section 2 of the spec, plus these decisions:
  - Write `status: running` **before** launching and `run_id` right after, so a lost session never launches twice; `running` without `run_id` on resume → check `tchelvis loop list --active`; if unclear, ask the user.
  - Launch from inside the worktree, with the ticket's absolute path. Before the first launch run `tchelvis loop list` and `tchelvis loop check <workflow>` there; a missing team or a pending model confirmation → stop and ask. Never edit `loops.toml`.
  - A launch refused by the concurrency limit: put the ticket back to `ready` and wait for the next wake-up.
  - Never run the project's build or tests; the last ticket does it in the sandbox. Never read diffs or implementation files; never edit code.
  - Use the language of the user in reports; a failure report has at most 6 lines.
- [ ] **Step 4: Run** `cargo test --locked --lib preamble` → PASS; run `cargo test --locked`.

### Task 4: Documentation

**Files:**
- Modify: `docs/TCHELVIS-ORCH.md`, `README.md`, `CHANGELOG.md` (Unreleased / Added), `docs/agent-install.md` if it lists installed skills

- [ ] **Step 1:** Guide: one section "From a spec to merged branches" (spec → `/to-tickets` → `/orchestration-tchelvis`), with the limits from the spec. README: one bullet in Use Case 5, and `to-tickets` in the sentence about the `skills` target: one copy in `~/.agents/skills`, linked for Claude Code, and `setup` asks before replacing the user's own. CHANGELOG: two entries; credit the ideas to Matt Pocock's MIT `to-tickets`.
- [ ] **Step 2:** Check that every relative link resolves and every command appears in its `--help`. Run `cargo test --locked`.

### Task 5: Real rehearsal (coordinator, not the implementer)

- [ ] **Step 1:** Temp repository with a small spec leaving one decision open: `to-tickets` run by a cheap model must ask about it.
- [ ] **Step 2:** Close the decision: expect 2 tickets, one blocked by the other, plus integration, and an approval request before any file is written.
- [ ] **Step 3:** Run the coordinator text by hand with real loops in an integration worktree: both merges, integration ticket `PASS`, the user's checkout unchanged.
- [ ] **Step 4:** A ticket made to fail: nothing new starts, report of at most 6 lines.
- [ ] **Step 5:** Wording fixes found here go back through Tasks 1 and 3 with their tests.

The wake-up itself needs a real Herdr and goes to the user's validation script.
