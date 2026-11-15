# M5a — Rename to `tchelvis`, Release Pipeline, One-Command Update — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename `cockpit` to `tchelvis` (clean install, **no migration**, per the user's decision of 2026-11-15) and publish static Linux builds on two channels (stable tags, nightly per push to `main`). `tchelvis update` / `install.sh` install a release and leave everything ready in one command.

**Architecture:**
- New focused modules: `src/envvars.rs` (single `TCHELVIS_` env lookup), `src/version.rs` (build metadata), `src/procs.rs` (stop/restart our daemons around `update`), `src/update.rs` (release resolution, download, verify, atomic replace).
- A GitHub Actions workflow publishes the releases. `install.sh` bootstraps installs.

**Tech Stack:** Rust 2021 (`ureq`, `sha2`, `serde_json`, `toml_edit`, `libc`; new: `flate2 = "1"`, `tar = "0.4"`), POSIX `sh`, GitHub Actions + `gh` CLI.

**Spec:** `docs/superpowers/specs/2026-11-15-m5a-rename-release-update-design.md`. Read it fully; § numbers below refer to it.

## Global Constraints
- **Linux only.** Release targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`.
- **Version** `0.2.0` in `Cargo.toml`. Build metadata comes from compile-time env `TCHELVIS_CHANNEL` (`stable` | `nightly`; unset → `dev`) and `TCHELVIS_COMMIT` (short sha).
- **Repo:** `fabriciof807/Tchelvis-Supervisor`. Asset name: `tchelvis-<version>-<target>.tar.gz` + `.sha256` (nightly `<version>` = `nightly`). Release tags: `vX.Y.Z` and `nightly`.
- **Renamed:** binary/crate `tchelvis`; dirs `…/tchelvis`; `tchelvis.sock`, `tchelvis.log`; mod id/dir and layout id `tchelvis`; layout node ids `tchelvis-*`; container `tchelvis-supervisor-llm` with labels `tchelvis.ngl` / `tchelvis.model_id`; unit `tchelvis.service`; shell fence `# >>> tchelvis managed >>>` / `# <<< tchelvis managed <<<`; hook marker `# tchelvis-managed`; shell function `tchelvis-tag`; env prefix `TCHELVIS_` (no `COCKPIT_` alias).
- **Kept unchanged:**
  - hc terminal labels `cockpit.*`;
  - Herdr shared-state keys `cockpit.*` used between the binary and the mod;
  - internal Rust type names (e.g. `CockpitPaths`).
- **Error copy:**
  - "No stable release yet — use `--channel nightly`";
  - "Restart Herdr Desktop to reload the mod."
- No `sudo` anywhere. Never delete a file that is not this project's. **No legacy migration or cleanup code** (amendment).
- Format only the files you touched (`rustfmt --edition 2021 <files>`); never run a repo-wide `cargo fmt`. Deterministic tests: temp dirs, fakes, no network.
- **Delegation:** the worker does not commit. Each task ends with "leave uncommitted"; the coordinator reviews and commits per batch.

## Review Focus
1. **`tchelvis daemon` is running when `tchelvis update` runs:** it is stopped before setup and restarted from the new binary afterwards. → Task 4 test `finds_daemon_and_watch_ignores_others`.
2. **`~/.local/bin` (the binary's dir) is not writable during `update`:** clear error, installed binary unchanged. → Task 5 test `install_bytes_unwritable_dir_errors_and_keeps_original`.
3. **A truncated download or wrong checksum:** the update aborts before replacing anything. → Task 5 test `install_bytes_rejects_bad_sha_and_keeps_original`.
4. **The GitHub API is unreachable or rate-limited (HTTP 403/5xx):** clear error naming the HTTP status, nothing replaced. → Task 5 test `http_error_message_names_status` (pure formatter).
5. **The machine's arch is not x86_64/aarch64:** a clear "unsupported architecture" error, in both `update` and `install.sh`. → Task 5 test `parse_release_missing_asset_errors`; Task 6 step 2.

## Execution batches (coordinator reviews and commits between batches)
- **B1:** Tasks 1–2
- **B2:** Tasks 4–6 (Task 3 dropped)
- **B3:** Tasks 7–8

---

### Task 1: Env aliases, version metadata, dual license

**Files:**
- Create: `src/envvars.rs`, `src/version.rs`, `LICENSE-APACHE`
- Rename: `LICENSE` → `LICENSE-MIT`
- Modify: `src/lib.rs`, `Cargo.toml` (`version = "0.2.0"`, `license = "MIT OR Apache-2.0"`), `src/main.rs` (clap version), every `env::var("COCKPIT_…")` call site (`src/config.rs`, `assets.rs`, `usage.rs`, `setup.rs`, `supervisor/{mod,ollama,models,config}.rs`, `providers/{linear,slack,jira}.rs`)

**Interfaces:**
- Produces:
  ```rust
  // envvars.rs — `suffix` is the name without prefix, e.g. "LLM_MODEL"
  pub fn var(suffix: &str) -> Option<String>                       // TCHELVIS_<suffix> only
  pub fn var_with(suffix: &str, get: impl Fn(&str) -> Option<String>) -> Option<String>
  // version.rs
  pub fn format_version(pkg: &str, channel: Option<&str>, commit: Option<&str>) -> String
  pub fn long_version() -> &'static str     // format_version(CARGO_PKG_VERSION, option_env!("TCHELVIS_CHANNEL"), option_env!("TCHELVIS_COMMIT"))
  pub fn build_channel() -> Option<&'static str>; pub fn build_commit() -> Option<&'static str>
  ```

- [ ] **Step 1: Write failing tests:**
  - `var_with_reads_tchelvis_only`: getter maps `TCHELVIS_X → "new"` → `Some("new")`. Only `COCKPIT_X` set → `None`.
  - `format_version_cases`: `("0.2.0", Some("nightly"), Some("a1b2c3d")) == "0.2.0 (nightly a1b2c3d)"`, `("0.2.0", Some("stable"), None) == "0.2.0 (stable)"`, `("0.2.0", None, None) == "0.2.0 (dev)"`.
- [ ] **Step 2:** Run `cargo test --quiet envvars version`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Replace every `COCKPIT_*` env read with `envvars::var("…")`.
  - Clap: `#[command(version = tchelvis… long_version())]`, using `Box::leak` for the `'static` string. Use the crate path the code has at the time; Task 2 renames it.
  - In shell/JS hook assets, rename the env vars to `TCHELVIS_*`.
  - `LICENSE-APACHE` contains the standard Apache License 2.0 text.
- [ ] **Step 4:** Run `cargo test --quiet`. Expected: PASS.
- [ ] **Step 5:** Leave uncommitted.

### Task 2: Mechanical rename `cockpit` → `tchelvis`

**Files:**
- `Cargo.toml` (`name = "tchelvis"`; lib and bin `tchelvis`);
- every `use cockpit::` in `src/main.rs` and `tests/integration_tests.rs`;
- `src/config.rs` (dir names, `tchelvis.log`, `tchelvis.sock`), `src/supervisor/{config,container,mod}.rs` (paths, `supervisor.toml` lookup `~/.config/tchelvis/supervisor.toml` then legacy `~/.config/herdr/supervisor.toml`, container name and labels, hf cache `~/.cache/tchelvis/llama.cpp`), `src/daemon.rs`;
- `src/assets.rs` (include paths, fence/marker constants, `tchelvis-tag`, `command -v tchelvis`), `src/setup.rs` (all install targets and names per Global Constraints), `src/doctor.rs`;
- `src/providers/runbooks.rs` (`.tchelvis/runbooks` instead of `.cockpit/runbooks`), `src/supervisor/hc.rs` and `main.js` (process filters use `tchelvis`);
- `git mv assets/herdr-app/mods/cockpit assets/herdr-app/mods/tchelvis`, `git mv assets/herdr-app/layouts/cockpit.json assets/herdr-app/layouts/tchelvis.json`;
- `mod.json` id;
- `main.js`:
  - `export const id = "tchelvis"`;
  - `exec("cockpit", …)` → `exec("tchelvis", …)`;
  - layout node ids `tchelvis-*`;
  - visible product strings `Cockpit` → `Tchelvis`, `COCKPIT FLEET` → `TCHELVIS FLEET`;
  - **keep** `shared("cockpit.*")` keys and `cockpit.*` label names.

- [ ] **Step 1: Write failing test:** `runbooks_reads_tchelvis_dir`: a temp repo with `.tchelvis/runbooks/b.md` → discovered.
- [ ] **Step 2:** Run `cargo test --quiet runbooks`. Expected: FAIL.
- [ ] **Step 3: Implement** the rename. Update existing test expectations (paths, names) to the new values. Do not delete tests.
- [ ] **Step 4: Verify.** Run `cargo test --quiet && cargo build --release && ./target/release/tchelvis --version`. Expected: all tests pass, and it prints `tchelvis 0.2.0 (dev)`.
- [ ] **Step 5:** Run this audit:
  ```bash
  grep -rn "cockpit" src assets tests | grep -vE 'cockpit\.[a-zA-Z_]+|CockpitPaths'
  ```
  Expected: no remaining hits except `cockpit.*` label/shared keys and internal type names. List any others in the return.
- [ ] **Step 6:** Leave uncommitted.

### Task 3: _(dropped — no legacy migration, per the 2026-11-15 amendment)_

### Task 4: Stop/restart our daemons around `update` (`src/procs.rs`)

**Files:** Create `src/procs.rs`; modify `src/lib.rs`. Task 5 calls it: stop before running the new binary's `setup`, restart after it.

**Interfaces:**
- Produces:
  ```rust
  pub struct OurProc { pub pid: i32, pub argv: Vec<String> }
  pub fn find_our_processes(proc_root: &Path, uid: u32, exclude_pid: i32) -> Vec<OurProc>
     // argv0 basename == "tchelvis" && (argv1 == "daemon" || (argv1 == "supervisor" && argv2 == "watch")), status Uid == uid
  pub fn stop_processes(procs: &[OurProc], grace: Duration) -> Vec<i32>   // SIGTERM, wait ≤ grace, SIGKILL
  pub fn restart_args(argv: &[String]) -> Vec<String>                      // drop argv0, keep the rest
  pub fn restart(procs: &[OurProc], exe: &Path, log_dir: &Path) -> Result<Vec<String>> // detached (setsid via pre_exec), logs <log_dir>/<daemon|supervisor-watch>.log;
     // if `systemctl --user is-enabled tchelvis.service` succeeds, restart the unit instead of spawning a daemon
  ```
- Grace: 3 s. The update summary lists restarted processes and warns that they inherit the current shell's environment.

- [ ] **Step 1: Write failing tests** (fake `/proc` in a tempdir: `<pid>/cmdline` NUL-separated, `<pid>/status` with a `Uid:\t1000\t…` line):
  - `finds_daemon_and_watch_ignores_others`: `tchelvis daemon` ✓, `/home/u/.local/bin/tchelvis supervisor watch` ✓, `tchelvis status` ✗, `tchelvis supervisor tick` ✗, uid 0 ✗, `exclude_pid` ✗, `cockpit daemon` ✗.
  - `restart_args_drops_argv0`: `["tchelvis","supervisor","watch"]` → `["supervisor","watch"]`.
- [ ] **Step 2:** Run `cargo test --quiet procs`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run `cargo test --quiet`. Expected: PASS.
- [ ] **Step 5:** Leave uncommitted.

### Task 5: `tchelvis update` (`src/update.rs`)

**Files:** Create `src/update.rs`; modify `src/main.rs` (`Commands::Update { channel: Option<String>, check: bool, no_setup: bool, force: bool }`), `src/lib.rs`, `Cargo.toml` (`flate2 = "1"`, `tar = "0.4"`).

**Interfaces:**
- Consumes: `version::{build_channel, build_commit}` (Task 1).
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Channel { Stable, Nightly }
  impl Channel { pub fn parse(s: &str) -> Option<Self>; pub fn as_str(&self) -> &'static str }
  pub fn resolve_channel(flag: Option<Channel>, remembered: Option<Channel>) -> Channel   // flag > file > Stable
  pub fn channel_file() -> PathBuf                                                        // ~/.local/state/tchelvis/update_channel
  pub struct ReleaseInfo { pub tag: String, pub version: String, pub commit: Option<String>, pub asset_url: String, pub sha_url: String }
  pub fn parse_release(json: &serde_json::Value, channel: Channel, arch: &str) -> anyhow::Result<ReleaseInfo>
     // asset "tchelvis-<ver>-<arch>-unknown-linux-musl.tar.gz"; ver = tag without 'v' (stable) or "nightly"; commit from body regex `commit ([0-9a-f]{7,40})`
  pub fn needs_update(channel: Channel, rel: &ReleaseInfo, cur_version: &str, cur_channel: Option<&str>, cur_commit: Option<&str>, force: bool) -> bool
  pub fn verify_sha256(bytes: &[u8], sha_file_contents: &str) -> anyhow::Result<()>        // first token = hex
  pub fn extract_binary(tar_gz: &[u8]) -> anyhow::Result<Vec<u8>>                          // entry named "tchelvis"
  pub fn install_bytes(target: &Path, archive: &[u8], sha_file: &str) -> anyhow::Result<()> // verify → extract → temp in same dir → chmod 755 → rename
  pub fn run_update(channel_flag: Option<Channel>, check: bool, no_setup: bool, force: bool) -> anyhow::Result<()>
  ```

**Behavior** (spec §4):
- **HTTP:** `ureq`, header `User-Agent: tchelvis-updater`, endpoints `…/releases/latest` (stable) or `…/releases/tags/nightly`.
- **Stable 404:** error "No stable release yet — use `--channel nightly`".
- **`--check`:** print `current → available`, then `std::process::exit(0)` if an update exists, else 1.
- **After `install_bytes`:** write the channel file. Unless `--no-setup`:
  1. `procs::find_our_processes` + `stop_processes`;
  2. spawn the **new** binary at the same path with `setup --set-default-layout`, streaming its output;
  3. `procs::restart` what was stopped.
- **Summary:** old → new version and channel, then "Restart Herdr Desktop to reload the mod."
- **Failures:** before the rename, the installed binary is untouched. A setup failure prints the retry command `tchelvis setup --set-default-layout`.

- [ ] **Step 1: Write failing tests** (JSON fixtures in-test; tarballs built in-test with `tar` + `flate2`):
  - `parse_release_stable` and `parse_release_nightly_reads_commit`.
  - `parse_release_missing_asset_errors`: the error names the arch.
  - `needs_update_matrix`:
    - stable same version + channel stable → false;
    - stable newer → true;
    - nightly same commit → false;
    - nightly other commit → true;
    - `force` → true.
  - `resolve_channel_precedence`.
  - `install_bytes_replaces_atomically`: the target ends up with the new content and mode `0o755`.
  - `install_bytes_rejects_bad_sha_and_keeps_original` (Review Focus 3).
  - `install_bytes_unwritable_dir_errors_and_keeps_original` (Review Focus 2): chmod the dir `0o555`; skip when running as root.
  - `http_error_message_names_status` (Review Focus 4): a pure helper formats `HTTP 403 from <url>`.
- [ ] **Step 2:** Run `cargo test --quiet update`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run `cargo test --quiet`. Expected: PASS.
- [ ] **Step 5:** Leave uncommitted.

### Task 6: `install.sh`

**Files:** Create `install.sh` (repo root, `chmod +x`).

**Behavior** (spec §5):
- **Usage:** `sh install.sh [--channel stable|nightly] [--dir DIR]`. Defaults: `stable`, `$HOME/.local/bin`.
- **Arch:** `uname -m` → `x86_64` | `aarch64` (also accept `arm64`); anything else → error.
- **URLs:**
  - nightly: `https://github.com/fabriciof807/Tchelvis-Supervisor/releases/download/nightly/tchelvis-nightly-<arch>-unknown-linux-musl.tar.gz` (+ `.sha256`);
  - stable: resolve the tag via `https://api.github.com/repos/fabriciof807/Tchelvis-Supervisor/releases/latest` (`"tag_name"` parsed with `sed`). A 404 → "No stable release yet — use `--channel nightly`".
- **Download:** `curl -fsSL`, falling back to `wget -qO-`. Verify with `sha256sum -c`, extract to a `mktemp -d`, then `install -m 0755` into `--dir`.
- **After install:** warn if `--dir` is not on `PATH`. Run `"$DIR/tchelvis" setup --set-default-layout`, write `${XDG_STATE_HOME:-$HOME/.local/state}/tchelvis/update_channel`, and print the restart-Herdr note.
- POSIX `sh`, `set -eu`, no bashisms, no `sudo`. Clean up the temp dir with `trap`.

- [ ] **Step 1:** Run `sh -n install.sh` and `command -v shellcheck && shellcheck install.sh`. Expected: no errors. If shellcheck is not installed, report "not run".
- [ ] **Step 2:** Run `sh install.sh --channel bogus`. Expected: a usage error, exit ≠ 0.
- [ ] **Step 3:** Leave uncommitted. End-to-end validation happens after the first nightly exists (Task 7).

### Task 7: Release workflow

**Files:** Create `.github/workflows/release.yml`.

- **Triggers:** `push` to branches `[main]` and tags `['v*']`, plus `workflow_dispatch`. Top-level `permissions: contents: write`. `concurrency: { group: release-${{ github.ref }}, cancel-in-progress: false }`.
- **Job `test`:** `ubuntu-latest`, `cargo test --locked --quiet`.
- **Job `build`** (needs `test`), matrix:
  - `{ target: x86_64-unknown-linux-musl, os: ubuntu-latest }`;
  - `{ target: aarch64-unknown-linux-musl, os: ubuntu-24.04-arm }`.

  Steps:
  1. `sudo apt-get install -y musl-tools` (the runner's own sudo is fine);
  2. `rustup target add ${{ matrix.target }}`;
  3. `env: CC_<target_underscored>: musl-gcc`;
  4. `TCHELVIS_CHANNEL` = `stable` for tags, else `nightly`; `TCHELVIS_COMMIT` = short sha;
  5. `cargo build --release --locked --target …`, then strip;
  6. package `tchelvis-<ver>-<target>.tar.gz` (binary + `LICENSE-MIT` + `LICENSE-APACHE` + `README.md`, at the archive root) and `.sha256` (`sha256sum <file> > <file>.sha256`);
  7. `actions/upload-artifact`.
- **Job `publish`** (needs `build`): download the artifacts and build `SHA256SUMS`.
  - **Tag** `vX.Y.Z`: extract the `## [X.Y.Z]` section of `CHANGELOG.md` with `awk` into `notes.md`, then `gh release create "$TAG" --title "Tchelvis $TAG" --notes-file notes.md <assets>`.
  - **`main`:**
    ```bash
    gh release delete nightly --cleanup-tag --yes || true
    gh release create nightly --prerelease --target "$GITHUB_SHA" --title "Nightly" --notes "commit ${GITHUB_SHA::7} · $(date -u +%F)" <assets>
    ```
  - `GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}`.

- [ ] **Step 1:** Validate the YAML: `python3 -c "import yaml,sys;yaml.safe_load(open('.github/workflows/release.yml'))"`, or `actionlint` if available (report which ran).
- [ ] **Step 2:** Leave uncommitted. The real validation is the first nightly run after the coordinator pushes.

### Task 8: Docs

**Files:** `README.md`, `docs/agent-install.md`, `llms.txt`, `HANDOFF.md` §3, `CHANGELOG.md`, `ROADMAP.md`; new `docs/RELEASING.md`.
- **README:**
  - **Install:** `curl -fsSL https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/install.sh | sh -s -- --channel nightly` (and stable).
  - **Update:** `tchelvis update [--channel …]`.
  - Build from source stays as an alternative.
  - Every `cockpit` command becomes `tchelvis`, and env vars become `TCHELVIS_*`.
  - **License:** the dual license (badge + section).
- **`docs/agent-install.md`:** detect `tchelvis`. Procedure A = `install.sh` / `tchelvis update`. Update the file paths to the `tchelvis` names.
- **`CHANGELOG.md` Unreleased:**
  - **Changed (breaking):** binary `tchelvis`, dirs, env prefix, Herdr layout `tchelvis`. No migration: uninstall `cockpit`, then install `tchelvis`;
  - **Added:** `tchelvis update`, `install.sh`, stable/nightly releases, dual license.
- **`docs/RELEASING.md`:** the steps to cut stable (CHANGELOG section, version bump, `git tag vX.Y.Z`, push tag), and how nightly works.
- **`HANDOFF.md` §3:** replace the incus sync block with `tchelvis update --channel nightly` (first time: `install.sh --channel nightly`).
- **ROADMAP:** M5a status → implemented, awaiting the first nightly and host validation.

- [ ] **Step 1:** Run `grep -n "cockpit" README.md docs/agent-install.md llms.txt`. Expected: only `cockpit.*` label names and the CHANGELOG/uninstall note remain.
- [ ] **Step 2:** Leave uncommitted.
