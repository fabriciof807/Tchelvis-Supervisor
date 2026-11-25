# Loop LFS Checkout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** The loop workspace holds real Git LFS content when the repository uses the trusted `lfs` filter, read only from the local LFS store, and a missing object is reported before anything is cloned.

**Architecture:** The trust check keeps returning the filters to disable and now also the validated `lfs` commands. Preflight resolves the local LFS store and checks every pointer of `base_commit` against it, without running `git-lfs`. `create_in` passes the commands and the store to the `checkout` command only, through `GIT_CONFIG_*`.

**Tech Stack:** Rust, the `git` CLI. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-25-loop-lfs-checkout-design.md` (decides behaviour)

## Global Constraints

- Product code changes only in `src/loops/workspace.rs`; plus one line in `CHANGELOG.md`.
- No network access by the engine; `.lfsconfig` never chooses an endpoint.
- `GIT_LFS_COMMANDS` and the set of trusted, disabled and refused filters do not change. Existing tests stay unchanged and pass.
- `snapshot`, `restore`, `engine_git` and `publish` do not change.
- Preflight never runs `git-lfs` beyond what `git status` already runs today, and writes nothing.
- `lfs_enabled` is additive: `#[serde(default)]`, false.
- User-visible text in Portuguese, exact strings below. Code, comments and CHANGELOG in English.
- Checks: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`.
- Do not commit, push, or edit the spec, this plan or `HANDOFF.md`. Each task ends "ready to commit".

## External facts (checked; do not re-derive)

git-lfs v3.7.0 source, and run with git-lfs 3.4.1 and git 2.43.0:

| Fact | Source |
|---|---|
| Object path is `<storage>/objects/<oid[0:2]>/<oid[2:4]>/<oid>` | `fs/fs.go` `localObjectDir` |
| Storage is `lfs.storage`, default `lfs`; a relative value is joined to the **common** git dir (a linked worktree follows `commondir`). `.lfsconfig` cannot set it | `fs/fs.go` `New`, `resolveGitStorageDir`; `config/git_fetcher.go` `safeKeys`; run |
| Canonical pointer: `version https://git-lfs.github.com/spec/v1\n`, `oid sha256:<64 lower hex>\n`, `size <n>\n`; size 0 encodes as an empty file | `lfs/pointer.go` `Encoded`, `oidRE` |
| Files of 1024 bytes or more are not pointers | `lfs/scanner.go` `blobSizeCutoff`, `DecodePointerFromBlob` |
| A non-canonical pointer still smudges, but `clean` writes a canonical one back, so the tree changes. Left as text, it passes through unchanged | run |
| Smudge **deletes** a local object of the wrong size, then fails or downloads | `lfs/gitfilter_smudge.go` `Smudge`; run |
| `--skip` and `lfs.fetchinclude`/`fetchexclude` leave the pointer even when the object is local; `.lfsconfig` may set both fetch keys | `commands/command_smudge.go`, `command_filter_process.go`; run |
| For a missing object smudge contacts `lfs.url`, which `.lfsconfig` may set. A `lfs.url` from git config (including `GIT_CONFIG_*`) wins and is looked up before any remote. `file:///dev/null` fails without network | `git/config.go` `Sources`, `lfsapi/endpoint_finder.go`; run |
| An empty `filter.lfs.process` hides a valid `filter.lfs.smudge`, so never set an empty value | run |
| `git check-attr --source=<commit>` reads `.gitattributes` from the commit, ignoring the worktree; git >= 2.40 | run |

## Review Focus

- A run started from a linked worktree (`.git/worktrees/x`): the store is the common `.git/lfs`. Task 1.
- A truncated local object: refused by preflight, and still on disk afterwards. Task 2.
- A repository `.lfsconfig` with `url` and `fetchexclude`: content still appears and the URL is never contacted. Task 3.
- An unused disabled filter and the LFS settings together: neither overwrites the other's `GIT_CONFIG_*` slots. Task 3.
- A file that contains pointer text but is not under `filter=lfs` (a fixture): its object is not required. Task 2.

---

### Task 1: the trust check returns the LFS commands and the local store

**Files:** Modify and test `src/loops/workspace.rs`.

**Produces:**
```rust
/// What the checkout needs to materialise LFS files from the local store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsCheckout {
    /// Validated `clean`/`smudge`/`process` commands of the repository, without `--skip`.
    pub commands: BTreeMap<String, String>,
    pub storage: PathBuf,
}
struct FilterPolicy { disabled: Vec<String>, lfs: Option<BTreeMap<String, String>> }

fn configured_filters(git_dir: &Path) -> Result<BTreeMap<String, Vec<(String, String)>>> // (key, value) in config order
fn lfs_checkout_commands(settings: &[(String, String)]) -> Option<BTreeMap<String, String>> // pure
fn filters_to_disable(git_dir: &Path, repo_root: &Path) -> Result<FilterPolicy>
fn lfs_storage(git_dir: &Path) -> Result<PathBuf>
// RepoPreflightInfo: pub lfs: Option<LfsCheckout>, pub fn lfs_enabled(&self) -> bool
// GitWorkspace: lfs_enabled: bool; FrozenWorkspaceMeta: #[serde(default)] pub lfs_enabled: bool
```

Rules:
- `lfs_checkout_commands`: last value per key wins (as git does); remove ` --skip` from each value, because the goal is real content; `None` when neither `smudge` nor `process` is present. Never adds a key that was not configured.
- `filters_to_disable`: the refusal and the disable rules keep their exact behaviour and messages. `lfs` is `lfs_checkout_commands(..)` only on the existing `(true, true)` arm for `lfs`; otherwise `None`.
- `lfs_storage`: `git config -z --get lfs.storage` through `main_git_dir_cmd` (exit 1 or empty = `lfs`; any other failure is an error with context). Absolute value as is; relative value joined to `git rev-parse --path-format=absolute --git-common-dir`, also through `main_git_dir_cmd`.
- `preflight_check_repo` builds `info.lfs` when the policy has commands. `to_frozen`/`from_frozen`/`create_in` carry `lfs_enabled`.

- [ ] **Step 1: failing tests** (none needs `git-lfs`). Helper for this and the next tasks; the attribute and files are committed **before** the filter is configured so git never runs `git-lfs`:
```rust
fn configure_lfs(repo: &Path) {
    git_ok(repo, &["config", "filter.lfs.clean", "git-lfs clean -- %f"]);
    git_ok(repo, &["config", "filter.lfs.smudge", "git-lfs smudge -- %f"]);
    git_ok(repo, &["config", "filter.lfs.process", "git-lfs filter-process"]);
    git_ok(repo, &["config", "filter.lfs.required", "true"]);
}
/// Commits a root `.gitattributes` (`*.bin filter=lfs`) and `files`, creating parent directories.
fn commit_lfs_files(repo: &Path, files: &[(&str, &str)])
/// `new_repo` + `commit_lfs_files` + `configure_lfs` (so the filter is in `.git/config` only).
fn lfs_repo(tmp: &TempDir, files: &[(&str, &str)]) -> (PathBuf, PathBuf) // (repo, run)
```
  - `lfs_commands_keep_the_last_value_and_never_skip`: settings `smudge="git-lfs smudge %f"`, `smudge="git-lfs smudge --skip -- %f"`, `process="git-lfs filter-process --skip"`, `clean="git-lfs clean -- %f"` → exactly `{clean: "git-lfs clean -- %f", process: "git-lfs filter-process", smudge: "git-lfs smudge -- %f"}`.
  - `lfs_commands_need_smudge_or_process`: only `clean` → `None`; `clean` + `smudge` → `Some` without key `process`.
  - `preflight_records_the_lfs_filter_and_its_local_store`: `lfs_repo` with `("a.bin", "raw\n")`, `preflight_check_repo(.., true)` → `lfs_enabled()`, `commands["smudge"] == "git-lfs smudge -- %f"`, `storage == <main_git_dir>/lfs`, `disabled_filters` does not contain `lfs`.
  - `lfs_configured_but_not_used_is_disabled`: `new_repo` + `configure_lfs` → `lfs.is_none()` and `disabled_filters` contains `"lfs"`.
  - `lfs_store_of_a_linked_worktree_is_the_common_one`: `new_repo`, `commit_lfs_files`, `git worktree add -q <tmp>/wt -b wt`, and only then `configure_lfs` (with the filter configured, `worktree add` would run `git-lfs`); preflight on `wt` → `storage == <repo>/.git/lfs` (compare canonicalised paths).
  - `configured_lfs_storage_is_respected`: `lfs.storage=store` → `<repo>/.git/store`; absolute `<tmp>/abs` → `<tmp>/abs`.
  - `untrusted_lfs_smudge_is_refused`: `lfs_repo`, then `filter.lfs.smudge="./evil"` → preflight error contains `'lfs'`.
  - `old_frozen_metadata_loads_without_lfs_enabled`: `to_frozen()` of a `new_repo` workspace → `serde_json::to_value`, remove `lfs_enabled`, `from_value` → `lfs_enabled == false`, everything else equal.
- [ ] **Step 2:** `cargo test --locked --lib loops::workspace` → does not compile (missing items).
- [ ] **Step 3:** implement; the same command passes, including the unchanged `only_the_exact_git_lfs_commands_are_trusted`. Run fmt and clippy. Ready to commit.

### Task 2: preflight refuses missing LFS objects

**Files:** Modify and test `src/loops/workspace.rs`.

**Consumes:** `RepoPreflightInfo.lfs`, `lfs_repo` (Task 1).

**Produces:**
```rust
enum LfsBlob { Pointer { oid: String, size: u64 }, NonCanonical, Content }
fn classify_lfs_blob(blob: &[u8]) -> LfsBlob // pure
fn lfs_paths_summary(paths: &[Vec<u8>]) -> String // pure
fn check_lfs_objects(repo_root: &Path, base_commit: &str, storage: &Path) -> Result<()>
fn run_git_with_input(cmd: Command, input: Vec<u8>, context_msg: &str) -> Result<Vec<u8>>
```

Rules:
- `run_git_with_input`: the spawn, writer thread and `wait_with_output` code now in `filters_in_use`, moved; `filters_in_use` uses it and keeps its message.
- `classify_lfs_blob`: `Pointer` only when the bytes equal the canonical form rebuilt from the parsed `oid` (64 lowercase hex) and `size` (> 0). Otherwise `NonCanonical` when the bytes contain `oid sha256:` **and** one of `git-lfs`, `git-media`, `hawser` (a superset of what git-lfs decodes). Otherwise `Content`.
- `check_lfs_objects`, every git command through `repo_git_cmd(repo_root)`:
  1. `ls-tree -r -l -z <base_commit>`: keep modes `100644`/`100755` with size < 1024. The size column is padded with spaces; split the part before the tab on whitespace. Paths are bytes.
  2. `check-attr -z --stdin --source=<base_commit> filter` with those paths; keep value `lfs`. Error context: `falha ao ler atributos do commit base (requer git 2.40 ou mais novo)`.
  3. `cat-file --batch` with the unique blob ids of those paths (`<oid> blob <size>\n<bytes>\n`); nothing to ask → return `Ok`; any other header (such as `<oid> missing`) is an error.
  4. An object is present when `fs::metadata` of its path is a regular file with exactly `size` bytes. Any metadata error other than `NotFound`/`NotADirectory` is returned with context.
- Errors, non-canonical first; `n` counts unique LFS `oid`s (for non-canonical, unique blobs) and lists the first path of each, in `ls-tree` order:
  - `{n} ponteiro(s) do Git LFS fora do formato canônico: {paths}; não suportado pelo loop`
  - `faltam {n} objeto(s) do Git LFS no repositório: {paths}; rode 'git lfs fetch' e tente de novo`
  - `lfs_paths_summary`: first 3, joined by `, `, each `String::from_utf8_lossy(..).escape_debug()`; then ` e mais {k}` when there are more.
- `preflight_check_repo` calls it last, after the dirty check and outside `if !from_head`, only when `lfs` is `Some`.

- [ ] **Step 1: failing tests** (none needs `git-lfs`; all use `from_head = true`). Helpers: `pointer(oid: &str, size: u64) -> String` (canonical) and `put_lfs_object(storage: &Path, oid: &str, size: usize)`, which writes `size` bytes at the path layout above. Use `"ab".repeat(32)`-style ids.
  - `lfs_blobs_are_classified`: canonical → `Pointer` with exact `oid` and `size`; version `https://hawser.github.com/spec/v1`, CRLF, an `ext-0-x sha256:…` line, `size 0`, `size 05`, and a missing final newline → `NonCanonical`; uppercase hex → `NonCanonical`; `"raw\n"`, empty, and non-UTF-8 bytes → `Content`.
  - `lfs_summary_lists_three_paths`: 2 paths → `a, b`; 5 paths → `a, b, c e mais 2`; a path with `\x1b` is printed escaped.
  - `missing_lfs_object_is_refused_before_anything_is_created`: 4 pointers with one of them present → `GitWorkspace::create` error equals the string above with `faltam 3 objeto(s)` and the 3 paths; `run` does not exist.
  - `present_lfs_objects_pass_preflight`: all present, one in a subdirectory whose own `.gitattributes` sets `*.dat filter=lfs`, two paths with the same pointer → `Ok`.
  - `truncated_lfs_object_is_refused_and_kept`: object of size − 1 → error contains `faltam 1`; the file still exists.
  - `pointer_text_outside_the_lfs_attribute_is_not_required`: `fixture.txt` with pointer text and no object → `Ok`.
  - `lfs_attributes_come_from_the_base_commit`: commit a pointer without object; then overwrite the worktree `.gitattributes` with `x.bin filter=lfs\n`, where `x.bin` is another committed LFS file with its object present → still refused, naming the first file.
  - `non_canonical_lfs_pointer_is_refused`: hawser pointer whose object **is** present → error contains `1 ponteiro(s) do Git LFS fora do formato canônico`.
  - `lfs_store_objects_are_found_from_a_linked_worktree`: worktree built in the same order as in Task 1; object only in `<repo>/.git/lfs`; preflight on the worktree → `Ok`.
- [ ] **Step 2:** `cargo test --locked --lib loops::workspace` → does not compile. Then add the functions as stubs (`Ok(())`/`Content`/empty) and see the refusal tests fail on assertions.
- [ ] **Step 3:** implement; all pass. Run fmt and clippy. Ready to commit.

### Task 3: the checkout materialises LFS content

**Files:** Modify and test `src/loops/workspace.rs`; modify `CHANGELOG.md`.

**Consumes:** `RepoPreflightInfo.lfs`, `LfsCheckout` (Task 1); the preflight check (Task 2).

**Produces:**
```rust
fn disabled_filter_config(filters: &[String]) -> Vec<(String, String)> // pure, same order as today
fn lfs_checkout_config(lfs: &LfsCheckout) -> Vec<(String, String)>     // pure
fn set_git_config(cmd: &mut Command, config: &[(String, String)])      // GIT_CONFIG_KEY_n / VALUE_n / COUNT
```

Rules:
- `set_disabled_filters(cmd, filters)` keeps its signature and becomes `set_git_config(cmd, &disabled_filter_config(filters))`. Keep the comment about `=` on `set_git_config`.
- `lfs_checkout_config`, in this order: `filter.lfs.<key>` for each command; `filter.lfs.required=true`; `lfs.storage=<storage>`; `lfs.url=file:///dev/null`; `lfs.fetchinclude=` and `lfs.fetchexclude=` (empty). Comment why: the last three outrank the repository's `.lfsconfig`, which could otherwise choose an address or leave pointers.
- `create_in`: only the `checkout` command gets `disabled_filter_config` **plus** `lfs_checkout_config` in **one** `set_git_config` call (a second call would restart at slot 0). `clone`, `remote remove` and later commands keep today's settings. Values come from `info`; do not read git config again.

- [ ] **Step 1: failing tests without `git-lfs`:**
  - `lfs_checkout_config_is_exact`: the full ordered vector for `clean`+`process`+`smudge`.
  - `disabled_filters_and_lfs_settings_do_not_overwrite_each_other`: a `git()` command with `disabled_filter_config(&["a=b".into()])` followed by `lfs_checkout_config(..)` set at once; `git config --get` on separate commands returns `false` for `filter.a=b.required`, the store for `lfs.storage`, `file:///dev/null` for `lfs.url`.
- [ ] **Step 2: failing tests with `git-lfs`.** Each starts with:
```rust
if !git_lfs_available() { // `git-lfs version` exits 0
    println!("SKIPPED: git-lfs unavailable");
    return;
}
```
  Helper `real_lfs_repo(tmp)`: `new_repo`, `configure_lfs`, commit `.gitattributes` (`*.bin filter=lfs diff=lfs merge=lfs -text`) and `a.bin` = `b"REAL\0CONTENT"`; assert the committed blob (`git cat-file -p HEAD:a.bin`) starts with `version https://git-lfs`.
  - `lfs_files_are_real_content_and_the_snapshot_keeps_the_tree`: `create(.., false)`; `a.bin` in the workspace equals `b"REAL\0CONTENT"`; `snapshot() == base_tree()`; `to_frozen().lfs_enabled`; after changing `a.bin` to `b"NEW"`, the `a.bin` blob of the new snapshot is the canonical pointer with size 3; `restore(base_tree)` brings `b"REAL\0CONTENT"` back.
  - `lfs_content_appears_for_every_trusted_command_form`: (a) `process` unset locally, (b) `smudge --skip -- %f` and `filter-process --skip`. Each → content in the workspace and `snapshot() == base_tree()`. Configure the form after the commit.
  - `repository_lfsconfig_chooses_nothing`: `TcpListener` on `127.0.0.1:0`, non-blocking; commit `.lfsconfig` with `[lfs] url = http://127.0.0.1:<port>/x` and `fetchexclude = *.bin`. (1) `create` → real content. (2) New `preflight_check_repo`, then delete the object, then `create_in` in a new directory → `Err`, and `listener.accept()` gives `WouldBlock`.
  - `lfs_checkout_works_from_a_linked_worktree`: `create` with the worktree as repository → real content and `snapshot() == base_tree()`.
  - `lfs_attribute_without_a_filter_is_unchanged`: skip with `SKIPPED: lfs filter configured globally` when `git config --get-regexp '^filter\.lfs\.'`, run in a fresh `git init` **without** the test overrides, finds anything. Otherwise `git config --remove-section filter.lfs`: `create(.., true)` succeeds even with the object deleted, `lfs_enabled == false`, `a.bin` is the pointer text, `snapshot() == base_tree()`.
- [ ] **Step 3: see them fail with a real `git-lfs`**, unpacked in a scratch directory (installs nothing):
```bash
LFS_DIR=$(mktemp -d) && (cd "$LFS_DIR" && apt-get download git-lfs && dpkg -x git-lfs_*.deb root)
PATH="$LFS_DIR/root/usr/bin:$PATH" cargo test --locked --lib loops::workspace -- --nocapture
```
  Expected before the implementation: the content tests fail because `a.bin` is pointer text, and no `SKIPPED: git-lfs unavailable` line. If the download fails, say so in the return and list the LFS tests as **not run**; do not claim they passed.
- [ ] **Step 4:** implement; run the command of Step 3 (all pass, not skipped), and `cargo test --locked --lib loops::workspace -- --nocapture` without it (the LFS tests print `SKIPPED: git-lfs unavailable`).
- [ ] **Step 5:** `CHANGELOG.md`, `[Unreleased]` → `### Fixed`, first item:
  `- **Git LFS files in loop workspaces.** With the LFS filter configured only in the repository's .git/config, agents saw pointer files. The checkout now applies the validated LFS filter and reads objects only from the local LFS store; a run is refused before cloning, with a hint to run git lfs fetch, when an object is missing (or when a pointer is not canonical), and the repository's .lfsconfig no longer affects the checkout.`
- [ ] **Step 6: final checks**
```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
PATH="$LFS_DIR/root/usr/bin:$PATH" cargo test --locked --lib loops::workspace
git diff --stat   # only src/loops/workspace.rs and CHANGELOG.md
```
  Ready to commit.

## After integration (user, on the host)

Run the workflow on the Godot project with the LFS filter only in `.git/config`: a `.glb`/texture in the workspace has its real size, and the published branch stores a pointer for an unchanged or edited LFS file.
