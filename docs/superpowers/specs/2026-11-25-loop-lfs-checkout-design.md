# Loop: LFS files in the workspace checkout

Date: 2026-11-25. Size: small change (one module, `src/loops/workspace.rs`).

## Intent

In a repository that uses Git LFS with the filter configured only in the main
`.git/config` (`git lfs install --local`), the agent sees LFS pointer files instead of the
binaries. Cause (confirmed on the host): `create_in` checks out `base_commit` inside the
clone (`workspace/.git`), which does not inherit that config, so no smudge runs. Runs of the
Godot project stopped in `implement` for this reason.

## Goals

- The checkout materialises real LFS content when the repository uses LFS and the `lfs`
  filter passes the existing trust check (`filters_to_disable`, `GIT_LFS_COMMANDS`).
- The snapshot, tree and `base_tree` stay byte-for-byte what they are today.
- A missing local LFS object is reported before anything is cloned.

## Non-goals

- No network access by the engine; no download from an LFS endpoint.
- No change to which filters are trusted; other filters stay disabled or refused.
- No `.lfsconfig` support: the agent's or the repository's `.lfsconfig` never chooses a URL.

## Anti-goals

- A trusted filter must never run with agent-controlled configuration.
- The snapshot must never store raw binary where a pointer belongs, or the reverse.

## Approaches

1. **Local store, fail early (chosen).** Pass the validated `lfs` filter to the checkout
   through `GIT_CONFIG_*`, and set `lfs.storage` to `<main .git>/lfs` so smudge reads only
   the main repository's cache. Preflight checks that every LFS object of `base_commit` is
   present locally and otherwise fails, listing them and telling the user to run
   `git lfs fetch`.
2. Smudge downloads from the network. Convenient, but moves network access, with an address
   that comes from the repository, outside the sandbox. Rejected by the user.
3. Make the clone inherit the main `.git/config`. Copies agent-reachable and untrusted
   filters and hooks into the workspace. Rejected.

## Design

- New helper next to `set_disabled_filters`: an env setter that adds the trusted `lfs`
  `clean`/`smudge`/`process` values (copied from the already validated main config) plus
  `filter.lfs.required=true` and `lfs.storage=<main_git_dir>/lfs`. The engine has no switch
  that forbids the network, so the preflight object check is the guard: with every object
  present locally, smudge never needs to download.
- `filters_to_disable` also returns whether `lfs` is trusted and in use (`lfs_enabled`),
  stored in `RepoPreflightInfo` and the frozen workspace metadata (additive field, default
  false so existing `state.json` loads).
- Preflight: when `lfs_enabled`, list LFS objects of `base_commit` and fail if any is absent
  from `<main .git>/lfs/objects`. Error is in Portuguese like the rest, names the count and
  the first few paths, and suggests `git lfs fetch`.
- `create_in`: apply the LFS env only to the `checkout` command, not to `clone` (no
  checkout there) or later commands.
- If `.gitattributes` uses LFS but no `lfs` filter is configured anywhere, behaviour is
  unchanged (pointers stay pointers, as in the main repository).

## Decisions after planning (coordinator, 2026-11-25)

The planner checked git-lfs 3.7.0 source and ran 3.4.1; these refine the design and are
accepted (details in the plan):

- ` --skip` is removed from the trusted commands at checkout; a `--skip-smudge` user without
  the objects is refused by preflight like anyone else.
- The checkout also sets `lfs.url=file:///dev/null` and empty `lfs.fetchinclude` /
  `lfs.fetchexclude`, which outrank `.lfsconfig`; so no URL from the repository is contacted.
- The store is the common git dir's `lfs` (linked worktrees), or a configured `lfs.storage`.
- Preflight checks object size too (smudge deletes a wrong-size object from the user's store)
  and refuses non-canonical pointers (materialising them would change the snapshot tree).
- Attributes are read from `base_commit` (`check-attr --source`, git 2.40 or newer, LFS
  repositories only). `lfs_enabled` needs a configured `smudge` or `process`.

Out of scope, follow-up: `restore`/`snapshot` still run git-lfs with the agent's
`.lfsconfig`, which can make a request outside the sandbox (existing; reproduced by the
planner). Fix: the same `lfs.url`/`fetch*` overrides in `engine_git` when `lfs_enabled`.

## Left to the builder

Exact way to enumerate LFS objects without running anything untrusted (parse pointer blobs
of `base_commit` via `git ls-files`/`cat-file`, not `git lfs` if it is simpler); test layout;
changelog line. Tests need `git-lfs`; skip with a clear message when it is not installed
(not in this sandbox, present on ai-box).

## Validation

Unit/integration tests: pointers become content with local-only config; missing object fails
in preflight; untrusted `filter.lfs.smudge` is still refused; snapshot tree equals
`base_tree` for an unchanged workspace. Then host check on the Godot project.
