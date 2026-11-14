# Host-test fixes (agy detection, LLM evaluation, pending prompts, runtime fallback, agent icons)

> **For agentic workers:** execute tasks in order. TDD where a test is listed. Do not commit; the
> coordinator reviews and commits.

**Source:** user's host test report (2026-11-14) + coordinator's root-cause investigation:
- `antigravity`/`agy` is missing from the agent lists → agy sessions are classified as `shell`.
- `llama-server` starts with `total_slots 4` and splits `-c 4096` into ~1024 tokens per slot. The
  supervisor request overflows that (`failed to find … cells in kv cache`).
- The free-text fields of the JSON schema are unbounded. A repro with SmolLM2-360M and the same schema
  gave 298 tokens in 28 s; with `maxLength: 160` on the strings it gave 102–106 tokens in 7–8 s.
- `watcher.rs` drops evaluation errors (`let Ok(raw_eval) = … else { continue; }`), so a blocked
  session silently stays `WORKING`.
- Auto-detection picks `podman` whenever its binary exists, even when rootless Podman is broken
  (no subuid/subgid, `run` exits 125).

**Item 1 (clicking live background sessions does nothing):** this is Task 6. It runs **after** Tasks 1–5,
because it edits the same `main.js`. Host evidence:
- Herdr Desktop's `PATH` contains `~/.local/bin`, and `herdr-core` is there, so the PATH hypothesis is refuted.
- On click, *nothing* happens: no tab opens, not even briefly.

## Global constraints
- Keep the legacy Ollama path working. Every new serde field gets `#[serde(default)]`.
- Deterministic tests only: fakes, temp dirs, no network, no real containers.
- Format only the files you touched (`rustfmt --edition 2021 <files>`); never run a repo-wide `cargo fmt`.
- UI copy: follow the language already used in the surrounding UI.

---

### Task 1: Detect Antigravity (`agy`) as an agent
**Files:** `src/model.rs` (`AgentKind`), `src/correlate.rs` (`classify_agent_kind`, label parsing ~line 33),
`src/herdr.rs` (~line 152 agent-name list), `src/supervisor/hc.rs` (agent-name candidates),
`assets/herdr-app/mods/cockpit/main.js` (`isPlainShellRow` lists ~280 / ~539, program labels ~4201).
- Add `AgentKind::Antigravity` (`as_str() == "antigravity"`).
- Match `antigravity` and `agy` (basename, case-insensitive) in every place that matches `claude`/`codex`/`pi`/`opencode`.
  This covers the foreground name and argv, `term.agent`, `status.app`, and the `cockpit.agent` label.
- **Tests** (`correlate.rs`):
  - `classifies_antigravity_by_status_app`: `status.app = "antigravity"` with a plain foreground → `Antigravity`, and `should_include_terminal` is true.
  - `classifies_agy_by_foreground`: foreground name `agy` → `Antigravity`.
- **Test** (`supervisor/hc.rs`): a session listed with `status.app = "antigravity"` is **not** excluded as a plain shell.

### Task 2: Bounded, observable LLM evaluation
**Files:** `src/supervisor/ollama.rs`, `src/supervisor/container.rs`, `src/supervisor/state.rs`,
`src/supervisor/watcher.rs`, `src/supervisor/tui.rs`.
- **Schema:** in `output_json_schema()`, add `"maxLength"` to `updated_summary` (200), `reason_for_operator` (200), and `hc_command_payload` (120).
- **Request limits:**
  - OpenAI-compatible body: `"max_tokens": 384`.
  - Ollama `/api/chat` body: `options.num_predict = 384`.
- **Container:** `build_run_args` appends `-np 1` unless `cfg.extra_args` already contains `-np`, `--parallel`, or `--parallel=…`.
- **State:**
  - `AgentDurableState` gets `#[serde(default)] last_eval_error: Option<String>` and `#[serde(default)] last_eval_error_at: Option<u64>`.
  - `AgentCardView` gets `eval_error: Option<String>`, filled from `last_eval_error`.
- **Watcher:**
  - When `evaluate` returns `Err(e)`: set `last_eval_error` to `e` (chain via `{e:#}`, truncated to 300 chars) and `last_eval_error_at = now`, emit `tracing::warn!`, and do **not** set `last_evaluated_hash`. The output is evaluated again later.
  - On success: clear both fields.
  - `runtime_status` must not stay `Evaluating` after an error.
- **Tests:**
  - `schema_strings_have_max_length`.
  - `openai_body_has_max_tokens`, and `ollama_body_has_num_predict`.
  - `run_args_default_single_slot` (contains `-np 1`), and `run_args_respect_user_parallel` (`extra_args = ["--parallel","2"]` → no `-np 1`).
  - Watcher `eval_error_is_recorded_and_cleared`: a `MockCognitive` returning `Err` → `last_eval_error` is Some; the next tick with Ok → None.
- **Mod:** in the Inspector "Supervisor & Guardrails" section, when the selected agent has `eval_error`, show a warning line `⚠ LLM evaluation failed: <eval_error>`, using the existing warning color.

### Task 3: Deterministic pending-prompt card (no LLM needed)
**Files:** `src/model.rs` (fleet row), `src/correlate.rs` or wherever fleet rows are built, `src/main.rs`,
`src/supervisor/mod.rs`, `src/supervisor/guardrails.rs` (reuse), `assets/herdr-app/mods/cockpit/main.js`.
- **Fleet row field:** `prompt_destructive: bool` (serde default). It is true when the row is `blocked`
  and `guardrails::detect_destructive(<hc status msg>, "")` or `contains_forbidden_payload(<msg>)` matches.
- **New CLI:** `cockpit supervisor respond <terminal> <approve|deny>`.
  - `approve` runs `hc keys <t> enter`; `deny` runs `hc keys <t> esc`, through the existing `HcBackend` / `hc_bin`.
  - It first reads the terminal's current hc status. If it is not `blocked`, fail with "Nenhum prompt pendente em <t>".
  - On `approve`, if the pending msg (and the last 8 output lines, via `pending_prompt_area`) is destructive, refuse with
    "Comando destrutivo: aprove manualmente no terminal (Open)". This is a server-side check, so it holds even if the UI is bypassed.
  - Then clear the `cockpit.state` / `cockpit.kind` / `cockpit.msg` labels, as auto-reply already does, and print the supervisor snapshot JSON.
- **Tests** (pure helper `fn respond_keys(action) -> &'static str`, plus a guard helper):
  - `approve_maps_to_enter_deny_to_esc`;
  - `approve_refused_for_destructive_prompt` (msg `"Approve Bash: rm -rf build"`) → Err;
  - `respond_requires_blocked`.
- **Mod:** in the Inspector, when the selected row is blocked (`kind` permission/question/auth) **and** there is
  **no** supervisor Action Card for it, render a **Pending prompt** card:
  - kind badge + `msg`;
  - buttons:
    - **Semi-Auto mode:** `Approve` (`["supervisor","respond",addr,"approve"]`), `Deny` (`… "deny"`), `Open` (`focusOrAttachTerminal`).
    - **Alert Only and Paused modes:** only `Open`.
    - When `prompt_destructive` is true: `Approve` is hidden. Show `Deny` + `Open` and a critical badge "destrutivo".
  - After Approve/Deny, refresh the cockpit and supervisor snapshots.

### Task 4: Container runtime fallback
**Files:** `src/supervisor/container.rs`, `src/supervisor/config.rs`, `src/supervisor/mod.rs`.
- **Remembered runtime:** `~/.local/state/cockpit/container_runtime`, which holds the last runtime that started the container successfully.
  - Helpers: `read_runtime_pref(path) -> Option<ContainerRuntime>` and `write_runtime_pref(path, rt)`.
  - Write atomically, like the `supervisor_gpu` toggle.
- **Detection in `auto` mode:** if the remembered runtime is on PATH, it wins. Otherwise use the usual order kiri → podman → docker.
  An explicit `runtime = "docker"` (TOML/env) always wins and never falls back.
- **`load_model` in `auto` mode:** if `start` fails, `rm -f` any half-created container on that runtime and try the next
  available runtime in order. Write the pref for the first runtime whose start + health succeed. If all fail,
  return an error listing each runtime and its error.
- Make the fallback loop a pure-ish function over a list of runtimes and a manager factory, so it can be tested with `FakeCli`.
- **Tests:**
  - `auto_prefers_remembered_runtime`.
  - `explicit_runtime_ignores_pref`.
  - `fallback_tries_next_runtime_on_run_failure`: podman `run` exits 125, then docker succeeds → docker is chosen and the pref written.
  - `fallback_reports_all_errors`.

### Task 5: Agent icon in Fleet cards
**Files:** `assets/herdr-app/mods/cockpit/main.js`.
- In the Fleet card's first row (`● t117 antigravity DONE`), render the agent's icon before the agent name:
  - Prefer Herdr's own program icon via the existing `programIcon(prog, size, color)` helper, where `prog` is the Herdr
    program for the terminal (find how `term.program` / programs are exposed in the mod).
  - Fallback: a small monochrome glyph per agent kind (claude, codex, pi, opencode, antigravity, generic terminal).
- Size it to the row's text (≈12–14 px) and keep the row height unchanged. No `"transparent"` colors.

### Task 6: Fleet click on background sessions (attach) + finished sessions
**Files:** `assets/herdr-app/mods/cockpit/main.js` (`focusOrAttachTerminal` ~1117).

**Hypothesis:** `run("agent.focus", { terminal: addr })` gives focus to a background (`hc run --bg`) terminal
that has no pane. After 200 ms, `focused().terminal === addr`, so the guard `!paneAfter?.pane && mappedFocused !== addr`
is false and `openAttachTab()` never runs.

- **Instrumentation** (keep it; it is cheap):
  - Record the decision path in `setShared("cockpit.attachDebug", { addr, livePane, paneAfter, rawFocusedTerm, mappedFocused, action, at })`,
    where `action` is one of `"focused-pane"`, `"attach-tab"`, `"debounced"`, `"finished-transcript"`.
  - Render it as one muted line at the bottom of the Inspector's controls section (`attach: <action> · focused=<…> · pane=<…>`).
- **Fix:** decide on **visible panes only**. After the focus attempt, if `findPaneForTerminal(panesAfter, termsAfter, addr, termName, null)`
  finds no pane (direct or attach-wrapper), call `openAttachTab()`, whatever `focused()` reports.
  Keep the 1200 ms debounce.
- **Finished terminals** (`term.state === "finished"` or a row status `done`/`error` with no live terminal): do not attach. Instead open
  a transcript tab, `run("tab.new", { space, name: "<name> (log)", argv: ["sh", "-c", "herdr-core read \"$1\" | less -R", "sh", addr] })`.
  Record `action: "finished-transcript"`.
- **Manual check (host):** click a live `hc run --bg` agy session → an attach tab opens. Click a finished one → a transcript tab opens.
  The Inspector debug line shows the path taken.

## Validation
```bash
cargo test --quiet
cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" -A3 | grep -E "correlate.rs|ollama.rs|container.rs|watcher.rs|guardrails.rs|state.rs" || echo "no new warnings"
node --check assets/herdr-app/mods/cockpit/main.js
cargo build --release
```
