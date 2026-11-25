# Loop Without Sandbox (`kind = "none"`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Loop stages can run as plain user processes (`[sandbox] kind = "none"`). Two gates in `loops.toml` must both be set, and the mode is visibly flagged in `check`, the 5 lines, `status` and `report.md`.

**Architecture:** A new `SandboxConfig::Unsandboxed` variant (the TOML value is `"none"`) plus a `NoSandbox` type implementing the existing `Sandbox` trait. The engine, preflight and free checks already take `&dyn Sandbox`, so they do not change. Per-provider differences go through `wrap_for_provider`. The flags are rendered from `state.sandbox == "none"`.

**Tech Stack:** Rust 2021, `serde`, `toml`, `libc`. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-25-loop-sandbox-none-design.md` (Portuguese; it decides behaviour). Parents: `2026-11-23-loop-engine-design.md` and `2026-11-24-loop-phase-b-pi-adapter-design.md`.

## Global Constraints

- TOML: `[sandbox] kind = "none"`, plus top-level `allow_unsandboxed = true`. Both are read only from `~/.config/tchelvis/loops.toml`. A missing `kind` still means `bwrap`. A failed sandbox is never replaced by another kind.
- Loading `kind = "none"` without `allow_unsandboxed = true` fails with, verbatim: `sandbox 'none' exige allow_unsandboxed = true em ~/.config/tchelvis/loops.toml: o agente roda sem isolamento, com seu HOME, sua rede e seus terminais`
- `auto_approve_in_sandbox = true` stays mandatory for `run` and `check --live`, in every kind.
- Kept in `none`: the isolated clone, the local branch `loop/<workflow>-<run-id>` (never merge or push), the clean env (`PATH HOME LANG TERM` plus `env_pass`), 5-line `stdout`, and the single wake message.
- Not in `none`: the loop's own `agy` login, `PI_CODING_AGENT_DIR` scratch dirs, and mounts. `pi` keeps the same argv as in every other kind.
- `OPENCODE_API_KEY` goes only to `opencode`. In `none` only, `agy` also gets `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR` (spec design Decision 3; user decision 5). It applies to stages, probes and free checks, because all three go through `wrap_for_provider`. No other provider gets them, and `agy` does not get them in `bwrap` or `inherited`.
- Exact user-visible strings:
  - Fixed first exposure in `none`: `SEM SANDBOX: o agente roda como o seu usuário, com HOME, rede, repositório principal, estado do tchelvis (~/.config, ~/.local/share e ~/.local/state/tchelvis) e terminais Herdr ao alcance`
  - Suffix of line 1 of the 5 lines and of `status`, when `state.sandbox == "none"`: ` | SEM SANDBOX`
  - `check` header in `none`: `Workflow '<wf>' válido (sandbox: none — SEM SANDBOX).`
  - `report.md` sandbox line in `none`: `- **Sandbox:** **SEM SANDBOX** (\`none\`): o agente rodou com o HOME, a rede e os poderes do seu usuário`
  - `check` warning when `allow_unsandboxed = true` with another kind: `aviso: allow_unsandboxed = true não tem efeito com sandbox '<kind>'`
  - First line of `loop auth` in `none`: `modo sem sandbox: os CLIs usam os logins normais do HOME`
  - `agy` account missing in `none`: `conta do 'agy' não encontrada; sem sandbox o agy usa o login normal (rode 'agy' e entre na conta)`
- Ticket coordinator rule, verbatim, in `src/orch/preamble.rs` under `## Running Tickets with Loops`, as the first sub-bullet of "On wake-up":
  ```markdown
    - Line 1 ends with `| SEM SANDBOX`: the loop ran without a sandbox, so its agent could have rewritten this result. Do not merge yet: show the ticket, `RUN_ID` and `BRANCH`, and ask the user. On yes, apply the rules below; on no, set `status: failed`.
  ```
- Output for `bwrap` and `inherited` stays byte-identical. Existing tests stay unchanged.
- User-visible text is in Portuguese. Code, docs and the CHANGELOG are in English.
- Tests use no network, no real `$HOME` (temp dirs only), no `bwrap`, and fake executables via `crate::test_support::write_executable`.
- Run for each task: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --locked`.
- The coordinator owns commits. A worker stops at "ready to commit".

## Review Focus

- A `loops.toml` with `kind = "none"` and `allow_unsandboxed = "yes"` (a string) or `1` must fail at load (serde type error), never be treated as true. Task 2.
- A `frozen.json` written before this change, with `Inherited` or `Bwrap`, must still load and run. Task 2.
- The run directory or workspace on a read-only path in `none` must fail in `check` with the workspace probe error, not later in a stage. Task 1.
- `agy` in `none` with neither `DBUS_SESSION_BUS_ADDRESS` nor `/run/user/<uid>/bus` must not get stray variables and must fail the account check with the exact message. Tasks 3 and 5.
- `status` and `result` for a run whose `state.json` was written by a `bwrap` run must stay unchanged; only `"none"` adds the suffix. Task 4.

## Dependencies and delegation

```
T1 (sandbox.rs) ── T2 (config.rs, cli.rs) ── T3 (sandbox.rs, cli.rs) ── T4 (state.rs, report.rs, cli.rs) ── T5 (cli.rs, free_check.rs) ── T7 (docs) ── T8 (ai-box)
T6 (src/orch/preamble.rs, tickets spec) ─────────────────────────────────────────────────────────────────────────┘
```

- T1 and T3 share `sandbox.rs`. T2, T3, T4 and T5 share `cli.rs`. Everything is sequential, so use one implementer in one worktree.
- The only part that could run in parallel is the `state.rs`/`report.rs` half of T4, which is independent of T1–T3. It is too small to be worth a second worktree.
- T6 shares no file with T1–T5 and can run in parallel in its own worktree, or last in the same one. It only needs the ` | SEM SANDBOX` string fixed in Global Constraints.
- T8 runs on the ai-box with the coordinator.

---

### Task 1: `NoSandbox`

**Files:**
- Modify: `src/loops/sandbox.rs:324-503`. Extract `pub(crate) fn host_exposures(root: &Path) -> Vec<String>` from `InheritedSandbox::check`, `:452-484` (sockets, `sudo -n`, `hc`). `InheritedSandbox::check` calls it and keeps its output identical.
- Create, in the same file: `NoSandbox`.
- Test: `src/loops/sandbox.rs` (tests module, `:986-`).

**Interfaces:**
- Produces:
  - `pub const UNSANDBOXED_EXPOSURE: &str` (the fixed string from Global Constraints).
  - `pub struct NoSandbox { pub env_pass: Vec<String>, pub root: PathBuf, pub agy_session_env: Vec<(String, String)> }`
  - `impl NoSandbox { pub fn new(env_pass: Vec<String>, agy_session_env: Vec<(String, String)>) -> Self }`, with `root = "/"`.
  - `impl Sandbox for NoSandbox`, whose `kind()` is `"none"`.
  - `pub fn unsandboxed_agy_env(env: &dyn Fn(&str) -> Option<String>, uid: u32, root: &Path) -> Vec<(String, String)>`. It returns the two variables when they are set. When they are not, and `<root>/run/user/<uid>/bus` exists, it returns `XDG_RUNTIME_DIR=/run/user/<uid>` and `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/<uid>/bus`. Otherwise it returns empty.

- [ ] **Step 1: Write the failing tests:**
  - `no_sandbox_check_needs_no_container_and_lists_fixed_exposure_first`. The fake `root` is a tempdir with no container marker and a writable `var/run/docker.sock` file. `check(workspace)` gives `Ok(v)` where `v[0] == UNSANDBOXED_EXPOSURE`, `v` contains `socket gravável acessível: /var/run/docker.sock`, and no item starts with `montagem gravável`.
  - `no_sandbox_check_fails_on_unwritable_workspace`. The workspace is a path under a `0o500` dir, and the result is `Err`.
  - `no_sandbox_wrap_keeps_argv_and_cleans_env`. With `env_pass = ["ALLOWED_API_KEY"]` (mirror `:1128`), argv is unchanged, `cwd` is the workspace, and `env` keys are a subset of `PATH HOME LANG TERM ALLOWED_API_KEY`.
  - `unsandboxed_agy_env_rules`. Env vars set: both returned. Unset, with the socket under a fake root: the two derived values. Neither: empty.
  - `inherited_check_output_is_unchanged`. Keep the existing inherited tests green; add no assertion changes.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::sandbox` and expect FAIL (`NoSandbox` is undefined).
- [ ] **Step 3: Implement.** `check` runs `probe_workspace_writable(workspace)?`, then builds `vec![UNSANDBOXED_EXPOSURE]` and extends it with `host_exposures(&self.root)`. `wrap` mirrors `InheritedSandbox::wrap` (`:489-499`).
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): NoSandbox type for unsandboxed stages`.

### Task 2: Configuration, the two gates and wiring

**Files:**
- Modify: `src/loops/config.rs:16-19` (variant `Unsandboxed`), `:59-67` and `:69-83` (`allow_unsandboxed: bool`, `#[serde(default)]`), `:230-241` (`"none"` arm and gate), and `:255-262` (copy the field).
- Modify: `src/loops/cli.rs:782-787` (`build_sandbox`), `:3236-3241` (engine sandbox from frozen), and `:2062-2071` (`cmd_auth` default config gets `allow_unsandboxed: false`). Also `:1636-1655` (template comment, Global Constraints wording) and `cmd_check` (`:2538-`): print the `allow_unsandboxed` warning when it is true and the sandbox is not `Unsandboxed`.
- Test: `src/loops/config.rs` (`:308-`) and `src/loops/cli.rs`.

**Interfaces:**
- Consumes: `NoSandbox::new`, `unsandboxed_agy_env` (Task 1).
- Produces:
  - `SandboxConfig::Unsandboxed` and `LoopsConfig.allow_unsandboxed: bool`.
  - `pub const UNSANDBOXED_GATE_ERROR: &str` (the verbatim load error).
  - `fn sandbox_from_config(sandbox: &SandboxConfig, env_pass: &[String], paths: &CockpitPaths) -> Box<dyn Sandbox + Sync>`, used by both `build_sandbox` and the engine path. For `Unsandboxed` it builds `NoSandbox::new(env_pass, unsandboxed_agy_env(&|k| std::env::var(k).ok(), unsafe { libc::geteuid() }, Path::new("/")))`.
  - `fn allow_unsandboxed_warning(cfg: &LoopsConfig) -> Option<String>`, which is pure.

- [ ] **Step 1: Write the failing tests:**
  - `none_requires_allow_unsandboxed`. `kind="none"` alone, or with `allow_unsandboxed=false`: `Err` whose text equals `UNSANDBOXED_GATE_ERROR`. With `allow_unsandboxed=true`: `sandbox == Unsandboxed`. `allow_unsandboxed="yes"` gives `Err`.
  - `allow_unsandboxed_has_no_effect_elsewhere`. `kind="bwrap"` with `allow_unsandboxed=true` loads as `Bwrap`, and `allow_unsandboxed_warning` returns the exact warning with `'bwrap'`. With `Unsandboxed` it returns `None`.
  - `bwrap_is_the_default_sandbox` (`:445`) stays green.
  - `frozen_config_roundtrips_unsandboxed_and_old_variants`. Serialize and deserialize a `FrozenRunConfig` with `Unsandboxed`. A literal JSON with `"sandbox":"Inherited"` still parses.
  - `sandbox_from_config_kinds`. `Unsandboxed` gives `kind() == "none"`, `Inherited` gives `"inherited"`.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::config loops::cli` and expect FAIL.
- [ ] **Step 3: Implement.** The gate goes in the `match` at `:230`: `"none" => { if !parsed.allow_unsandboxed { bail!("{UNSANDBOXED_GATE_ERROR}") } SandboxConfig::Unsandboxed }`. `ensure_sandbox_prereqs` (`cli.rs:794`) and the `if let SandboxConfig::Bwrap` blocks (`:2439`, `:2454`, `:2765`) need no change.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): kind = "none" behind allow_unsandboxed`.

### Task 3: Per-provider environment and `pi` without managed install

**Files:**
- Modify: `src/loops/sandbox.rs:61-80` (trait: `fn agy_session_env(&self) -> &[(String, String)] { &[] }`; `NoSandbox` returns its field), and `:506-561` (`wrap_for_provider`: when `provider == Agy && sandbox.kind() == "none"`, `apply_extra_env` with `agy_session_env()`; no other change).
- Modify: `src/loops/cli.rs:958-971` (`pi_check_notes` gains `sandbox_kind: &str`; it fails on an unrecognised install only when `sandbox_kind == "bwrap"`). Update its callers at `:2429` (`cmd_check`), `:2858` (`cmd_run`) and the tests at `:4266-4275`.
- Test: `src/loops/sandbox.rs` and `src/loops/cli.rs`.

**Interfaces:**
- Consumes: `NoSandbox.agy_session_env` (Task 1).
- Produces: `Sandbox::agy_session_env` and `pi_check_notes(workflow, install, sandbox_kind)`.

- [ ] **Step 1: Write the failing tests:**
  - `wrap_for_provider_under_no_sandbox`. Use a `NoSandbox` with `agy_session_env = [("DBUS_SESSION_BUS_ADDRESS","unix:path=/x"),("XDG_RUNTIME_DIR","/r")]` and `opencode_api_key = Some("k")`:
    - `Agy`: env has both D-Bus vars and no `OPENCODE_API_KEY`.
    - `Pi`: neither D-Bus var nor `PI_CODING_AGENT_DIR`, and no `pi-agent` dir was created in `scratch`.
    - `Opencode`: env has the key and no D-Bus var.
    - `Claude`: none of the three.
  - `inherited_agy_gets_no_dbus`. `InheritedSandbox` with `Agy` and `DBUS_SESSION_BUS_ADDRESS` in the process env gives no D-Bus vars (it is not in `PATH HOME LANG TERM`). Under `BwrapSandbox` (built, never run) the env has no D-Bus vars either.
  - `pi_check_notes_by_kind`. Unrecognised install with `"bwrap"` gives `Err(PI_UNRECOGNIZED)`. With `"none"` and `"inherited"` it gives `Ok` containing the `auth.json` warning.
  - Update the existing `check_warns_pi_credentials_and_fails_unrecognized_install` call sites to pass `"bwrap"`; the expectations stay the same.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::sandbox loops::cli` and expect FAIL.
- [ ] **Step 3: Implement** as listed under Files.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): provider env and pi checks for the unsandboxed kind`.

### Task 4: Visible "SEM SANDBOX" in the 5 lines, `status`, `report.md` and `check`

**Files:**
- Modify: `src/loops/state.rs:316-356` (`format_result`), `src/loops/report.rs:198`, and `src/loops/cli.rs:3339-3341` (`cmd_status` line 1) and `:2538-2549` (`cmd_check` header).
- Test: `src/loops/state.rs` (`:359-`), `src/loops/report.rs` (`:412-`) and `src/loops/cli.rs`.

**Interfaces:**
- Produces: `pub fn result_line_one(status: &RunStatus, workflow: &str, cycle: u32, max: u32, sandbox: &str) -> String` in `state.rs`, used by `format_result` and `cmd_status`. It appends ` | SEM SANDBOX` only for `"none"`. Also `fn check_header(workflow: &str, kind: &str) -> String` in `cli.rs`.

- [ ] **Step 1: Write the failing tests:**
  - `format_result_flags_unsandboxed_runs`. Using `sample_state` with `sandbox = "none"` for every status: 5 lines, `lines[0].ends_with(" | SEM SANDBOX")`, and `lines[0].starts_with("STATUS: ")`. With `"bwrap"`, the output equals today's (compare against the literal format from `:353-356`).
  - `format_result_is_five_lines_for_every_status` (`:394`) stays green.
  - `report_flags_unsandboxed` (in `report.rs`). With `sandbox: "none"`, the report contains the exact sandbox line from Global Constraints. The existing `bwrap` report test at `:421` stays unchanged.
  - `check_header_texts`. `check_header("wrc","none")` gives `Workflow 'wrc' válido (sandbox: none — SEM SANDBOX).`, and `check_header("wrc","bwrap")` gives `Workflow 'wrc' válido (sandbox: bwrap).`
- [ ] **Step 2: Run** `cargo test --locked --lib loops::state loops::report loops::cli` and expect FAIL.
- [ ] **Step 3: Implement.** Do not touch the record-only output at `cli.rs:3383` and `:3409-3412`; it has no sandbox kind.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): flag unsandboxed runs in result, status, report and check`.

### Task 5: `loop auth`, `agy` login and accounts in `none`

**Files:**
- Modify: `src/loops/cli.rs:2055-2176` (`cmd_auth` status), `:2178-2230` (login: `agy` under `Unsandboxed` gives argv `["agy"]`, with no `bwrap`, no `AGY_CLI_FORCE_FILE_TOKEN_STORAGE` and no loop login dir), and `:1185-1200` (`build_free_checks_for_targets`, `Agy` arm).
- Modify: `src/loops/free_check.rs:16` (new `pub const AGY_ABSENT_REASON_UNSANDBOXED`, the verbatim Global Constraints string) and `:430-440` (pick the reason by `sandbox.kind()`).
- Test: `src/loops/cli.rs` and `src/loops/free_check.rs`.

**Interfaces:**
- Consumes: `Sandbox::kind()`, `SandboxConfig::Unsandboxed` (Task 2).
- Produces:
  - `fn agy_login_argv(cfg_sandbox: &SandboxConfig, bwrap: Option<&Path>, login_dir: &Path, target_dir: &Path) -> Result<Vec<String>, LoopError>`, which is pure, extracted from the current `agy` arm.
  - `fn auth_header(kind: &str) -> Option<&'static str>`, which returns the Global Constraints line for `"none"`.
  - `build_free_checks_for_targets` gains `sandbox_kind: &str`.

- [ ] **Step 1: Write the failing tests:**
  - `agy_login_argv_by_kind`. `Unsandboxed` gives exactly `["agy"]` even when `bwrap` is `None`. `Bwrap` gives today's argv, which contains `AGY_CLI_FORCE_FILE_TOKEN_STORAGE=1`. `Bwrap` with no `bwrap` binary gives today's error.
  - `agy_account_unsandboxed_uses_list_or_file`. With `sandbox_kind = "none"`, no token file and an outcome with `list_known = true`, `account_ok` is `Ok`. With no file and `list_known = false`, it is `Err(AGY_ABSENT_REASON_UNSANDBOXED)`. With `"bwrap"` the behaviour is today's (`Err(AGY_ABSENT_REASON)` without a file).
  - `free_check_agy_reason_by_kind`. A fake `agy` prints nothing and exits 0, there is no credential, and the sandbox is a fake with `kind() == "none"`: the reason is `AGY_ABSENT_REASON_UNSANDBOXED`.
  - `auth_header_only_for_none`.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::cli loops::free_check` and expect FAIL.
- [ ] **Step 3: Implement.** In `cmd_auth` status, print `auth_header` first. When `<state>/loops/agy-login` exists under `none`, add the line `agy:      login próprio do loop em <state>/loops/agy-login não é usado sem sandbox`.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): normal logins in the unsandboxed kind`.

### Task 6: Ticket coordinator asks before merging an unsandboxed run

**Files:**
- Modify: `src/orch/preamble.rs:156-160` (the skill text; `orchestration_skill_markdown()` at `:56` is the single source that `tchelvis setup` installs for every agent and that `tchelvis orch skill` prints) and `:279-325` (test).
- Modify: `docs/superpowers/specs/2026-11-24-tickets-coordinator-design.md:92-99` (section 2, "Ao acordar"): add the same rule in Portuguese as the first sub-bullet:
  ```markdown
    - Linha 1 terminando em `| SEM SANDBOX`: o loop rodou sem sandbox e o agente pode ter reescrito o resultado; não junta sozinho: mostra ticket, `RUN_ID` e `BRANCH` e pergunta. Com o sim, seguem as regras abaixo; sem ele, `status: failed`.
  ```
- Test: `src/orch/preamble.rs`.

**Interfaces:**
- Consumes: the ` | SEM SANDBOX` suffix (Task 4, string fixed in Global Constraints).

- [ ] **Step 1: Failing test.** In `orchestration_skill_documents_running_tickets`, add `"| SEM SANDBOX"`, `"could have rewritten this result"` and `"ask the user. On yes, apply the rules below"` to the needles. Add `unsandboxed_rule_comes_before_merge_rule`: in the section, the index of `| SEM SANDBOX` is less than the index of `merge --no-ff <branch>` in the "Every name inside" bullet. The section stays at 70 lines or fewer (existing assertion).
- [ ] **Step 2: Run** `cargo test --locked --lib orch::preamble` and expect FAIL.
- [ ] **Step 3: Insert** the Global Constraints line as the first sub-bullet under "On wake-up (one per loop)", before "Every name inside the ticket's `files`". Then edit the tickets spec as listed.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(orch): ask before merging a loop that ran without a sandbox`.

### Task 7: Docs

**Files:** `docs/superpowers/specs/2026-11-23-loop-engine-design.md` (line 84, the `kind` comment gets `none`; the Sandbox table at `:94-98` gets a row `| 4 | none | Processo comum do usuário; exige allow_unsandboxed = true | Nada (ver 2026-11-25-loop-sandbox-none-design.md) |`), `README.md:323-326`, `docs/TCHELVIS-ORCH.md` (loop section near `:213`), `CHANGELOG.md` (`[Unreleased]`), and `ROADMAP.md:175` and its M11 notes.

- [ ] **Step 1:** Engine spec: the edits above, and nothing else in that spec.
- [ ] **Step 2:** README and `TCHELVIS-ORCH.md` get one bullet each. It covers `kind = "none"`, the two gates, ` | SEM SANDBOX` on line 1, the normal logins (including `agy` with D-Bus, `agy` only and only in `none`), that the ticket coordinator asks before merging a ` | SEM SANDBOX` run, and the accepted limits (the agent reaches the tchelvis state, the main repository, `hc` and `sudo`).
- [ ] **Step 3:** CHANGELOG `Added` gets one bullet. ROADMAP M11 gets "unsandboxed kind (`none`) implemented (ai-box validation pending)".
- [ ] **Step 4:** Run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `docs: unsandboxed loop kind`.

### Task 8: Real validation (ai-box; not CI)

Use the release build and a throwaway repo with `loops.toml` `kind = "none"`, `auto_approve_in_sandbox = true` and `allow_unsandboxed = true`.

- [ ] **Step 1:** Remove `allow_unsandboxed` and expect `loop check` to fail with the exact gate error. Restore it, and expect `check` to show the header and the fixed exposure first.
- [ ] **Step 2:** `loop run` a one-stage `claude` workflow in the foreground. Expect 5 lines with line 1 ending ` | SEM SANDBOX`, `report.md` flagged, and the branch created. The user's checkout is untouched.
- [ ] **Step 3:** `agy` stage with the keyring login (no loop login, no token file) passes `check --live` and a run. Then repeat with `DBUS_SESSION_BUS_ADDRESS` unset and no bus socket, and expect the account check to fail with the exact message.
- [ ] **Step 4:** `pi` stage: passes, and no `pi-agent` dir appears under the run's logs.
- [ ] **Step 5:** `--detach` from a Herdr terminal: the caller is woken once, and `loop result` prints the flagged 5 lines.
- [ ] **Step 6:** Check the D-Bus scope: in the stage logs (or with a fake `env` stage, `env > .tchelvis-loop/out/env.txt`), `DBUS_SESSION_BUS_ADDRESS` appears only in the `agy` stage. In a `bwrap` run of the same workflow it appears in no stage.
- [ ] **Step 7:** Ticket coordinator: run `/orchestration-tchelvis` over one ticket with `kind = "none"`. On the wake-up it must stop and ask before merging. Answer no and expect `status: failed`; rerun, answer yes, and expect the merge.
- [ ] **Step 8:** Record the results with dates in the spec's "Testes" section and in ROADMAP.
