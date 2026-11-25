# Loop LFS engine network Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** the engine's git-lfs (in `snapshot`/`restore`) ignores the agent's `.lfsconfig`.

**Spec:** `docs/superpowers/specs/2026-11-25-loop-lfs-engine-network-design.md`

## Global Constraints

- Change only `src/loops/workspace.rs` and one line in `CHANGELOG.md`.
- Existing tests stay unchanged and pass. Test helpers to reuse: `real_lfs_repo`,
  `git_lfs_available`, `classify_lfs_blob`, `lfs_checkout`, `git_ok`, `git_cmd`.
- Do not commit or push. User-visible text in Portuguese; code/comments/CHANGELOG in English.
- Checks: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
  `cargo test --locked`, and the LFS tests with a real git-lfs:
  ```bash
  LFS_DIR=$(mktemp -d) && (cd "$LFS_DIR" && apt-get download git-lfs && dpkg -x git-lfs_*.deb root)
  PATH="$LFS_DIR/root/usr/bin:$PATH" cargo test --locked --lib loops::workspace -- --nocapture
  ```
  If the download fails, report the LFS tests as NOT RUN.

### Task 1: one network block for checkout and engine

- [ ] **Step 1: failing tests.**
  - `lfs_network_config_is_exact` (no git-lfs): exactly
    `[("lfs.url","file:///dev/null"),("lfs.fetchinclude",""),("lfs.fetchexclude",""),("lfs.skipdownloaderrors","false")]`.
  - Update the expected vector of `lfs_checkout_config_is_exact` by appending
    `("lfs.skipdownloaderrors","false")` after `lfs.fetchexclude` (this is the only allowed
    edit of an existing test, because the spec adds that key).
  - `engine_git_ignores_the_agents_lfsconfig_url` (git-lfs; skip pattern as the other LFS
    tests): `real_lfs_repo`; `create(.., false)`; non-blocking `TcpListener` on
    `127.0.0.1:0`; in the workspace write `.lfsconfig` with
    `[lfs]\n\turl = http://127.0.0.1:<port>/x\n` and `b.bin` = a canonical pointer
    (`version https://git-lfs.github.com/spec/v1\noid sha256:<"ab".repeat(32)>\nsize 5\n`),
    then `let tree = ws.snapshot().unwrap()`; delete `b.bin` from the workspace;
    `ws.restore(&tree)` → `Err`; `listener.accept()` → `WouldBlock`.
  - `engine_git_ignores_the_agents_fetchexclude` (git-lfs): `real_lfs_repo`;
    `create(.., false)`; write `.lfsconfig` with `[lfs]\n\tfetchexclude = *.bin\n` and
    `a.bin` = `b"CHANGED"` in the workspace; `ws.restore(base_tree)`; `a.bin` equals
    `b"REAL\0CONTENT"` (note: `restore` removes the untracked `.lfsconfig` only in
    `git clean` after `read-tree -u`, so the override is what matters).
  - Run with real git-lfs: the two engine tests must fail before the implementation (the
    first one sees a connection or succeeds; the second sees pointer text). Before the fix
    git-lfs may wait on the never-answered connection, so run that pre-implementation check
    under `timeout 300 …`; a timeout there counts as the expected failure. If either passes
    before the change, stop and report `needs-decision` with output.
- [ ] **Step 2: implement** per the spec "Design": `lfs_network_config`, use it in
  `lfs_checkout_config` (comment: these outrank `.lfsconfig`, which the agent controls in the
  engine's commands and the repository controls at checkout), and in `engine_git` when
  `self.lfs_enabled`, in one `set_git_config` call with the disabled filters (a second call
  would restart at slot 0).
- [ ] **Step 3: CHANGELOG** `[Unreleased]` → `### Fixed`, after the Git LFS item:
  `- **The agent's .lfsconfig no longer steers the engine.** Snapshot and restore ran git-lfs outside the sandbox with the workspace's .lfsconfig, so an agent could make the engine contact an address of its choice or leave pointers; the engine now overrides those settings.`
- [ ] **Step 4: final checks** (all commands of Global Constraints, plus `git diff --stat`).
  Ready to commit.
