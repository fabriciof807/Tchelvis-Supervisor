# M5a — Rename to `tchelvis`, release pipeline, one-command update (design)

_Date: 2026-11-15 · Roadmap: [M5a](../../../ROADMAP.md), pulled forward · Status: awaiting user approval_

## Intent

Today, updating the host takes several manual steps:
1. build inside the `ai-box` incus container;
2. `incus file pull` the binary;
3. `chmod`;
4. kill the daemons;
5. `cockpit setup --set-default-layout`;
6. check that no stale copy shadows the new binary.

The user wants **one command** that fetches the new version and leaves everything ready: binary, Herdr mod/layout,
hooks, and restarted daemons. This comes before the AUR and Homebrew packages (M5b).

Decision D1 says the rename `cockpit` → `tchelvis` happens **before the first public release**, so it is part of this work.

## Goals
- **Rename:** binary, crate, and user-facing product name `tchelvis`. Existing installs migrate automatically, without losing config, state, tasks, or settings.
- **Release pipeline (GitHub Actions + GitHub Releases), two channels:**
  - **stable:** a release `vX.Y.Z`, created when a `v*` tag is pushed;
  - **nightly:** a prerelease with the fixed tag `nightly`, rebuilt and overwritten on **every push to `main`**. The user chose the "nightly" name; it is per push rather than per night, so the user can test right after each fix.
- **Artifacts:** Linux `x86_64` and `aarch64` static binaries (musl), each with a `sha256`.
- **`tchelvis update [--channel stable|nightly]`:** download, verify, replace the binary atomically, re-run setup, restart the daemons. The chosen channel is remembered.
- **`install.sh`** for the first install, e.g. `curl -fsSL https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/install.sh | sh -s -- --channel nightly`.
- **License:** dual **MIT OR Apache-2.0**. Today `Cargo.toml` says Apache-2.0 while `LICENSE`/README say MIT; this fixes the mismatch.

## Amendment (2026-11-15, user decision): no `cockpit` → `tchelvis` migration
The only existing install is the user's, and the user will run `cockpit uninstall --purge` and then install `tchelvis` cleanly.
Therefore:
- the directory migration, the TOML path rewrite, the legacy file, marker, and fence cleanup, the legacy binary removal, the
  legacy container removal, and the `COCKPIT_*` env aliases are **dropped**. Wherever §1 says "Migration", read
  "none, clean install";
- `.cockpit/runbooks` is not read any more, only `.tchelvis/runbooks`;
- §1.1 (stop and restart processes) stays, but only for `tchelvis daemon` / `tchelvis supervisor watch`. It is used by
  `update`, so a new binary does not leave the old daemons running.

## Non-goals
- AUR and Homebrew packages (M5b, written by hand later against these release artifacts).
- macOS and Windows (D2: Linux only).
- A Docker image of the CLI (D3).
- Signing artifacts beyond `sha256` checksums.
- Background or automatic update checks. Updating is always explicit.

## Anti-goals
- `update` leaves a half-written binary or a broken install if the download, verification, or setup fails.
- The rename silently loses user data: `supervisor.toml`, daily tasks, supervisor state, the GPU toggle, the chosen model, or the Herdr default layout.
- An old `cockpit` binary keeps shadowing the new one on `PATH`.
- `update` deletes files it does not own.

## Constraints
- Linux only. Build artifacts are static (musl). This is feasible because `reqwest` uses `rustls-tls` and `rusqlite` is `bundled`, so there is no OpenSSL.
- The repository is public (`fabriciof807/Tchelvis-Supervisor`). Standard GitHub-hosted runners are available, including arm64 Linux runners for public repos.
- No new runtime dependencies for the update path: use the existing `ureq`, `sha2`, and `serde_json`.
  A tarball reader (`flate2` + `tar`) is acceptable if artifacts are `.tar.gz`; otherwise ship plain binaries (see §3).
- Do not use the GitHub API with authentication. Anonymous requests are enough for a public repo; respect rate limits by
  making one API call per update.

## Approaches considered
| Approach | Trade-off | Decision |
| :-- | :-- | :-- |
| A. `cargo-dist` (generated CI + installer + Homebrew + `axoupdater`) | Little custom code and Homebrew almost for free, but it is tag-driven: nightly-per-push fights the tool, and the post-update steps (setup, daemon restart) must be bolted on. | Rejected |
| **B. Own workflow + built-in `tchelvis update`** | Exactly the stable/nightly model and the one-step "everything ready" update. More code to maintain; Homebrew/AUR written by hand in M5b. | **Chosen (user)** |
| C. `cargo install --git` | One command, but needs Rust on the host and recompiles for minutes on every update. | Rejected |

## Design

### 1. Rename `cockpit` → `tchelvis`

| Item | Old | New | Migration |
| :-- | :-- | :-- | :-- |
| Crate / binary | `cockpit` | `tchelvis` | `setup` / `update` remove known old binaries (below) |
| Product name in UI and docs | Cockpit | Tchelvis | — |
| Config / data / state / runtime / cache dirs | `~/.config/cockpit`, `~/.local/share/cockpit`, `~/.local/state/cockpit`, `$XDG_RUNTIME_DIR/cockpit`, `~/.cache/cockpit` (model download cache, can be GBs) | `…/tchelvis` (socket/log become `tchelvis.sock` / `tchelvis.log`) | **Only in `setup`** (and therefore in `install.sh` / `update`), never implicitly from other commands. Steps: (1) stop the old processes (§1.1); (2) for each dir, if the new one is missing and the old one exists, `rename` old → new (copy + remove across filesystems); if both exist, keep the new one, leave the old in place, and report it; (3) rewrite absolute paths that point into old dirs inside `supervisor.toml` (`model_id = "file:…"`, `model_path`, `state_file_path`) to the new prefix, with `toml_edit`. Log one line per moved dir and per rewritten key. |
| Herdr mod id, mod dir, layout id | `cockpit` (`~/.config/herdr-app/mods/cockpit`, `layouts/cockpit.json`) | `tchelvis` | `setup` installs the new files, removes the old mod dir and layout file, and switches `herdr-app/settings.json` from `default_layout = "cockpit"` to `"tchelvis"` (only if it was `cockpit`). |
| Layout node ids | `cockpit-pill`, `cockpit-fleet`, `cockpit-home`, `cockpit-inspector`, `cockpit-status` | `tchelvis-*` | covered by the layout reinstall |
| Shell fence markers / hook marker | `# >>> cockpit managed >>>`, `# cockpit-managed`, `cockpit-tag()` | `tchelvis` equivalents | `setup` and `uninstall` recognize **both** old and new markers, so they can replace or remove old blocks |
| Hook / plugin files | `opencode/plugins/cockpit.js`, `.pi/agent/extensions/cockpit.js`, Claude hooks | `tchelvis.*` | `setup` removes the old files after writing the new ones; the manifest records both |
| systemd user unit | `cockpit.service` | `tchelvis.service` | `setup --systemd` disables and removes the old unit if present |
| LLM container | `cockpit-supervisor-llm` (labels `cockpit.ngl`, `cockpit.model_id`) | `tchelvis-supervisor-llm` (labels `tchelvis.ngl`, `tchelvis.model_id`) | On `load`/`unload`: if the old-named container exists, `rm -f` it first |
| Env vars | `COCKPIT_*` | `TCHELVIS_*` | Read `TCHELVIS_X` first, then `COCKPIT_X` (deprecated alias, still honored). One helper for every lookup. |
| hc terminal labels | `cockpit.*` (`cockpit.agent`, `cockpit.state`, …) | **unchanged** | _Coordinator's call:_ this is an internal protocol between the hooks, `hc`, and the binary, written onto live terminals. Renaming it would break running sessions for no user-visible gain. |

- **Other `cockpit` names (_coordinator's calls_):**
  - **Kept:** Herdr shared-state / doc keys exchanged between the binary and the mod (`cockpit.snapshot`, …) stay, like the hc labels. They are internal, and renaming them would break a still-loaded old mod until Herdr restarts.
  - **Per-repo runbooks:** read both `.cockpit/runbooks/` and `.tchelvis/runbooks/`. Users' repos are not touched.
  - **Process-name filters:** `src/supervisor/hc.rs` and `main.js` treat both `cockpit…` and `tchelvis…` as our own UI processes.

#### 1.1 Running processes during setup/migration (_coordinator's call_)
- **Before moving dirs,** `setup` stops our running processes:
  - `cockpit daemon`, `cockpit supervisor watch`, `tchelvis daemon`, `tchelvis supervisor watch`;
  - matched by `/proc/<pid>/cmdline` argv[0] basename ∈ {`cockpit`, `tchelvis`} and argv[1] ∈ {`daemon`, `supervisor`}; only the current user's processes;
  - SIGTERM, wait up to 3 s, then SIGKILL.
  - This avoids old processes recreating `~/.local/state/cockpit` and two daemons running at once.
- **After setup,** it restarts **only what it stopped**:
  - same subcommand and args, with `cockpit` → `tchelvis`;
  - detached (`setsid`), stdout/stderr to `~/.local/state/tchelvis/<name>.log`;
  - with the environment of the `setup` / `update` process;
  - if the systemd user unit is enabled, it is restarted instead of spawning.
  - The summary lists what was restarted and warns that the environment came from this shell (relevant for `TCHELVIS_LLM_*` / GPU variables).
- **The Herdr mod** does not run these daemons (it calls one-shot commands), so nothing else needs restarting except Herdr Desktop itself, to reload the mod.

- **Old binaries:** `setup` and `update` remove `~/.local/bin/cockpit` and `~/.cargo/bin/cockpit` **only if** they are this project's
  binary (`<path> --version` prints `cockpit 0.` and `<path> --help` mentions `Herdr`). Anything else is left alone and reported.
  No `cockpit` compatibility symlink is created, because the name clashes with the Cockpit Project.
- **CLI surface:** unchanged apart from the binary name.

### 2. Versioning and build metadata
- The first release after the rename is **`v0.2.0`**. `0.1.0` stays as the pre-rename baseline in `CHANGELOG.md`.
- `Cargo.toml` is bumped to `0.2.0` **in this work**, so nightlies print `0.2.0 (nightly <sha>)`.
  _Coordinator's call:_ the first stable tag `v0.2.0` is cut **after the user validates the nightly on the host**, and only with the user's explicit OK, because it pushes a tag.
- Compile-time metadata via `option_env!`: `TCHELVIS_CHANNEL` (`stable` | `nightly` | unset = `dev`) and `TCHELVIS_COMMIT` (short sha).
  - `tchelvis --version` → `tchelvis 0.2.0 (nightly a1b2c3d)` / `tchelvis 0.2.0 (stable)` / `tchelvis 0.2.0 (dev)`.
- Releasing stable: move `CHANGELOG.md` "Unreleased" into `## [0.2.0] - <date>`, bump `Cargo.toml`, tag `v0.2.0`, push the tag.
  Write these steps in `docs/RELEASING.md`.

### 3. Release workflow (`.github/workflows/release.yml`)
- **Triggers:**
  - `push` to `main` → nightly;
  - `push` of tags `v*` → stable;
  - `workflow_dispatch` for manual runs.
- **Gate:** `cargo test --locked` must pass before any publish.
- **Build matrix:**
  - `x86_64-unknown-linux-musl` on `ubuntu-latest`;
  - `aarch64-unknown-linux-musl` on the GitHub arm64 Linux runner (or `cross`/`cargo-zigbuild` if the runner is unavailable; builder's choice).
  - Release profile with `strip`.
- **Assets per target:** `tchelvis-<version>-<target>.tar.gz` containing `tchelvis` + `LICENSE-MIT` + `LICENSE-APACHE` + `README.md`,
  plus `tchelvis-<version>-<target>.tar.gz.sha256`. Also a combined `SHA256SUMS`.
  - Nightly `<version>` = `nightly`, so the asset names stay stable for the updater.
- **Stable:** create a GitHub Release `vX.Y.Z` with the assets. The release notes are the version's CHANGELOG section.
- **Nightly:**
  1. move the `nightly` tag to the pushed commit (force-update);
  2. delete the old `nightly` release assets;
  3. upload the new ones;
  4. mark the release as **prerelease**, with a body line `commit <sha> · <date>`.
- Use `concurrency: nightly` so overlapping pushes do not race.
- **Permissions:** `contents: write` only.

### 4. `tchelvis update`

```
tchelvis update [--channel stable|nightly] [--check] [--no-setup] [--force]
```

- **Channel:** the `--channel` flag, else the remembered channel (`~/.local/state/tchelvis/update_channel`), else `stable`.
  A successful update writes the channel used.
- **Steps:**
  1. **Resolve the release.**
     - stable: `GET https://api.github.com/repos/fabriciof807/Tchelvis-Supervisor/releases/latest`;
     - nightly: `…/releases/tags/nightly`.
     - If stable returns 404 (no stable release yet), fail clearly: "No stable release yet — use `--channel nightly`". Do not silently switch channels. `install.sh` behaves the same way.
     - Pick the asset for the running target, using `std::env::consts::ARCH` → `x86_64` | `aarch64`.
  2. **Skip if current.**
     - stable: skip when the release version equals `CARGO_PKG_VERSION` and the build channel is `stable`;
     - nightly: skip when the release body's commit equals `TCHELVIS_COMMIT`.
     - `--force` overrides the skip.
     - `--check` prints `current → available` and exits 0 if an update exists, 1 if not.
  3. **Download** to a temp file in the **same directory** as the installed binary, verify the `sha256`, and extract `tchelvis`.
  4. **Replace atomically:**
     - `chmod 0755`;
     - `rename(temp, <path of current_exe()>)`, which is safe on Linux while the binary is running;
     - if the directory is not writable, fail with a clear message suggesting `install.sh` or the right permissions;
     - never use `sudo`.
  5. **Post-update**, unless `--no-setup`: run the **new** binary's `setup --set-default-layout` as a child process. That setup installs the new embedded assets, runs the migration, removes old binaries, and stops/restarts the processes per §1.1.
  6. **Print a summary:** old → new version, channel, and what setup changed. Tell the user to restart Herdr Desktop, which is needed to reload the mod.
- **Failure handling:** any failure before step 4 leaves the installed binary untouched. A failure in step 5 reports the error and the
  command to retry (`tchelvis setup --set-default-layout`). The binary stays updated.

### 5. `install.sh` (repository root)
- **Usage:** `sh install.sh [--channel stable|nightly] [--dir ~/.local/bin]`.
- **Steps:**
  1. detect the architecture;
  2. download the asset and its `.sha256` with `curl` (fall back to `wget`) and verify with `sha256sum`;
  3. install to `~/.local/bin/tchelvis` (create the dir; warn if it is not on `PATH`);
  4. run `tchelvis setup --set-default-layout`. This performs the `cockpit` → `tchelvis` migration, including stopping and restarting old processes (§1.1). It is the only migration path for existing installs, since the old binary has no `update` command;
  5. write the channel file.
- POSIX `sh` (no bashisms), `set -eu`, no `sudo`.
- It is idempotent: running it again acts as an update.

### 6. License
- Add `LICENSE-MIT` (the current `LICENSE` text, renamed) and `LICENSE-APACHE` (the standard Apache-2.0 text).
- `Cargo.toml`: `license = "MIT OR Apache-2.0"`. The README badge and license section show the dual license.

### 7. Docs
- `README.md`: Install = `install.sh` (stable and nightly), update = `tchelvis update`; build from source stays as an alternative.
  Every `cockpit` command becomes `tchelvis`.
- `docs/agent-install.md`: detection order `tchelvis` → `cockpit` (legacy). Procedure A = `install.sh` / `tchelvis update`.
- `HANDOFF.md` §3: the sync block is replaced by `tchelvis update --channel nightly`.
- `CHANGELOG.md`: the rename and migration are listed under Unreleased → Changed, with the breaking notes (binary name, env prefix).
- `docs/RELEASING.md` (new): the steps to cut a stable release.

## Left to the builder
- Choosing `flate2`+`tar` for the tarballs vs. other formats, the exact GitHub Actions used (e.g. `softprops/action-gh-release`), and
  arm64 runner vs. cross-compilation.
- Internal module layout (suggested: `src/update.rs`, `src/migrate.rs`, `src/envvars.rs`).
- Wording of the update summary.

## Testing
- **Pure helpers, with unit tests:**
  - asset selection from a releases-API JSON fixture;
  - the "is current?" decision for stable/nightly/`--force`;
  - sha256 verification (match and mismatch);
  - channel resolution (flag > file > default);
  - the env alias helper (`TCHELVIS_X` wins over `COCKPIT_X`);
  - fence-marker recognition (old and new);
  - "is our old binary?" detection using fake command output;
  - directory migration in a temp HOME (rename, already-migrated no-op, both-exist → keep new, and log).
- **Atomic replace:** write into a temp dir and simulate a failure before the rename → the original is unchanged.
- **CI:** the workflow is validated by its first nightly run on `main` (the assets exist, `sha256` matches, and `tchelvis --version` in the artifact prints `nightly <sha>`).
- **Manual (host):**
  1. Run `install.sh --channel nightly` on a machine with the old `cockpit` install. Config, tasks, state, and the chosen model survive; the Herdr layout is `tchelvis`; no `cockpit` binary is left.
  2. Push to `main` → after CI, `tchelvis update` fetches the new nightly.
