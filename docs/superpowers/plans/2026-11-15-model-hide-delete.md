# Hide and Delete Supervisor Models Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user hide a model from the supervisor model list and delete it from disk (with confirmation), from the CLI and the mod's model card.

**Architecture:** Hidden ids persist in `supervisor.toml` and are applied in `build_model_list`. Deletion is split into a pure planner (`model_delete.rs`: what to remove, whether it is protected) and an executor (filesystem / Ollama). The CLI asks for confirmation; the mod confirms with a second click and calls `rm --yes --json`.

**Tech Stack:** Rust (clap, serde, toml_edit, tempfile for tests), the mod in plain JS (`gpui-kit`/`herdr`), node:test with runtime stubs (`tests/mod/stub-hooks.mjs`).

**Spec:** `docs/superpowers/specs/2026-11-15-model-hide-delete-design.md`

## Global Constraints

- Config key: `[llm_container] hidden_models = ["<id>", …]` (array of model ids, default empty).
- CLI: `tchelvis supervisor model hide <id>`, `unhide <id>`, `rm <id> [--yes] [--json]`; `list --all` includes hidden models.
- `rm` refuses without confirmation; non-interactive stdin without `--yes` refuses.
- Protected (no hide, no delete): the effective model, `fallback_model_id`, and the running container's `tchelvis.model_id`. Hide only protects the effective model.
- Ollama models are removed only with `ollama rm <name>` or `DELETE {ollama_host}/api/delete` — never by deleting blobs.
- Never delete a directory except an HF-layout `models--…` repo dir with no snapshot files left.
- Model ids for files are `file:<canonical path>`; for HF-layout files that canonical path is the **blob**, not the snapshot symlink.

## Review Focus

- HF blob shared by two snapshot revisions: deleting one revision's model must keep the blob while another snapshot symlink still points to it. (Task 2 test `hf_shared_blob_kept`.)
- Sharded model (`x-00001-of-00003.gguf`): all shards go, in both flat and HF layouts. (Task 2 tests `flat_shards_all_removed`, `hf_shards_all_removed`.)
- Unknown or stale id (model already gone, typo): `rm` must fail with a clear "model not found" and touch nothing. (Task 3 test `rm_unknown_id_errors`.)
- Deleting a hidden model: the id must also leave `hidden_models`, or the list keeps a ghost count. (Task 3 test `rm_removes_hidden_entry`.)
- Mod double-click: the first click on Delete must never delete; only a second click within the window does. (Task 4 test `delete_needs_second_click`.)

---

### Task 1: Hide / unhide

**Files:**
- Modify: `src/supervisor/container.rs` (`LlmContainerConfig`: add `hidden_models: Vec<String>`, default empty; serde default)
- Modify: `src/supervisor/config_write.rs` (writer)
- Modify: `src/supervisor/models.rs` (`LocalModel.hidden`, `ModelListOutput.user_hidden_count`, `build_model_list`)
- Modify: `src/supervisor/mod.rs` (`list_models`, new `hide_model`, `unhide_model`)
- Modify: `src/main.rs` (`ModelCommand::Hide { id }`, `ModelCommand::Unhide { id }`, both print `ModelListOutput` JSON)

**Interfaces:**
- Produces: `config_write::write_hidden_models(path: &Path, ids: &[String]) -> Result<()>` (writes the array; removes the key when empty).
- Produces: `models::build_model_list(scanned, filter, effective, show_all, hidden: &[String]) -> (Vec<LocalModel>, usize, usize)` — returns `(list, filter_hidden_count, user_hidden_count)`. User-hidden rows are dropped unless `show_all`; with `show_all` they stay with `hidden: true`. The in-use model is never dropped.
- Produces: `supervisor::hide_model(id: &str) -> Result<ModelListOutput>`, `supervisor::unhide_model(id: &str) -> Result<ModelListOutput>` (idempotent; hide errors with "cannot hide the model in use" for the effective id).

- [ ] **Step 1: Write failing tests** in `models.rs` tests: `build_list_drops_user_hidden` (hidden id absent, `user_hidden_count == 1`), `build_list_show_all_marks_hidden` (`show_all` keeps it with `hidden == true`), `build_list_never_hides_in_use`; in `config_write.rs`: `write_hidden_models_roundtrip` (write `["a","b"]`, `SupervisorConfig` parses them; write `[]` removes the key; other keys preserved).
- [ ] **Step 2: Run** `cargo test --quiet hidden` — expect compile failure / FAIL.
- [ ] **Step 3: Implement** the fields, writer, `build_model_list` change (update all callers), `hide_model`/`unhide_model`, CLI subcommands; `print_model_table` mentions `(N hidden by you)` when `user_hidden_count > 0`.
- [ ] **Step 4: Run** `cargo test --quiet` — all pass.
- [ ] **Step 5: Commit** `feat(supervisor): hide/unhide models in the model list`.

### Task 2: Delete planner (pure)

**Files:**
- Create: `src/supervisor/model_delete.rs` (register in `src/supervisor/mod.rs`)
- Modify: `src/supervisor/models.rs` (make the shard regex helper `pub(crate) fn shard_parts(file_name: &str) -> Option<(String, u32)>` returning base name and total shards)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Serialize, Debug, PartialEq)]
  #[serde(tag = "kind", rename_all = "snake_case")]
  pub enum DeletePlan {
      Ollama { name: String },
      Files { remove: Vec<PathBuf>, prune_dir: Option<PathBuf>, bytes: u64 },
  }
  pub fn plan_delete(model: &LocalModel) -> Result<DeletePlan>;
  pub fn protected_reason(id: &str, effective: Option<&str>, fallback: Option<&str>, running: Option<&str>) -> Option<String>;
  ```
- `plan_delete` rules:
  - `ollama:<name>` → `Ollama { name }`.
  - Path inside `<repo>/blobs/` where `<repo>` is named `models--*` (HF layout): walk `<repo>/snapshots/*/**`; collect symlinks whose canonical target is the model blob. If a matched symlink name is shard 1 of N, also collect sibling shard symlinks in the same directory. `remove` = those symlinks + each of their blobs not pointed to by any other symlink in `<repo>/snapshots`. `prune_dir = Some(<repo>)` when no other `.gguf` symlink remains under snapshots.
  - Any other path: the file, plus sibling shards when it is shard 1 of N. `prune_dir = None`.
  - `bytes` = sum of sizes of removed regular files (blobs/files, not symlinks).
- `protected_reason` returns `"in use by the supervisor"`, `"configured as fallback model"`, or `"loaded in the running container"`, else `None`.

- [ ] **Step 1: Write failing tests** with `tempfile::TempDir` building real files/symlinks: `flat_file_removed`, `flat_shards_all_removed` (3 shards → 3 paths), `hf_single_model_prunes_repo` (symlink + blob, `prune_dir == Some(repo)`), `hf_shared_blob_kept` (two revisions point to the same blob → blob not in `remove`, only one symlink), `hf_shards_all_removed`, `hf_other_model_keeps_repo` (`prune_dir == None` when another gguf remains), `ollama_plan`, `protected_reasons` (each of the three + `None`).
- [ ] **Step 2: Run** `cargo test --quiet model_delete` — FAIL.
- [ ] **Step 3: Implement** `plan_delete`, `protected_reason`, `shard_parts`.
- [ ] **Step 4: Run** `cargo test --quiet` — all pass.
- [ ] **Step 5: Commit** `feat(supervisor): model delete planner (HF layout, shards, protection)`.

### Task 3: Delete executor + `model rm`

**Files:**
- Modify: `src/supervisor/model_delete.rs` (executor)
- Modify: `src/supervisor/mod.rs` (`delete_model`)
- Modify: `src/main.rs` (`ModelCommand::Rm { id, yes, json }`)

**Interfaces:**
- Consumes: `plan_delete`, `protected_reason`, `write_hidden_models`, `ContainerManager::container_model_id`.
- Produces:
  ```rust
  pub trait OllamaRemover { fn remove(&self, name: &str) -> Result<()>; }
  pub struct SystemOllamaRemover { pub host: String } // `ollama rm` if on PATH, else HTTP DELETE {host}/api/delete {"model": name}
  pub fn execute_plan(plan: &DeletePlan, ollama: &dyn OllamaRemover) -> Result<()>;
  #[derive(Serialize)] pub struct DeleteOutcome { pub id: String, pub plan: DeletePlan, pub deleted: bool }
  pub fn supervisor::delete_model(id: &str, confirm: &mut dyn FnMut(&LocalModel, &DeletePlan) -> bool) -> Result<DeleteOutcome>;
  ```
- `delete_model`: scan with `show_all`, find by id (else error `model not found: <id>`), check `protected_reason` (error with the reason), build the plan, call `confirm` (false → `deleted: false`, nothing touched), execute, drop the id from `hidden_models` if present.
- `execute_plan` removes files (`remove_file`), then `remove_dir_all(prune_dir)` only if it still contains no `.gguf` under `snapshots`; errors carry the path.
- CLI `rm`: without `--yes`, if stdin is not a terminal → error `refusing to delete without --yes (stdin is not a terminal)`; else print source, every path, total size (`format_bytes`), and for Ollama the line `This also removes it from Ollama for every app.`, then read `y/N`. `--json` prints `DeleteOutcome`.

- [ ] **Step 1: Write failing tests** in `model_delete.rs`/`mod.rs` tests using a `FakeOllama` that records names and a temp HOME/model dir: `execute_flat_removes_file`, `execute_hf_prunes_repo`, `execute_ollama_calls_remover` (no filesystem change), `rm_unknown_id_errors`, `rm_protected_errors` (effective model), `rm_declined_touches_nothing`, `rm_removes_hidden_entry`. Make `delete_model` testable by an inner `delete_model_with(cfg_path, roots, effective/fallback/running ids, ollama, confirm)`; the public fn wires the real ones.
- [ ] **Step 2: Run** `cargo test --quiet rm_ execute_` — FAIL.
- [ ] **Step 3: Implement** executor, `delete_model`, CLI.
- [ ] **Step 4: Run** `cargo test --quiet` and a manual check: `HOME=$(mktemp -d)` with a synthetic GGUF in `model_dirs`, `tchelvis supervisor model rm file:<path>` then answer `n` (file stays) and `y` (file gone); `echo | tchelvis supervisor model rm …` refuses.
- [ ] **Step 5: Commit** `feat(supervisor): tchelvis supervisor model rm with confirmation`.

### Task 4: Mod card actions

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` (model picker rows near `ck-model-row-${idx}`; new methods `hideModel`, `unhideModel`, `deleteModel`)
- Test: `tests/mod/model-actions.test.mjs`

**Interfaces:**
- Consumes: CLI `model hide|unhide <id>` (prints `ModelListOutput`) and `model rm <id> --yes --json` (prints `DeleteOutcome`).
- Produces (exported, pure):
  ```js
  export function modelRowActions(row, selectedId) // -> { canHide, canUnhide, canDelete }
  export function deleteClick(pending, id, now, windowMs = 4000) // -> { pending: {id, at} | null, confirmed: boolean }
  ```
  `modelRowActions`: no actions for `row.in_use` or `row.id === selectedId`; `canUnhide` iff `row.hidden`; `canHide` iff not hidden. `deleteClick`: first click (or different id, or expired) → `{pending:{id,at:now}, confirmed:false}`; second click on the same id within `windowMs` → `{pending:null, confirmed:true}`.
- UI: each eligible row gets `Hide`/`Show` and `Delete`; while pending, Delete reads `Delete <size>?` (reuse the card's size formatter). Confirmed → `exec("tchelvis", ["supervisor","model","rm",id,"--yes","--json"], {timeout: 120000})`, then `fetchModelList`; errors go to `modelListError` (shown in the card). Hide/Show use `_runModelListCmd` with the hide/unhide args (keep `--all` view state).

- [ ] **Step 1: Write failing tests**: `in_use_row_has_no_actions`, `hidden_row_can_unhide`, `delete_needs_second_click` (first → not confirmed; second same id at +1000 ms → confirmed), `delete_expires` (second at +5000 ms → not confirmed, new pending), `delete_other_id_resets`.
- [ ] **Step 2: Run** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/` — FAIL.
- [ ] **Step 3: Implement** helpers and wire the buttons.
- [ ] **Step 4: Run** the node tests, `node --check assets/herdr-app/mods/tchelvis/main.js`, `cargo test --quiet` — all pass.
- [ ] **Step 5: Commit** `feat(mod): hide/show/delete actions in the supervisor model card`; update `HANDOFF.md` and `ROADMAP.md` (M1c → Implemented, host validation pending).
