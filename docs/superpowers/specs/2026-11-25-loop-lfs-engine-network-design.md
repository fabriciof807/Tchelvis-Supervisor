# Loop: the agent's `.lfsconfig` never reaches the engine's git-lfs

Date: 2026-11-25. Size: small change (`src/loops/workspace.rs`). Follow-up of
`2026-11-25-loop-lfs-checkout-design.md`.

## Intent

`snapshot` and `restore` run git-lfs (through `engine_git`) outside the sandbox with the
workspace as work tree. git-lfs reads `.lfsconfig` from the work tree, which the agent writes.
Reproduced by the planner of the checkout fix: an agent commits `.lfsconfig` with `lfs.url`
and a pointer to an unknown object, and `restore` POSTs to the agent's URL.

## Goals

- No key of the agent's `.lfsconfig` that git-lfs honours changes what the engine's git-lfs
  does. Per git-lfs 3.7.0 `config/git_fetcher.go` `safeKeys`, `.lfsconfig` can only set
  `lfs.allowincompletepush`, `lfs.fetchexclude`, `lfs.fetchinclude`, `lfs.gitprotocol`,
  `lfs.locksverify`, `lfs.pushurl`, `lfs.skipdownloaderrors`, `lfs.url`. The engine never
  pushes or locks; the four that matter are overridden.
- A pointer to an object missing from the local store makes `restore` fail without network.

## Non-goals

- `--skip` filter commands in the user's config still leave pointers after `restore` (not a
  security issue; separate).
- No change to which filters are trusted, nor to the snapshot result.

## Approaches

1. **Override in `engine_git` (chosen).** Same `GIT_CONFIG_*` mechanism the checkout uses;
   git config outranks `.lfsconfig`.
2. Delete/ignore `.lfsconfig` before engine commands: mutates the agent's workspace and races
   with it. Rejected.
3. Run engine git-lfs inside the sandbox: large change. Rejected.

## Design

- Pure `lfs_network_config() -> Vec<(String, String)>`: `lfs.url=file:///dev/null`,
  `lfs.fetchinclude=`, `lfs.fetchexclude=`, `lfs.skipdownloaderrors=false`, in this order.
- `lfs_checkout_config` uses it in place of its three literal lines (gains
  `skipdownloaderrors`).
- `engine_git`: when `self.lfs_enabled`, one `set_git_config` call with
  `disabled_filter_config(..)` followed by `lfs_network_config()`; otherwise unchanged.
  Without LFS enabled the `lfs` filter is disabled or refused, or has no smudge/process, so
  git-lfs never downloads.
