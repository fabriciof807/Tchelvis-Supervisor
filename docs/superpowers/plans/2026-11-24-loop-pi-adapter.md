# Loop `pi` Adapter (M11, phase B item 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `provider = "pi"` works in `tchelvis loop` stages, `check` (free and `--live`), `auth` and evidence, as the fourth adapter next to `claude`, `agy` and `opencode`.

**Architecture:** One more `Provider` variant. `adapters.rs` builds the `pi` command and parses its JSON event stream, where errors arrive with exit code 0. `sandbox.rs` recognises the managed `pi` install and mounts only its executable, Node, `auth.json`, `models-store.json` and `models.json` into the `bwrap` tmpfs home. `inherited` instead gets `PI_CODING_AGENT_DIR` pointing at a per-stage directory of symlinks. A new `wrap_for_provider` puts the per-provider environment rules in one place; today they are copied in three files. The free check lists models with `pi --offline --list-models` and checks accounts with `pi auth check`.

**Tech Stack:** Rust 2021, `serde_json`, `libc`, `sha2`. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-24-loop-phase-b-pi-adapter-design.md` (Portuguese; it decides behaviour). Parent: `docs/superpowers/specs/2026-11-23-loop-engine-design.md`.

## Global Constraints

- Stage argv, verbatim and in this order: `pi -p --mode json --no-session --offline --no-approve --no-extensions --no-skills --no-mcp --no-prompt-templates --no-themes --model <model>`. The prompt goes in through stdin and has no size limit. Never `--no-context-files`, `--tools` or `--provider`.
- `pi` always needs a model in the form `<provider>/<id>` with an optional `:<level>`, where `<level>` is one of `off minimal low medium high xhigh max`. A trailing `:<x>` that is not one of those levels belongs to the id.
- Only the managed install is supported: `pi` on `PATH` canonicalises to `<home>/.pi/agent/bin/pi` and `<home>/.pi/agent/install/current-version` is a non-empty regular file. Otherwise fail with, verbatim: `instalação do pi não reconhecida (só a instalação gerenciada em ~/.pi/agent é suportada)`.
- In `bwrap`, mount from `$HOME` only, all read-only: the launcher (existing generic logic), `.pi/agent/install`, `.local/share/pi-node`, `.pi/agent/auth.json`, `.pi/agent/models-store.json` and `.pi/agent/models.json`, each only if it exists. Never `settings.json`, `AGENTS.md`, `extensions/`, `sessions/` or `trust.json`.
- `OPENCODE_API_KEY` reaches only `opencode` stages, probes and free checks, never `pi`.
- `tchelvis` never reads the content of `auth.json`. It uses only its metadata, and it never passes `--credentials`.
- Cost: sum `usage.cost.total` even when `totalFromProvider` is false.
- `loop check` with any `pi` stage prints, verbatim: `aviso: pi: a etapa vê todas as credenciais de ~/.pi/agent/auth.json`.
- `loop auth login pi` without a provider is a usage error, verbatim: `informe o provedor: tchelvis loop auth login pi <provedor>`.
- User-visible text is in Portuguese. Code, docs and the CHANGELOG are in English. Fixtures must contain no personal path or id; this is a public repository.
- Tests use no network and no real `pi`. Executables are fake scripts written with `crate::test_support::write_executable`.
- Run for each task: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --locked`.
- The coordinator owns commits. A worker stops at "ready to commit" unless its briefing says otherwise.

## Review Focus

- A `pi` model that fails with exit code 0 (bad id, 400, aborted, truncated) must fail the stage and the probe. If it gave `PASS`, that would be the failure the spec warns about. Tasks 1 and 4.
- A workspace that contains a planted `.pi/extensions/x.ts`, `.pi/settings.json` or `AGENTS.md` must not get the extension or settings loaded (`--no-approve`); the project `AGENTS.md` is loaded. Covered in Task 1 by `argv_is_exact`, and live in Task 8.
- `auth.json` or `models-store.json` missing, or a symlink, must not break the mount: the mount is skipped and `pi` reports the problem itself. Task 3.
- A model id that contains `:` (`ollama/llama3:8b`) must be accepted and keep its suffix; a missing provider (`glm-5.3`) must be refused before any process runs. Task 2.
- `pi --list-models` output when the user has no provider configured, or `No models matching`, must give `Listed::Unknown` or `No` and never panic. Task 5.

## Files

| File | Change |
|---|---|
| `tests/fixtures/loop/pi_ok.jsonl`, `pi_bad_model.jsonl`, `free_pi_models.txt`, `free_pi_no_match.txt`, `free_pi_auth_ready.json`, `free_pi_auth_not_ready.json` | New, copied from `.work/loop-captures/` |
| `src/loops/workflow.rs` | `Provider::Pi`; `validate_pi_model` |
| `src/loops/models.rs` | `resolve_workflow` rejects `pi` without a valid model |
| `src/loops/adapters.rs` | `command` and `EventParser` for `pi` |
| `src/loops/sandbox.rs` | `pi_managed_install`, `pi` mounts, `wrap_for_provider` |
| `src/loops/engine.rs`, `src/loops/preflight.rs` | Use `wrap_for_provider`; extra argv for the `pi` probe |
| `src/loops/free_check.rs` | `pi` list and account check |
| `src/loops/evidence.rs` | `AccountIdentityInput::PiAuthFile` |
| `src/loops/cli.rs` | Identity, free checks, `auth` line, `auth login pi <provedor>`, `check` warning, messages |
| `README.md`, `docs/TCHELVIS-ORCH.md`, `CHANGELOG.md`, `ROADMAP.md` | Docs |

## Dependencies and delegation

```
T1 ──┬── T2 (workflow.rs, models.rs)
     └── T3 (sandbox.rs) ── T4 (sandbox.rs, engine.rs, preflight.rs, free_check.rs) ── T5 (free_check.rs, cli.rs) ── T6 (cli.rs, evidence.rs) ── T7 (docs) ── T8 (host)
```

- T1 comes first: it adds the enum variant that every `match` needs.
- T2 and T3 touch disjoint files and can run in parallel in separate worktrees.
- T4 shares `sandbox.rs` with T3, T5 shares `free_check.rs` with T4, and T6 shares `cli.rs` with T5, so these run one after another.
- A single implementer in one worktree running T1 to T7 in order is the simplest choice; the only parallelism available is T2 against T3.
- T8 runs on the host and the ai-box. It is not a worktree task.

---

### Task 1: `Provider::Pi`, command and event parser (pure)

**Files:**
- Create: the six fixtures above. `pi_ok.jsonl` and `pi_bad_model.jsonl` are byte copies of `.work/loop-captures/pi_ok.jsonl` and `pi_bad_model.jsonl`. `free_pi_models.txt` and `free_pi_no_match.txt` are copies too. `free_pi_auth_ready.json` is `{"status":"ready","provider":"opencode-go","authType":"api_key"}` and `free_pi_auth_not_ready.json` is `{"status":"not_ready","provider":"anthropic","reason":"credentials_not_configured"}`, both from `.work/loop-captures/free_pi_auth_check.txt`. Before copying, run `grep -E 'ubuntu|claude-1000|/home/[a-z]' ` on each file and expect only `/home/user`.
- Modify: `src/loops/workflow.rs:79-112` (variant `Pi`, `as_str` `"pi"`, `FromStr` `"pi"`; the `serde(rename_all = "lowercase")` attribute already gives `"pi"`).
- Modify: `src/loops/adapters.rs:22-103` (`command`), `:115-166` (parser state and dispatch), `:303-335` (`finish`).
- Modify, for compilation only: `src/loops/free_check.rs:115-155` (`parse_model_list`: `Provider::Pi => Vec::new()`), `:276-280` (argv `["pi","--offline","--list-models"]`), `:326-400` (`Provider::Pi => fallback(Ok(()), None)`), and `src/loops/cli.rs:928-990` (`Provider::Pi => return None`). `cli.rs:1024-1105` gets `Provider::Pi => FreeCheck { account_ok: Ok(()), listed: Listed::Unknown }`. These are safe defaults: with no evidence the target is always probed. Tasks 5 and 6 replace them.
- Test: `src/loops/adapters.rs` (tests module, `:384-`), `src/loops/workflow.rs`.

**Interfaces:**
- Produces: `Provider::Pi`. `command(Provider::Pi, Some(m), prompt)` returns `AdapterCommand { argv: <Global Constraints argv>, stdin: prompt bytes }`. With `model == None` it returns `Err("'pi' exige modelo no formato provedor/id")`, as a guard; Task 2 makes that case unreachable. `EventParser::new(Provider::Pi)`.

- [ ] **Step 1: Write the failing tests** in `adapters.rs`:
  - `argv_is_exact`: add `command(Provider::Pi, Some("opencode-go/glm-5.3:high"), "hello prompt")`, whose argv equals the Global Constraints list ending `--model opencode-go/glm-5.3:high` and whose `stdin == b"hello prompt"`. `command(Provider::Pi, None, "x")` is `Err`.
  - `ok_fixtures`: `parse_fixture(Provider::Pi, "pi_ok.jsonl")` gives `turns == Some(1)`, `input_tokens == Some(1493)`, `output_tokens == Some(39)`, `cache_read_tokens == Some(0)`, `cost_usd == Some(0.0)` and `error == None`.
  - `bad_model_fixtures_report_an_error`: for `pi_bad_model.jsonl`, the error contains `isn't included in OpenCode Go`, the error is a single line, and `turns == None`.
  - `pi_error_rules`, using synthetic lines built with `serde_json::json!`:
    - Last assistant `message_end` with `stopReason:"aborted"` and `errorMessage:"x"` gives an error containing `x`.
    - `stopReason:"length"` gives an error containing `truncada`.
    - `{"type":"agent_settled","aborted":true}` gives an error containing `abortou`.
    - A stream with no `agent_settled` gives the error `pi terminou sem evento 'agent_settled'`.
    - An earlier `error` turn followed by a final `stop` turn, with no `auto_retry_end`, gives no error and `turns == Some(1)`.
  - `pi_usage_null_rules`: two assistant `message_end` where the second has no `usage` give `input_tokens == None` while turns still count. `cost.total` present with `totalFromProvider:false` is summed.
  - `garbage_missing_or_renamed_fields_become_null_without_failing` (`:573-`): add `Provider::Pi` cases with the same expectations as `opencode`, except the missing-`agent_settled` rule.
  - `error_text_is_redacted_and_single_line` (`:606-`): add a `pi` `auto_retry_end.finalError` that contains `sk-...` and a newline.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::adapters`. Expect a compile failure (no `Provider::Pi`).
- [ ] **Step 3: Implement.** Add the variant and the compile-only arms listed above.
  - `command`: return `Err` when the model is `None`. Build the argv as a `[&str]` array plus `--model m`, the same way as `Provider::Claude`.
  - Parser state: `pi_turns: u64`, sums and seen-counters for `input`, `output`, `cacheRead` and `cost.total` (the `opencode` pattern), `pi_assistant_msgs: u64`, `pi_last_stop: Option<String>`, `pi_last_error: Option<String>`, `pi_retry_failed: Option<String>`, `pi_settled: bool` and `pi_aborted: bool`.
  - Events:
    - `message_end` with `message.role == "assistant"` updates the sums, `pi_last_stop` and `pi_last_error` (from `errorMessage`).
    - `turn_end` with an assistant message whose `stopReason` is not `error` or `aborted` increments `pi_turns`.
    - `auto_retry_end` with `success:false` sets `pi_retry_failed = finalError`.
    - `agent_settled` sets `pi_settled` and `pi_aborted`.
  - `finish` checks, first match wins: retry failed; then last stop `error` or `aborted` (`errorMessage`, or `pi retornou stopReason <x>`); then `agent_settled.aborted` (`pi abortou a execução`); then `length` (`resposta truncada no limite de saída`); then not settled (`pi terminou sem evento 'agent_settled'`). Each message goes through `sanitize_error_text`. Usage fields are `Some` only when the seen count equals `pi_assistant_msgs > 0`, and turns is `Some` only when it is greater than 0.
- [ ] **Step 4: Run** `cargo test --locked --lib loops::adapters` and expect PASS. Then run the three per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): pi provider, command and event parser`.

### Task 2: `pi` model validation

**Files:**
- Modify: `src/loops/workflow.rs:196-208` (add the new function next to `validate_model_name`).
- Modify: `src/loops/models.rs:307-422` (`resolve_workflow`).
- Test: both modules.

**Interfaces:**
- Consumes: `Provider::Pi` (Task 1).
- Produces: `pub(crate) const PI_THINKING_LEVELS: [&str; 7] = ["off","minimal","low","medium","high","xhigh","max"]`; `pub(crate) fn validate_pi_model(model: Option<&str>) -> Result<(), String>`; `pub(crate) fn pi_model_base(model: &str) -> &str`, which strips a trailing `:<level>` only when the level is one of the seven.

- [ ] **Step 1: Write the failing tests:**
  - `validate_pi_model_rules`. `Ok` for `opencode-go/glm-5.3`, `opencode-go/glm-5.3:high`, `ollama/llama3:8b` and `openrouter/anthropic/claude-x`. `Err` for `None`, `glm-5.3`, `/x`, `x/`, `opencode-go/a b` and `opencode-go/:high`.
  - `pi_model_base_strips_only_known_levels`. `"a/b:high"` gives `"a/b"`, `"a/b:8b"` gives `"a/b:8b"`, and `"a/b"` gives `"a/b"`.
  - In `models.rs`, `resolve_rejects_pi_without_provider_model`. A workflow stage `provider = "pi"` with no model, and separately `--use implement=pi:glm-5.3`, both give `Err` containing `etapa 'implement': 'pi' exige modelo no formato provedor/id`. `--use implement=pi:opencode-go/glm-5.3:high` gives `Ok` with model `opencode-go/glm-5.3:high`.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::workflow loops::models` and expect FAIL.
- [ ] **Step 3: Implement.** In `resolve_workflow`, after `chosen` is known and before `resolutions.push`, when `choice.provider == Provider::Pi` and `validate_pi_model` fails, push `format!("etapa '{}': 'pi' exige modelo no formato provedor/id", st.id)` and `continue`. All five sources pass through this point.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): require provider/id models for pi`.

### Task 3: Managed install and `bwrap` mounts

**Files:**
- Modify: `src/loops/sandbox.rs:31-36` (`ADAPTER_CREDENTIALS` gets `("pi", None)`) and `:498-546` (`adapter_binds`).
- Test: `src/loops/sandbox.rs` (follow the fake-home tests near `:1239`).

**Interfaces:**
- Produces: `pub struct PiInstall { pub agent_dir: PathBuf, pub version: String }`; `pub const PI_UNRECOGNIZED: &str = "instalação do pi não reconhecida (só a instalação gerenciada em ~/.pi/agent é suportada)"`; `pub fn pi_managed_install(home: &Path, path_var: Option<&OsStr>) -> Result<PiInstall, String>`. The `Err` is `PI_UNRECOGNIZED`. The version is the trimmed content of `current-version` and must match `[0-9A-Za-z._+-]+`.

- [ ] **Step 1: Write the failing tests.** Build a fake home: `.local/bin/pi` is a symlink to `../../.pi/agent/bin/pi`, which is a script; `.pi/agent/install/current-version` holds `1.5.2`; `install/releases/1.5.2/` exists; `.local/share/pi-node/current/bin/node` exists; `.pi/agent/{auth.json,models-store.json,settings.json,AGENTS.md}` exist; `.pi/agent/{sessions,extensions}/` exist. Set `PATH` to `<home>/.local/bin:/usr/bin`.
  - `pi_managed_install_recognises_layout_and_refuses_others`. The fake home gives `Ok` with version `1.5.2`. Without `current-version`, with a `pi` that is a plain file in `.local/bin`, or with a version `../x`, the result is `Err(PI_UNRECOGNIZED)`.
  - `bwrap_mounts_for_pi_are_exactly_the_managed_files`. For `BwrapSandbox::wrap` of `["pi","-p"]`, the `--ro-bind` destinations include `.pi/agent/install`, `.local/share/pi-node`, `.pi/agent/auth.json` and `.pi/agent/models-store.json`. They do not include any path ending in `settings.json`, `AGENTS.md`, `sessions` or `extensions`, nor `.pi/agent` itself. With `models.json` present it is included. With `auth.json` as a symlink it is not mounted.
  - `bwrap_wrap_of_unrecognized_pi_fails`. When `pi` is not managed, `wrap` returns `Err` containing `instalação do pi não reconhecida`.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::sandbox` and expect FAIL.
- [ ] **Step 3: Implement.** Add `pi_managed_install` (canonicalise with `fs::canonicalize`; read `current-version` with the existing nofollow helpers). In `adapter_binds`, for `pi`: call `pi_managed_install`, and on `Err` propagate (change the return to `Result<Vec<_>>`; the `?` goes in `wrap_clean`). Add each of the five home paths that is a regular file or directory, not a symlink (`symlink_metadata`) and not something `reaches_tchelvis` would hit, as `(p, p)`. The generic launcher logic stays as it is.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): mount only the managed pi install in bwrap`.

### Task 4: `wrap_for_provider`, the `inherited` agent dir, probe argv, and fake-`pi` engine and preflight tests

**Files:**
- Modify: `src/loops/sandbox.rs` (new function).
- Modify: `src/loops/engine.rs:121-145`, `src/loops/preflight.rs:200-219` and `src/loops/free_check.rs:282-297`. All three replace their `match (provider, key)` and `retain` with the new call.
- Modify: `src/loops/preflight.rs:202-207`, adding the `pi` probe extras.
- Test: `src/loops/sandbox.rs`, `src/loops/preflight.rs` (next to `:847`), `src/loops/engine.rs` (next to `:2477`).

**Interfaces:**
- Consumes: `pi_managed_install` (Task 3) and `Provider::Pi` (Task 1).
- Produces: `pub fn wrap_for_provider(sandbox: &dyn Sandbox, provider: Provider, argv: &[OsString], workspace: &Path, scratch: &Path, opencode_api_key: Option<&str>) -> Result<SandboxedCommand>`.
  - `Opencode` with a key calls `wrap_with_extra_env(.., &[("OPENCODE_API_KEY", key)])`.
  - Every other provider calls `wrap` and removes `OPENCODE_API_KEY` from `env`.
  - `Pi` under `sandbox.kind() == "inherited"` also creates `<scratch>/pi-agent/` (mode `0o700`). In it, it creates symlinks named `auth.json`, `models-store.json` and `models.json`, each pointing to `<HOME from cmd.env>/.pi/agent/<name>` when that file exists. Then it sets `PI_CODING_AGENT_DIR=<scratch>/pi-agent` in `env`, never in argv.
  - `scratch` is `req.log_path.parent()` in the engine, the probe log dir in preflight, and the free-check log dir in `free_check`.

- [ ] **Step 1: Write the failing tests:**
  - `wrap_for_provider_env_rules`, using `InheritedSandbox` with a fake home:
    - `Pi` gives `PI_CODING_AGENT_DIR` in `env`, no `OPENCODE_API_KEY` even when a key is given, and argv equal to the input. The agent dir holds exactly the symlinks for the files that exist.
    - `Opencode` with a key gives the key in `env`.
    - `Claude` gives no key.
  - Under `BwrapSandbox`, `Pi` gives no `PI_CODING_AGENT_DIR`.
  - `pi_probe_adds_no_tools_and_short_system_prompt` in preflight: a fake `pi` script on `PATH` logs its argv to a file and prints `pi_ok.jsonl`. The probe returns `Ok`, and the logged argv ends with `--model <m> --no-tools --system-prompt Reply briefly.`.
  - `pi_bad_model_with_exit_zero_fails_probe`: the fake `pi` prints `pi_bad_model.jsonl` and exits 0. The result is `Err` with a reason containing `isn't included`.
  - In the engine, `pi_bad_model_with_exit_zero_fails_stage`: the same fake makes `StageResult.outcome == StageOutcome::Error`, and the error contains `isn't included`.
  - `opencode_api_key_reaches_only_opencode_and_is_redacted_from_failure_reasons` (`preflight.rs:847`) and the engine test at `:2477` stay green unchanged.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::sandbox loops::preflight loops::engine` and expect FAIL.
- [ ] **Step 3: Implement** `wrap_for_provider`, and replace the three copies with calls to it. In `probe_one`, for `Provider::Pi`, push `--no-tools`, `--system-prompt`, `Reply briefly.`.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): one place for per-provider stage environment; pi agent dir in inherited`.

### Task 5: Free check for `pi` (list and account)

**Files:**
- Modify: `src/loops/free_check.rs:60-66` (`ProviderFreeOutcome`), `:115-179` (list and match), `:240-400` (`run_one_free_check`) and `:401-` (`run_free_checks_with_options`).
- Modify: `src/loops/cli.rs:1024-1105` (`Provider::Pi` arm of `build_free_checks_for_targets`), plus the three callers at `:1930`, `:2316` and `:2711`.
- Test: `src/loops/free_check.rs` (fixtures as `include_str!`, next to `:467`; fake-binary tests next to `:844`).

**Interfaces:**
- Consumes: `pi_model_base` (Task 2) and `wrap_for_provider` (Task 4).
- Produces:
  - `pub enum PiAccount { Ready, NotReady(String), Unknown }`.
  - `ProviderFreeOutcome.pi_accounts: BTreeMap<String, PiAccount>`, empty for the other providers.
  - `pub fn parse_pi_auth_check(raw: &str) -> PiAccount`. `status` `ready` gives `Ready`; `not_ready` or `invalid` gives `NotReady(reason)`; anything else gives `Unknown`.
  - `run_free_checks_with_options` gains the parameter `pi_providers: &[String]`, the distinct `<provider>` prefixes of `pi` targets, which callers compute with `split_once('/')`.

- [ ] **Step 1: Write the failing tests:**
  - `parse_model_list(Provider::Pi, FIX_PI_MODELS)` has 34 items, starts with `opencode-go/beam-free` and contains `opencode-go/hy4-flash-free`.
  - `FIX_PI_NO_MATCH`, `""` and `"Error: x\n{bad}"` each give an empty list.
  - A header that is not `provider` followed by `model` gives an empty list.
  - `is_model_listed(Provider::Pi, Some("opencode-go/hy4-flash-free:high"), &list)` is `Yes`, `Some("opencode-go/nope")` is `No`, and an empty list is `Unknown`.
  - `parse_pi_auth_check(FIX_PI_AUTH_READY)` is `Ready`; `FIX_PI_AUTH_NOT_READY` gives `NotReady("credentials_not_configured")`; garbage gives `Unknown`.
  - `pi_free_check_runs_list_and_one_auth_check_per_provider`: a fake `pi` script answers `--list-models` with the fixture and `auth check --provider opencode-go` with the ready JSON. It writes each argv line to a log. With `pi_providers = ["opencode-go"]`, the log has exactly two invocations, argv `pi --offline --list-models` and `pi auth check --provider opencode-go --json --no-refresh`. No argv contains `--credentials`. The outcome has `list_known` and `pi_accounts["opencode-go"] == Ready`.
  - In `cli.rs`, `pi_free_check_maps_account_and_listing`. `NotReady` gives `account_ok == Err` containing `rode 'tchelvis loop auth login pi anthropic'`. `Ready` plus a listed model gives `Ok` and `Listed::Yes`. A missing account entry gives `Ok` and `Listed::Unknown`.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::free_check loops::cli` and expect FAIL.
- [ ] **Step 3: Implement.**
  - The `Pi` list parser: the first non-empty line, split on whitespace, must start with `provider` and `model`. Every following line must have at least two tokens, which become `format!("{p}/{m}")`. Any bad line gives an empty list.
  - Matching compares `pi_model_base(m)` exactly.
  - `run_one_free_check` for `Pi` runs the list command, then one auth check per provider. It uses the same limits, captures each auth check separately, and treats the JSON as data whatever the exit code (0, 1 or 2).
  - Account message: `provedor '<p>' do pi sem credencial (<reason>); rode 'tchelvis loop auth login pi <p>'`.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): free model list and account check for pi`.

### Task 6: Evidence identity, `loop auth`, `auth login pi <provedor>`, `check` warnings and messages

**Files:**
- Modify: `src/loops/evidence.rs:30-46` (variant) and `:582-` (`compute_account_identity` arm with label `b"pi-auth\0"` via `file_identity`).
- Modify: `src/loops/cli.rs:928-990` (identity), `:216-222` (`AuthSubcommand::Login { cli, provider: Option<String> }`), `:1929-2010` (status lines), `:2013-2056` (login), `:2238-` (`cmd_check` warning and install check), and `:2015` together with `:217-219` (the help and error text gets `pi`).
- Test: `src/loops/evidence.rs` and `src/loops/cli.rs`.

**Interfaces:**
- Consumes: `PiInstall` (Task 3) and `ProviderFreeOutcome` with `pi_accounts` (Task 5).
- Produces:
  - `AccountIdentityInput::PiAuthFile(&fs::Metadata)`.
  - The executable identity for `pi` is `compute_executable_identity("pi", path_var)` followed by `:` and `PiInstall.version`.
  - `pub fn pi_auth_line(outcome: Option<&ProviderFreeOutcome>, install: &Result<PiInstall, String>) -> String`, which is pure.

- [ ] **Step 1: Write the failing tests:**
  - `pi_identity_changes_with_auth_file_and_version`. Rewriting `auth.json` (new mtime) changes `account`. Changing `current-version` changes `executable`. With `auth.json` absent, the identity is `None`.
  - `pi_auth_line_texts`:
    - Listed models from `opencode-go` give `pi:       34 modelo(s) de opencode-go (pi --list-models)`.
    - An empty list gives `pi:       nenhum provedor com credencial (rode 'tchelvis loop auth login pi <provedor>')`.
    - An unrecognised install gives `pi:       ` followed by `PI_UNRECOGNIZED`.
  - `auth_login_pi_requires_provider`. `cli = "pi"` with `provider = None` gives a usage error equal to the Global Constraints text. `provider = Some("Bad Name")` gives a usage error. `Some("opencode-go")` builds argv `["pi","auth","login","opencode-go"]`. Test the pure argv builder `fn login_argv(cli: &str, provider: Option<&str>) -> Result<Vec<String>, LoopError>`, extracted from `:2025-2056`.
  - `check_warns_pi_credentials_and_fails_unrecognized_install`, a pure helper `fn pi_check_notes(workflow: &Workflow, install: &Result<PiInstall, String>) -> Result<Vec<String>, String>`. A workflow with a `pi` stage gives `Ok(vec!["aviso: pi: a etapa vê todas as credenciais de ~/.pi/agent/auth.json"])`. An unrecognised install gives `Err(PI_UNRECOGNIZED)`. A workflow without `pi` gives `Ok(vec![])`.
- [ ] **Step 2: Run** `cargo test --locked --lib loops::evidence loops::cli` and expect FAIL.
- [ ] **Step 3: Implement**, and wire the code in:
  - `cmd_auth` adds `Provider::Pi` to the providers list at `:1929` and prints `pi_auth_line`.
  - `cmd_check` and `cmd_run` call `pi_check_notes` after the workflow resolves. `Err` ends the command with `LoopError::err`, and the notes go to stderr through `eprint_redacted`.
  - The login error for an unknown CLI becomes `CLI desconhecido '{cli}' (use claude, agy, opencode ou pi)`.
- [ ] **Step 4: Run** the tests and expect PASS. Then run the per-task commands.
- [ ] **Step 5: Ready to commit** with the message `feat(loop): pi in loop auth, check and evidence`.

### Task 7: Docs

**Files:** `CHANGELOG.md` (`[Unreleased]` / `Added`), `ROADMAP.md:44` and `:174-184` (M11), `docs/TCHELVIS-ORCH.md:213`, and `README.md` (only if a loop-provider list exists; today `README.md` lists none for loops, so check with `grep -n "loop" README.md`). `docs/agent-install.md` does not list loop providers, but confirm with `grep -n "agy" docs/agent-install.md`.

- [ ] **Step 1:** CHANGELOG gets one bullet. Content: a `pi` loop adapter (managed install only), the argv isolation flags, the minimal agent dir (`bwrap` mounts and `inherited` `PI_CODING_AGENT_DIR`), errors detected from the JSON stream even on exit code 0, `provider/id[:thinking]` required, the free checks `pi --offline --list-models` and `pi auth check`, `loop auth login pi <provider>`, and the warning that a `pi` stage sees every credential in `~/.pi/agent/auth.json`.
- [ ] **Step 2:** ROADMAP M11 row and section: "Phase B: `pi` adapter implemented (host validation pending)" until Task 8 passes. Then "validated on the host (date)". Add an operational note about the `auth.json` visibility.
- [ ] **Step 3:** `docs/TCHELVIS-ORCH.md:213` becomes `` `tchelvis loop` supports `claude`, `agy`, `opencode`, and `pi` stages only. ``
- [ ] **Step 4:** Run the per-task commands, plus `grep -rn "claude, agy ou opencode\|claude\`, \`agy\`, and \`opencode\` stages" src docs README.md`. Expect no hits outside the specs and plans.
- [ ] **Step 5: Ready to commit** with the message `docs: pi loop adapter`.

### Task 8: Host validation (mandatory before calling it done)

Run with the release binary (`cargo build --release`; on the host use `tchelvis update`) and a throwaway repo. Use the free model `opencode-go/hy4-flash-free`. OAuth providers are out of scope.

- [ ] **Step 1: ai-box, `bwrap`, `install/` read-only.** `tchelvis loop check <wf> --use implement=pi:opencode-go/hy4-flash-free --live` passes and prints the `auth.json` warning. Then `tchelvis loop run` a one-stage workflow that writes `.tchelvis-loop/out/<file>`, and expect `PASS` with tokens in `report.md`.
- [ ] **Step 2: Write outside the workspace.** The prompt asks the agent to create `$HOME/pi-escape.txt` and `/tmp/../$HOME/.pi/agent/pwned`. After the run, neither exists on the real filesystem, and `~/.pi/agent` has an unchanged `ls -la --time-style=full-iso` listing, `install/` included.
- [ ] **Step 3: Planted project files.** The workspace has `.pi/extensions/evil.ts`, which writes a marker, and `AGENTS.md`, which asks to start the reply with `AGENTS-OK`. The marker must be absent; the `AGENTS.md` instruction is followed (seen in the stage log).
- [ ] **Step 4: Bad model.** `--use implement=pi:opencode-go/nope-model` fails in `check` (free mode: not listed; `--live`: reason contains `isn't included`).
- [ ] **Step 5: `inherited`** in a container: Step 1 passes, and `~/.pi/agent/sessions` gains no file.
- [ ] **Step 6: Host with `incus`,** once that sandbox kind exists. Until then, record it as pending in ROADMAP.
- [ ] **Step 7: Plan B, only if Step 1 fails because `install/` is read-only.** In `adapter_binds`, replace the `.pi/agent/install` mount with two read-only mounts, `install/current-version` and `install/releases/<version>`, leaving the rest of `install/` on the tmpfs home. Update Task 3's mount test to match and re-run Steps 1 to 3.
- [ ] **Step 8:** Record the results, with dates, in the spec's "Riscos" item 1 and in ROADMAP (Task 7 Step 2).
