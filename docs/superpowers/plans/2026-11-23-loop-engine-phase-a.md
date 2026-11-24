# Loop Engine (M11, phase A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tchelvis loop` runs a fixed multi-agent workflow in clean headless sessions inside a sandbox and wakes the caller once.

**Architecture:** New module tree `src/loops/`. Pure parts (workflow, verdict, state machine, report) know nothing about processes; `Sandbox`, `StageRunner` and `Workspace` are traits defined in `engine.rs`. The engine is a normal process in one Herdr terminal; the daemon only detects the end and writes one message to the existing `orch` mailbox.

**Tech Stack:** Rust 2021, `toml` 0.8, `serde_json`, `libc`, `sha2`, `clap`, `rusqlite`; system `git`. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-23-loop-engine-design.md` (Portuguese; it decides behaviour).

## Global Constraints

- Outside the sandbox, never run git with the workspace's `.git`. Engine git always sets `GIT_DIR` (main repository), `GIT_WORK_TREE` (workspace), `GIT_INDEX_FILE` (run) and `-c core.hooksPath=/dev/null -c core.fsmonitor=false`, plus `--no-ext-diff --no-textconv` on diffs.
- `loops.toml` is read only from `~/.config/tchelvis/`; a repository can never set sandbox, env or auto-approval.
- No stage runs unless the sandbox check passed **and** `auto_approve_in_sandbox = true`. No fallback between sandbox kinds.
- `stdout` of `run` (without `--verbose`) and `result`: at most 5 lines, in every outcome.
- Exit codes: PASS 0, ERROR 1, usage 2, ESCALATE 3, CANCELLED 4.
- Never merge, push, delete a branch, or touch the user's checkout or `.gitignore`.
- Files from agents: regular files only, no symlink in any component, ≤ 1 MiB, UTF-8.
- User-visible text in Portuguese; code, docs and CHANGELOG in English. Public repository: fixtures contain no personal path or id.
- Tests: no network, no real agent CLI, no wall-clock sleeps for logic (inject a clock); executable scripts through `tests/common` `write_executable`.
- Per task: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`.

## Review Focus

- The agent replaces `.tchelvis-loop/out` (or a parent) with a symlink: nothing outside is read or written. Test in Task 3.
- A stage leaves a background process that called `setsid`: it dies with the stage. Test in Task 4.
- A stage prints megabytes on one line, or invalid UTF-8: bounded memory, no crash, log kept. Test in Task 4.
- Disk full or run directory removed mid-run: the run still ends as ERROR, never hangs. Test in Task 6.
- The caller's terminal was closed and its address reused: the new terminal is not woken. Test in Task 7.

## Files

| File in `src/loops/` | Responsibility |
|---|---|
| `workflow.rs` | TOML schema, validation, presets and roles (`assets/loops/`) |
| `config.rs` | `loops.toml` |
| `verdict.rs` | verdict parser, 5-line output |
| `state.rs` | `state.json`, usage totals |
| `workspace.rs`, `artifacts.rs` | clone, snapshots, branch; safe file exchange |
| `sandbox.rs`, `process.rs` | command wrapping and checks; process tree, activity, timeouts |
| `adapters.rs` | `claude`, `agy`, `opencode` |
| `engine.rs`, `report.rs` | state machine; `report.md` |
| `watch.rs`, `cli.rs` | daemon side; commands |

---

### Task 1: Workflow and configuration (pure)

**Files:** Create `src/loops/{mod,workflow,config}.rs`, `assets/loops/workflows/*.toml` (4), `assets/loops/roles/*.md` (6); Modify `src/lib.rs`. Tests inside the modules.

**Produces:**
```rust
pub enum Provider { Claude, Agy, Opencode }
pub struct Stage { pub id: String, pub role: String, pub provider: Provider, pub model: Option<String>,
    pub reads: Vec<String>, pub writes: Option<String>, pub edits: bool, pub next: Option<String>,
    pub verdict_gate: bool, pub on_pass: Option<String>, pub on_retry: Option<String> }
pub struct Workflow { pub name: String, pub description: String, pub max_cycles: u32,
    pub stage_timeout_secs: u64, pub stall_secs: u64, pub max_duration_secs: u64, pub stages: Vec<Stage> }
pub fn parse_workflow(raw: &str, defaults: &RoleDefaults) -> Result<Workflow, Vec<String>>; // every problem, not the first
pub struct LoopsConfig { pub max_concurrent: u32, pub auto_approve_in_sandbox: bool, pub env_pass: Vec<String>,
    pub sandbox: SandboxConfig, pub defaults: RoleDefaults }
pub enum SandboxConfig { Inherited, Bwrap { home_ro: Vec<String> } }
pub fn parse_loops_config(raw: &str) -> Result<LoopsConfig>;
```
Defaults: `max_cycles 3`, `stage_timeout_secs 1800`, `stall_secs 600`, `max_duration_secs 7200`, `max_concurrent 2`. `[sandbox] kind` is required; `incus` gives "ainda não suportado nesta versão". `home_*` entries: relative, no `..`.

- [ ] **Step 1: Failing tests.** `valid_example_parses` (the spec's example verbatim). `each_rule_is_reported`, one table row per rule and the id it must mention: duplicate id; id not `[a-z0-9-]{1,32}`; unknown `next`/`on_pass`/`on_retry`; unreachable stage; no path to `done`; cycle without a gate; gate without `writes`; gate with `next`; non-gate with `on_pass`/`on_retry`; non-gate without `next`; artifact name with `/`, `..`, leading `.`, or reserved (`input.md`, `current.diff`, `state.json`, `report.md`); `reads` nobody produces; unknown provider; provider missing with no default; unknown key; zero limits. `two_problems_are_both_reported`. `presets_validate_with_defaults` and `presets_without_defaults_name_the_missing_role`. `config_rejects_incus_unknown_keys_and_escaping_home_paths`.
- [ ] **Step 2:** run → FAIL. **Step 3:** implement. **Step 4:** validation → PASS.
- [ ] **Step 5: Commit** `feat(loop): workflow and configuration`

Roles follow spec §"Workflow" of the user's original text: designer/planner plan only; worker implements and fixes `review.md`/`critic.md`; tester writes and runs tests; reviewer reports real bugs only; critic audits. Each role ends by naming its output file; gates end with the `VERDICT:` line.

---

### Task 2: Verdict, output and state (pure)

**Files:** Create `src/loops/{verdict,state}.rs`.

**Produces:**
```rust
pub enum Verdict { Pass, Retry }
pub fn parse_verdict(text: &str) -> Option<Verdict>;      // last non-empty line
pub fn summary_line(text: &str) -> String;                // line before the verdict, sanitized, ≤ 140 chars
pub enum RunStatus { Running, Pass, Error, Escalate, Cancelled } // + exit_code(), is_final()
pub struct Usage { pub turns: Option<u64>, pub input_tokens: Option<u64>, pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>, pub cost_usd: Option<f64> }
pub struct Total { pub value: u64, pub partial: bool }
pub struct RunState { schema_version, run_id, workflow, status, reason, sandbox, current_cycle, max_cycles, branch,
    base_commit, engine_pid, engine_start_ticks, caller, heartbeat_at_ms, last_activity_at_ms, started_at_ms,
    finished_at_ms, executions: Vec<Execution>, files: Vec<String>, summary }
pub fn save_state(dir: &Path, s: &RunState) -> Result<()>; // temp file + fsync + rename
pub fn format_result(s: &RunState) -> String;              // exactly 5 lines
```

- [ ] **Step 1: Failing tests.** `parse_verdict`: `VERDICT: PASS`, ` **VERDICT: RETRY** `, `` `verdict: pass` ``, trailing blank lines → parsed; `VERDICT: PASS` not on the last line, `VERDICT: MAYBE`, `VERDICT: PASS RETRY`, empty → `None`. `format_result_is_five_lines_for_every_status`, also with newlines, ANSI escapes, 500 files and a 1 MB summary (files: first 10 then ` (+490)`). `totals_with_null_are_partial` → `≥ 18.420`. `state_round_trips_and_survives_a_leftover_temp_file`. `unknown_schema_version_is_an_error`.
- [ ] **Steps 2–4** as usual. **Step 5: Commit** `feat(loop): verdict, state and five-line result`

---

### Task 3: Workspace and artifacts (real git, no agents)

**Files:** Create `src/loops/{workspace,artifacts}.rs`.

**Produces:**
```rust
pub trait Workspace { fn snapshot(&mut self) -> Result<String>; fn restore(&mut self, tree: &str) -> Result<()>;
    fn diff(&self, tree: &str) -> Result<String>; fn changed_files(&self, tree: &str) -> Result<Vec<String>>;
    fn publish(&mut self, tree: &str, message: &str) -> Result<()>; fn path(&self) -> &Path; }
pub struct GitWorkspace; // create(repo, run_dir, branch, from_head) -> Result<Self>
pub fn prepare_exchange(workspace: &Path, reads: &[(String, PathBuf)]) -> Result<()>;
pub fn collect_output(workspace: &Path, name: &str) -> Result<String>;
```
Clone with `--no-hardlinks --template=`, remove `origin`. Open artifacts relative to a directory fd with `openat` + `O_NOFOLLOW` per component, then `fstat`.

- [ ] **Step 1: Failing tests** (temporary repositories, `GIT_CONFIG_GLOBAL=/dev/null`):
  - `hostile_workspace_runs_nothing`: plant `core.fsmonitor`, `diff.external`, `core.hooksPath`, `pre-commit`, `post-index-change`, a `filter.x.clean` in the workspace `.git/config` with `.gitattributes`; after `snapshot`, `diff`, `changed_files`, `restore`, `publish`, no marker file exists.
  - `snapshot_sees_new_files_and_agent_commits_but_not_ignored_or_exchange_files`.
  - `restore_undoes_edits_additions_and_deletions`.
  - `publish_creates_the_branch_and_leaves_checkout_index_and_head_untouched`; `existing_branch_is_refused`.
  - `dirty_repository_is_refused_unless_from_head`; `nested_repository_is_an_error_naming_the_path`; `repository_with_tchelvis_loop_dir_is_refused`; `repository_without_commits_is_a_clear_error`.
  - `symlinked_output_parent_or_exchange_dir_is_refused` (file, `out`, `.tchelvis-loop` each a symlink); FIFO, directory, 1 MiB + 1, invalid UTF-8 refused; `prepare_exchange` never writes through a symlink.
- [ ] **Steps 2–4.** **Step 5: Commit** `feat(loop): isolated workspace and safe artifacts`

---

### Task 4: Sandbox and process control

**Files:** Create `src/loops/{sandbox,process}.rs`.

**Produces:**
```rust
pub struct SandboxedCommand { pub argv: Vec<OsString>, pub env: Vec<(OsString, OsString)>, pub cwd: PathBuf }
pub trait Sandbox { fn kind(&self) -> &'static str; fn check(&self, workspace: &Path) -> Result<Vec<String>>; // exposures found
    fn wrap(&self, argv: &[OsString], workspace: &Path) -> Result<SandboxedCommand>; }
pub struct Limits { pub timeout: Duration, pub stall: Duration }
pub enum Outcome { Exited(i32), Signaled(i32), TimedOut, Stalled, Cancelled }
pub fn run_process(cmd: SandboxedCommand, stdin: &[u8], log: &Path, limits: Limits, clock: &dyn Clock,
    cancel: &AtomicBool, on_line: &mut dyn FnMut(&str), probe_activity: &mut dyn FnMut() -> bool) -> Result<Outcome>;
```
- `inherited`: container when `/run/systemd/container`, `/.dockerenv` or `/run/.containerenv` exists (root injected for tests). Exposures: writable Docker/Incus sockets, `sudo -n true`, `hc` on PATH, writable mounts shared from outside.
- `bwrap` argv: `--die-with-parent --unshare-pid --unshare-ipc --unshare-uts --new-session --ro-bind / / --dev /dev --proc /proc --tmpfs /run --tmpfs /tmp --tmpfs $HOME`, `--ro-bind` for existing `home_ro` and for the executable and credential of the CLI in use, `--bind` for the workspace only, `--clearenv` + `--setenv`, `--chdir`. Network stays. `--ro-bind-try /run/systemd/resolve` (without it DNS fails). Verified on bubblewrap 0.9.0 inside an unprivileged Incus container; `home_ro` entries may be files as well as directories. `check` reports that the network, `localhost` services and abstract sockets are shared.
- The engine is a child subreaper (`PR_SET_CHILD_SUBREAPER`). Ending a stage: SIGTERM to the group, 5 s, SIGKILL to group and every descendant until none.
- Lines over 1 MiB are truncated for parsing; the log keeps at most 64 MiB with a note. Activity = output bytes or `probe_activity`.

- [ ] **Step 1: Failing tests** (scripts as fake agents): exit code and signal; `timeout`, `stall` and `cancel` end the tree, including a grandchild that called `setsid` (assert pid gone); silent process with workspace activity is not stalled; 5 MB single line and invalid UTF-8; stdin larger than a pipe buffer with a child that never reads; environment contains only the allowed names. `bwrap_argv_is_exact` (pure). `bwrap_check_blocks_outside_reads_and_writes`: runs only if `bwrap` works, otherwise prints `SKIPPED: bwrap unavailable` (reported in the return, never silently). `inherited_requires_a_container`.
- [ ] **Steps 2–4.** **Step 5: Commit** `feat(loop): sandbox and process control`

---

### Task 5: Adapters

**Files:** Create `src/loops/adapters.rs`, `tests/fixtures/loop/*.jsonl` (the sanitized captures named in the brief).

**Produces:** `pub fn command(p: Provider, model: Option<&str>, prompt: &str) -> AdapterCommand { argv, stdin }`, `pub struct EventParser` with `fn new(Provider)`, `fn line(&mut self, &str)`, `fn finish(self) -> AdapterResult { usage: Usage, error: Option<String> }`, `pub fn build_prompt(role: &str, stage: &Stage, available_reads: &[String]) -> String`.

| Provider | argv (model appended when set) | prompt |
|---|---|---|
| claude | `claude -p --output-format stream-json --verbose --no-session-persistence --strict-mcp-config --setting-sources project --permission-mode bypassPermissions [--model M]` | stdin |
| agy | `agy -p <prompt> --output-format stream-json --print-timeout 0 --dangerously-skip-permissions [--model M]` | argument; error above 100 KiB |
| opencode | `opencode run --standalone --format json --auto [-m M]` | stdin |

| Provider | Usage | Error |
|---|---|---|
| claude | `result`: `num_turns`, `usage.{input_tokens,output_tokens,cache_read_input_tokens}`, `total_cost_usd` | `is_error`, or no `result` |
| agy | `result.usage.{input_tokens,output_tokens,cache_read_tokens}`; turns = `step_update` with `DONE` + `agent_response`; cost `null` | `result.status != SUCCESS`, or no `result` |
| opencode | sum over `step_finish.part`: `tokens.{input,output,cache.read}`, `cost`; turns = count | `type == error` |

- [ ] **Step 1: Failing tests.** `argv_is_exact` per provider. `ok_fixtures`: claude turns 2, in 4, out 129, cache 7000, cost 0.0010269; agy turns 2, in 16537, out 296, cache 12092, cost `None`; opencode turns 2, in 14995, out 99, cache 15616, cost 0. `bad_model_fixtures_report_an_error`. `garbage_missing_or_renamed_fields_become_null_without_failing`. `error_text_is_redacted_and_single_line`.
- [ ] **Steps 2–4.** **Step 5: Commit** `feat(loop): claude, agy and opencode adapters`

---

### Task 6: Engine and report

**Files:** Create `src/loops/{engine,report}.rs`.

**Produces:** `pub trait StageRunner { fn run(&mut self, req: StageRequest<'_>) -> StageResult; }`, `pub struct Engine<W: Workspace, R: StageRunner, C: Clock>` with `fn run(self) -> RunState`, `pub fn render_report(state: &RunState, dir: &Path) -> String`.

Per stage: prepare exchange → run → collect `writes` (kept also as `c<cycle>-<name>`) → snapshot → if not `edits` and the tree changed, restore and ERROR. `current.diff` is refreshed before each stage that reads it. The final tree is published in every outcome with changes.

- [ ] **Step 1: Failing tests** with fakes: `pass_on_first_cycle`; `retry_then_pass_sums_usage`; `third_retry_escalates_and_runs_exactly_three_gates`; `malformed_verdict_reruns_only_the_gate_once_then_errors`; `missing_output_is_an_error`; `reviewer_that_edits_is_reverted_and_errors`; `stall_timeout_and_max_duration_escalate_with_reason`; `cancel_ends_as_cancelled`; `nonzero_exit_or_adapter_error_is_error`; `first_cycle_omits_missing_reads`; `second_cycle_worker_receives_latest_review`; `report_failure_does_not_change_status`; `unwritable_run_dir_ends_as_error_without_hanging`; `heartbeat_and_activity_are_updated`; report contains status, timeline, partial totals, sandbox kind and exposures, redacted text.
- [ ] **Steps 2–4.** **Step 5: Commit** `feat(loop): engine and report`

---

### Task 7: Watch and single wake

**Files:** Create `src/loops/watch.rs`; Modify `src/orch/store.rs`, `src/daemon.rs` (`reconcile_tick` and `OrchWake`).

**Produces:** table `loop_runs(run_id PK, dir, workflow, status, caller_terminal, caller_created_ms, engine_pid, engine_start_ticks, orch_run_id, notify_state, created_at_ms, updated_at_ms)`; `OrchStore::{register_loop, list_loops, record_loop_final}`; `pub fn watch_loops(store: &OrchStore, tracker: &FleetTracker, procs: &dyn ProcTable, now_ms: i64) -> Result<usize>`.

`record_loop_final` runs in one transaction: creates the `orch` run (title `Loop <run-id>`, caller as coordinator) and the message `msg_loop_<run-id>` (`Status`, `pending`) with body `Loop <run-id>: <STATUS>. Rode: tchelvis loop result <run-id>`; a second call changes nothing. The body never contains agent text. The engine calls it and pokes `OrchWake`; the daemon calls it for dead engines.

- [ ] **Step 1: Failing tests:** `final_state_creates_one_message_even_when_called_twice_or_from_two_connections`; `dead_engine_becomes_error_engine_died` (pid gone; pid reused with other start ticks); `live_engine_with_old_heartbeat_is_not_killed`; `caller_with_other_created_ms_or_absent_is_marked_undelivered`; `orch_check_shows_the_loop_message`; `existing_database_is_migrated`; `unreadable_state_json_does_not_break_the_daemon_cycle`.
- [ ] **Steps 2–4.** **Step 5: Commit** `feat(loop): daemon watch and single wake`

---

### Task 8: CLI, documentation and end-to-end

**Files:** Create `src/loops/cli.rs`, `tests/loop_integration.rs`; Modify `src/main.rs`, `README.md`, `CHANGELOG.md`, `ROADMAP.md`, `src/orch/preamble.rs` (one short section in the skill).

Commands: `init`, `new`, `list [--runs]`, `check`, `run`, `status [--json]`, `result`, `logs`, `report`, `stop`, `clean [--yes]`, hidden `engine <run-id>`. `--detach` starts `tchelvis loop engine <run-id>` with `hc run --bg --no-notify --outlive-parent` through `build_run_worker_args`. `run` fails before creating anything when the workflow, config, sandbox, `max_concurrent` or git check fails.

- [ ] **Step 1: Failing tests** (binary, temporary `HOME`, fake CLIs on `PATH`, fake `hc`, container marker injected by a test-only environment variable compiled under `debug_assertions`): PASS with branch created and checkout untouched; 5-line stdout and exit code for PASS, ERROR, ESCALATE and CANCELLED (SIGINT); `--detach` returns only `RUN_ID` and one message appears; `--detach` without Herdr, missing `auto_approve_in_sandbox`, `prompt` with `--input-file`, `--model` with an unknown stage → exit 2; `max_concurrent` refused; `clean` refuses an active run, shows what it deletes and keeps the branch; `init` twice keeps edited files; a path-like run id (`../x`) is refused everywhere.
- [ ] **Steps 2–4.** **Step 5: Commit** `feat(cli): tchelvis loop`

---

## Delivery

Four sequential batches, coordinator review after each: **A** = Tasks 1–2, **B** = 3–4, **C** = 5–6, **D** = 7–8. Then an independent review of the whole range before the push.

## Real validation (ai-box, then host)

1. `loop check` in `inherited` lists the exposures. A small real loop with the three providers, started by a Claude that ends its turn and is woken once.
2. With `bubblewrap` installed: the same loop, and for each provider a stage asked to write outside the workspace; the file must not exist.
3. Kill the engine, close its terminal, restart the daemon mid-run: one wake, correct reason.
4. Incus from the host: separate task after phase A.
