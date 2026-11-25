# Changelog

All notable changes to Tchelvis Supervisor (binary `tchelvis`, previously `cockpit`) are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Loop stop diagnostics.** A stage the engine cuts (`STALLED`, `STAGE_TIMEOUT`, or `MAX_DURATION`) now records what it was doing in a new `stop` object in `state.json`: last stream event, time since the last output, whether the CLI was still alive, the tools it had open, and the surviving processes named only by kernel `comm` with age, CPU, and whether each started after the last output. The run reason ends with a hint (`ferramenta em execução: bash`, `possível ferramenta em execução: … (processos iniciados depois da última saída)`) and `report.md` gains a `## Etapas interrompidas` section with the same facts. A cut stage no longer reports the misleading `pi terminou sem evento 'agent_settled'`; names are redacted and capped, and no command line, argument, path, or environment is ever stored.
- **Loop sandbox `none` (unsandboxed mode), gated by `allow_unsandboxed`.** `tchelvis loop` can run with no sandbox at all when `~/.config/tchelvis/loops.toml` sets both `kind = "none"` and `allow_unsandboxed = true`; line 1 of `loop result`/`loop status` and `report.md` carry `SEM SANDBOX`, `loop check` names the mode, `loop auth` uses the normal `HOME` logins (`agy` gets the session bus instead of a separate loop login), and the ticket coordinator asks before merging such a run.
- **`to-tickets` skill (`~/.agents/skills/to-tickets/SKILL.md`).** Splits an approved spec into vertical-slice tickets with explicit `blocked_by`, `files`, failing-first acceptance criteria, and a final `integration` ticket (ideas adapted from Matt Pocock's MIT `to-tickets` skill). Installed by `tchelvis setup` under `skills` as one copy in `~/.agents/skills` (linked at `~/.claude/skills/to-tickets` when `~/.claude` exists); asks before replacing a user-owned file, never writes through symlinks, and `tchelvis uninstall` preserves user-modified copies.
- **Loop-backed ticket coordinator in `/orchestration-tchelvis`.** Adds `## Running Tickets with Loops` so a coordinator can run tickets in an isolated `integration/<feature>` worktree via `tchelvis loop run <workflow> --input-file <path> --detach`, merge `PASS` branches whose changed files are inside the ticket's `files` with `git -c core.hooksPath=/dev/null merge --no-ff`, stop on failures or out-of-scope edits, recover state from `loop list --runs` / `loop list --active`, and finish with an independent `tchelvis orch launch` review.
- **Declarative multi-agent loops (`tchelvis loop`, M11 phase A).**
  - Fixed multi-agent TOML workflows (`worker-reviewer-critic`, `designer-worker-reviewer`, `planner-worker-tester-critic`, `fast-worker-reviewer`, plus `tchelvis loop new <name> --stages ...`) running clean headless `claude`, `agy`, and `opencode` (`--standalone`) sessions, exchanging artifacts only through symlink-safe `.tchelvis-loop/{in,out}` directories, snapshotting git via isolated `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`, generating `report.md`, and writing a single wake message to `orch.db` when finished or when the engine dies.
  - CLI subcommands: `init`, `new`, `list [--runs]`, `check <workflow> [--live] [--use ...]`, `models [<workflow>|set|confirm]`, `auth [login <cli>]`, `run`, `status [--json]`, `result`, `logs [--stage]`, `report`, `stop`, and `clean [--yes]` (exit codes: `0` PASS, `1` ERROR, `2` usage, `3` ESCALATE, `4` CANCELLED; 5-line `stdout` on `result` and non-verbose `run`).
  - **Safety & operational notes:**
    - **The loop never merges or pushes:** it creates an isolated clone and commits final work only to a local `loop/<workflow>-<run-id>` branch, leaving the user's checkout and `.gitignore` untouched.
    - **`auto_approve_in_sandbox` must be turned on by the user:** `~/.config/tchelvis/loops.toml` defaults to `false`, is never read from the repository, and refuses to run workflows until explicitly set to `true`.
    - **`bwrap` shares the network and localhost:** `bubblewrap` is the default sandbox (no silent fallback) and isolates the filesystem, `$HOME`, PID, IPC, and UTS namespaces, while network access, `localhost` services, and abstract sockets remain shared (`inherited` requires a container marker; `incus` is planned).
    - **Three-level preflight without spending tokens every time:** `run` verifies each distinct `provider:model` pair across all stages via free sandboxed CLI checks (`claude auth status`, `agy models`, `opencode models --standalone`), recent salted-identity evidence (`~/.local/state/tchelvis/loops/model_evidence.json`, `0600`, updated on stage success and invalidated per failing pair), and lean `Reply with exactly: OK` probes only when needed. Global `[preflight]` configures `mode = "auto" | "always" | "free"` and `valid_hours = 1..=168`; `check` previews without tokens, `check --live` probes all and renews evidence, and `report.md` renders verification methods separately from loop totals.
    - **Pi stages (M11 phase B, host validation pending):** a workflow stage may now use `pi`, restricted to the managed install at `~/.pi/agent/bin/pi` (any other `pi` on `PATH` is refused with a clear message). Stages run print mode with a JSON event stream (`--no-session --offline --no-approve --no-extensions --no-skills --no-mcp --no-prompt-templates --no-themes`), take the prompt from `stdin`, keep the model in `provider/id` form with an optional `:thinking-level` suffix, and read turns, tokens, cache hits, cost, and errors from the stream itself.
    - **Pi credentials stay inside the agent directory:** `bwrap` mounts only the launcher, install, Node runtime, `auth.json`, `models-store.json`, and `models.json` from `~/.pi/agent` (never `settings.json`, `AGENTS.md`, extensions, or sessions), and `inherited` points `PI_CODING_AGENT_DIR` at a scratch directory with symlinks to those three files. `OPENCODE_API_KEY` is never forwarded to `pi`, and `check` warns that a pi stage can read every credential in `~/.pi/agent/auth.json`.
    - **Pi preflight, auth, and identity:** the free check is `pi --offline --list-models` plus one `pi auth check --provider <p> --json --no-refresh` per provider in use, each provider becomes its own account identity (salted hash of `auth.json` metadata and the installed version), `tchelvis loop auth` lists `pi`, and `tchelvis loop auth login pi <provider>` runs `pi auth login <provider>`.
    - **An exported `OPENCODE_API_KEY` is visible to every program started from that shell:** `tchelvis` never stores or reads keys from the OpenCode database and forwards `OPENCODE_API_KEY` solely to `opencode` stages and probes via environment while redacting it from state, reports, and errors—though exporting it in your shell exposes it to any program launched from that shell.
- **Configurable supervisor `[guardrails]` (M10).**
  - Optional `[guardrails]` table in `~/.config/tchelvis/supervisor.toml` (`allow_commands`, `deny_commands`, `deny_paths`, `allow_tools`, `builtin_read_only`, `builtin_build_test`, `llm_auto_reply`), reloaded on every watcher tick and parsed independently from other settings.
  - Fixed decision order (`floor` → `deny_*` → built-ins / `allow_*` → LLM) with an immutable safety floor (also refusing dot-segment globs such as `cat .e*`).
  - Invalid `[guardrails]` tables (unknown keys, wrong types, invalid `deny_*` entries, unreadable files, broken TOML) stop all automatic replies; invalid `allow_*` entries are ignored with warnings.
  - **CLI & Doctor:** `tchelvis supervisor rules [--json]` and dry-run `tchelvis supervisor rules test` (`<command> [--tool <name>]` or `--screen <file> [--msg <text>] [--kind <kind>]`, exit codes `0`/`1`/`2`), plus a `guardrails` check in `tchelvis doctor`.
  - **Observability:** deciding `rule_id` is recorded in `supervisor_events.jsonl` and displayed in the Herdr Desktop mod's **Log** view (`Regra: <rule_id>`).
- **Multi-line daily tasks & external editor.**
  - **CLI:** `tchelvis task add --edit` opens `$VISUAL` / `$EDITOR` (falling back to `nvi` then `vi`) pre-filled with any positional draft; empty saved text cancels cleanly. Human list output (`task list`, `home`) displays the first line plus `…(+N linhas)`, while `--json` keeps full titles.
  - **Herdr mod:** the task input wraps across lines (`Shift+Enter` inserts a newline, `Enter` saves), a `✎` button opens `tchelvis task add --edit` in a new terminal tab and clears the draft once the task appears, and task rows render full multi-line titles and full project tags without truncation.
- **Daily task tag menu.** Clicking the `📁 <tag>` pill opens a wrap-able menu listing `Geral` first followed by all Home projects instead of cycling one by one.
- **Supervisor fallback model (M1b, phase 1 API).**
  - When the managed `llama.cpp` container fails to start or fails `/health` on 2 consecutive ticks with active sessions, evaluation switches automatically to a configured OpenAI-compatible API model until the user chooses to return (`Voltar ao principal` / `▶ Load LLM` / `tchelvis supervisor fallback off`), or stops on `Unload LLM` / zero-idle.
  - **CLI:** `tchelvis supervisor fallback {status,set,key,models,off,clear}`. Keys saved via stdin go to `~/.config/tchelvis/fallback_api_key` (`0600`) and are never written to `supervisor.toml`, snapshots, audit entries, or logs; shared runtime state lives in `~/.local/state/tchelvis/supervisor_fallback.json`.
  - Intentional restarts (Load, model/GPU switch, CPU/GPU bench) never trigger it: probes are ignored during a 300 s start grace, and failures seen by several processes count once per poll interval. A fallback host typed without a scheme defaults to `https://`.
  - **Herdr mod:** active fallback shows `● LLM: <model> (Fallback)` in warning tone; the **Supervisor model** card splits into `PRINCIPAL (local)` (with `falhou` badge while active) and `FALLBACK (API)` with endpoint entry, `/v1/models` picker (`Trocar`), and `Voltar ao principal`.

### Changed
- **Compact Fleet cards.** Two lines at about half the height: state, agent icon, project and status; then branch, summary and the PR or issue. The terminal address moved to the Inspector title and shows on a card only when two would look the same. Permission requests and blocked or active children keep their own line.
- **Host-matched API keys for `openai-compatible`.** Provider environment variables are now matched against the target host (`openai.com` → `OPENAI_API_KEY`, `openrouter.ai` → `OPENROUTER_API_KEY`, `deepseek.com` → `DEEPSEEK_API_KEY`, `opencode.ai` → `OPENCODE_API_KEY`); an `OPENAI_API_KEY` is no longer sent to non-OpenAI hosts. Explicit `TCHELVIS_LLM_API_KEY` (and `TCHELVIS_FALLBACK_API_KEY` / key file for fallback) still take precedence.
- **Loop discovery and stopping ergonomics (`tchelvis loop`).** Foreground `loop run` prints `RUN_ID: <id>` to `stderr` upon registration (keeping 5-line `stdout`), `loop list --active` filters to live active runs (`Nenhum loop em execução.` when empty), and `loop stop [run_id]` accepts an optional ID that auto-selects when the current git repository has a single active loop (never crossing repositories).

### Fixed
- **Git LFS files in loop workspaces.** With the LFS filter configured only in the repository's .git/config, agents saw pointer files. The checkout now applies the validated LFS filter and reads objects only from the local LFS store; a run is refused before cloning, with a hint to run git lfs fetch, when an object is missing (or when a pointer is not canonical), and the repository's .lfsconfig no longer affects the checkout.
- **The agent's .lfsconfig no longer steers the engine.** Snapshot and restore ran git-lfs outside the sandbox with the workspace's .lfsconfig, so an agent could make the engine contact an address of its choice or leave pointers; the engine now overrides those settings.
- **Daemon no longer republishes `cockpit.snapshot` in a loop.** The `doc` event produced by its own `doc.set` counted as a change and triggered a new refresh and publish (about 13 writes per second, about 1 MB/s of disk writes in `herdr-core`). Events of that doc are now ignored, and a snapshot whose content is unchanged (apart from `published_at`) is written at most every 30 s.
- **Separate `agy` login for loops.** A desktop `agy` moves a renewed login to the system keyring and deletes its token file, so loops lost the account after a while. `tchelvis loop auth login agy` now keeps a login of its own in `<state_dir>/loops/agy-login` (`bwrap` replaces only agy's data directory; `AGY_CLI_FORCE_FILE_TOKEN_STORAGE=1`), preferred over the default file in the `bwrap` sandbox and mounted read-only. `loop auth` and preflight errors share one hint.
- **Claude evidence for logins that report no account in the sandbox.** When `claude auth status` shows no email, organization or expiry there, the credential file's metadata (never its content) identifies the account, so a recent successful call is reused instead of probing on every run.
- **Live loop terminal visibility without supervisor/orch interference.** Detached loop terminals (`--name loop-<RUN_ID>`, falling back once without `--name` only on explicit pre-creation name rejection) now appear as `working` with their `RUN_ID` in `tchelvis status` and the Herdr mod (`hc attach` clients stay hidden), while `supervisor` and `orch` reconciler ignore `cockpit.loop_run` terminals.
- **Cleaned loop runs reporting (`loop clean`).** After `loop clean`, missing run directories report `os registros da execução '<id>' foram removidos (loop clean)` with registered status across `report`, `logs`, `status` (text and additive JSON `records_removed: true`), and `result` (5 lines), while existing directories without `state.json` continue reporting that the engine never started.
- **Supervisor read-only fast path hardened.** Automatic approval now refuses a single `&`, any `$`, parentheses, brace expansion, truncated commands, quoted or escaped sensitive paths, options that run programs or write files (`rg --pre`, `rg --hostname-bin`, `git … --output`, `--ext-diff`, `--textconv`) and unquoted leading globs for `rg`/`git`. Shell tools are never taken for `Read`, a description or the last line of a multi-line command is never taken for the command, and an unreadable `approve …` message never falls back to the screen. Permission menus without a readable message are only answered when the whole request is visible and a single line can be the command.
- **`orch launch --agent` respects shell quoting.** The agent string is split like a POSIX shell, so `--agent "agy --model 'GPT-OSS 120B (Medium)'"` passes the model name as one argument (it used to be cut at every space, which forced a wrapper script). Unbalanced quotes fail at `agent_preflight`, and recovery hints quote a multi-word agent so the printed `launch` command can be pasted back.
- **Pending prompt found by screen structure.** The supervisor now reads Claude Code's permission dialog from its top rule instead of a fixed last-8-lines window (the dialog is taller, so a destructive command shown in it was alerted as `normal` instead of `critical`), and destructive detection for Action Cards also checks hc's `status.msg`. Other agents still use the last 8 lines, cut after an earlier answered prompt.
- **LLM sees the current request apart from earlier output.** The evaluation payload sends `pending_prompt` and `earlier_output` instead of one `recent_hc_output` block, and the model is told to decide only on the current request.
- **Fallback model names from an unnamed llama-server.** A `llama-server` started without `--alias` lists its model as a file path (`/models/<blob hash>`); the fallback picker, card and `(Fallback)` pill now show the local model's name when the hash matches a scanned model, `modelo sem nome · <8 hex>` otherwise. The id sent to the server is unchanged.
- **Supervisor model card overlap.** Model rows put size/source details under the name; `Hide` / `Delete` no longer squeeze the name over them.
- **Daily task tag pill vs saved tag.** When no explicit tag is chosen, the pill now defaults to the active Space only if it is a Home project (otherwise `Geral`) and falls back to `Geral` if a chosen project disappears, ensuring the saved `--project` always matches the displayed pill (`Geral` never passes `--project`).

## [0.2.0] - 2026-11-16

First tagged release, and the first published as `tchelvis` binaries (`x86_64` and `aarch64` musl).

### Added
- **Read-only fast path for permission prompts.** Trivial read-only requests are approved by a fixed rule, without the LLM:
  - `ls`, `cat`, `head`, `tail`, `wc`, `pwd`, `rg`, `grep`, `git status|diff|log|show`, and `Read` of a single file;
  - no shell metacharacters and no sensitive paths;
  - the request is read from `hc`'s status message (the real Claude Code menu), the prompt line, or the line above it;
  - the key is `1` on Claude's "1. Yes" menu, or `y` on `[y/N]` prompts. Claude's "always allow" and "auto mode" options are never chosen.
- **Durable auto-reply audit log.** Every auto-reply that was sent is appended to `auto_replies.jsonl` next to the state file, with its source (`fixed_rule` or `llm`). It survives closed terminals.
- **Hide and delete supervisor models (M1c).**
  - **CLI:** `tchelvis supervisor model hide|unhide|rm`, and `model list --all` to show hidden models.
  - **`model rm`** handles plain files and shards, Hugging Face cache entries (shared blobs are preserved), and Ollama models. It refuses protected models (effective, fallback, or loaded) and asks for `--yes` when stdin is not interactive.
  - **Herdr mod:** Hide/Show and Delete actions in the Supervisor model card. Delete asks for a second click.
- **Fleet child tree.** Child terminals are nested under their parent (`├`/`└`). The parent shows a `⚠ N` badge, and a blocked child pulls its group into "Needs".
- **CPU vs GPU benchmark.**
  - **CLI:** `tchelvis supervisor bench [--runs N] [--json] [--last]` measures the supervisor's real evaluation request on CPU and then on GPU (prompt tok/s, generation tok/s, eval time; median after a warm-up). It restores the previous container state afterwards.
  - **Herdr mod:** a "CPU vs GPU benchmark" card in the Doctor tab, with live progress and the last saved result.
- **GPU backend detection.** `gpu_backend = auto|cuda|vulkan|rocm`:
  - NVIDIA uses CUDA;
  - AMD and Intel use Vulkan (`/dev/dri`);
  - ROCm is available by forcing it in config.

  Before this, the GPU mode only worked on NVIDIA.
- **Self-updater & release pipeline (M5a):**
  - `tchelvis update [--channel stable|nightly] [--check] [--no-setup] [--force]` downloads the release archive from GitHub Releases, verifies its `.sha256` checksum, atomically replaces the binary, and runs `tchelvis setup --set-default-layout`.
  - `install.sh`: POSIX shell installer supporting `--channel stable|nightly` and `--dir`.
  - GitHub Actions release workflow (`.github/workflows/release.yml`) building static `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` binaries for `nightly` (pushes to `main`) and stable `v*` tags, with per-asset `.sha256` and combined `SHA256SUMS`.
  - Dual licensing under `MIT OR Apache-2.0` (`LICENSE-MIT` and `LICENSE-APACHE`).
- **Supervisor model selection (M1).** Pick a GGUF model already on the machine for the `llama.cpp` container, without downloading anything new.
  - **Discovery:** scans the Ollama store, the Hugging Face / llama.cpp / LM Studio caches, and folders from `[llm_container] model_dirs` or `COCKPIT_MODEL_DIRS`.
  - **Filter:** by parameter count (`≤2B / ≤4B / ≤8B / All`, default ≤4B). Embedding and projector models are hidden.
  - **CLI:** `cockpit supervisor model list|set|filter|reset`.
  - **Persistence:** the choice is written into `supervisor.toml` (`[llm_container] model_id`), preserving comments. Switching while loaded recreates the container (`cockpit.model_id` label).
  - **Herdr mod:** a picker card opens from the top-bar LLM pill or the Inspector's LLM card.
- **Agent-facing install guide** (`docs/agent-install.md`) and an `llms.txt` index, so AI agents given the repository URL can detect installed components and install only what is missing (e.g. just the LLM container).
- **Managed `llama.cpp` supervisor LLM container** (`6a38eee`)
  - **Container runtime:** auto-detected in the order Kiri, Podman, Docker. Override with `[llm_container] runtime` or `COCKPIT_CONTAINER_RUNTIME`.
  - **Container:** named `cockpit-supervisor-llm`, running `ghcr.io/ggml-org/llama.cpp:server` (or `:server-cuda` with GPU offload), served on `127.0.0.1:11435`.
  - **Model source, in order:** `model_path`, then `hf_repo` (cached in `~/.cache/cockpit/llama.cpp`), then the GGUF blob of an existing Ollama download.
  - **Explicit lifecycle:**
    - New `cockpit supervisor load` starts the container and waits for `/health`.
    - `unload` stops it.
    - Background ticks never start it.
    - It stops automatically after `idle_unload_secs` with no active sessions.
  - **Configuration:** new `[llm_container]` table (`mode = auto|on|off`) and `COCKPIT_LLM_CONTAINER`. With the container mode off, the existing HTTP backends work as before.
  - **Status fields:** the supervisor snapshot and `cockpit doctor` now report `llm_managed`, `llm_runtime`, and `llm_container_state`.
  - **Herdr mod:**
    - A single **Load LLM ⇄ Unload LLM** button.
    - New disabled and starting states in the LLM pill.
    - **Poll Now** loads the LLM first when it is stopped.
- **AI account usage** (`aeb76e7`, `69c6d19`): `cockpit usage [--json] [--detect]` integrates `ai-usagebar`.
  - In the mod, a **Usage** button on the bottom bar, left of Home, opens a compact AI Usage popover with one tab per provider.
  - The popover shows quota and elapsed-time bars, a reset countdown, and refresh, detect, open-TUI, and login actions.
- **GPU offload toggle** (`2ce7dc3`): `cockpit supervisor gpu <on|off> [--no-warm]` and a clickable `num_gpu` pill. The choice is persisted in `~/.local/state/cockpit/supervisor_gpu`; `COCKPIT_ALLOW_GPU` still takes precedence.
- **LLM memory usage** (`2ce7dc3`): `llm_ram_bytes` / `llm_vram_bytes` in the supervisor snapshot, shown in the LLM pill (e.g. `1.1 GB VRAM · 0.2 GB RAM`).
- **Home overview** (`58213b9`): Home panel with KPI cards, Daily Tasks (`cockpit task add|list|toggle|remove|clear-done`), Projects in Development (live Herdr Spaces plus git status), and Immediate Attention cards (`cockpit home [--json]`).
- **More LLM backends** (`4c842ec`): `llama.cpp`, LM Studio, and OpenAI-compatible APIs (OpenRouter, OpenAI, Groq, Gemini, DeepSeek), selected with `llm_backend` / `COCKPIT_LLM_BACKEND`. API keys are read from `COCKPIT_LLM_API_KEY`, `OPENAI_API_KEY`, or `OPENROUTER_API_KEY`.
- **Live LLM health** (`d414a7a`): connection and model status in the top bar, Home, Doctor, and `cockpit doctor`.

### Changed
- **Breaking — Rename `cockpit` → `tchelvis` (M5a):**
  - Binary renamed to `tchelvis` (package version bumped to `0.2.0`).
  - Directories moved to `~/.config/tchelvis`, `~/.local/share/tchelvis`, `~/.local/state/tchelvis`, and `~/.cache/tchelvis/llama.cpp`.
  - Environment variable prefix changed from `COCKPIT_*` to `TCHELVIS_*` (`TCHELVIS_LLM_CONTAINER`, `TCHELVIS_CONTAINER_RUNTIME`, `TCHELVIS_LLM_BACKEND`, `TCHELVIS_LLM_HOST`, `TCHELVIS_LLM_MODEL`, `TCHELVIS_LLM_API_KEY`, `TCHELVIS_ALLOW_GPU`, `TCHELVIS_JIRA_TOKEN`, `TCHELVIS_LINEAR_TOKEN`, `TCHELVIS_SLACK_TOKEN`).
  - Managed LLM container renamed to `tchelvis-supervisor-llm`; Herdr Desktop mod/layout renamed to `tchelvis`; systemd unit renamed to `tchelvis.service`; agent plugins renamed to `tchelvis.js`. (`cockpit.*` `hc` labels remain readable.)
  - **No automatic migration:** uninstall `cockpit` (`cockpit uninstall --purge`), remove any old `cockpit-supervisor-llm` container, and rename `COCKPIT_*` variables in your shell rc files to `TCHELVIS_*` before installing `tchelvis`.
- `supervisor.toml` may now contain only some keys. Missing keys fall back to defaults instead of the whole file being ignored, and parse errors are logged.
- **Project renamed** to **Tchelvis Supervisor**. The GitHub repository moved to `fabriciof807/Tchelvis-Supervisor` and is now public; the README was rewritten in English around the container setup (`fb01886`).
- **Default supervisor model** is now `lfm3:1.7b` (previously `qwen2.5:1.5b-instruct`). Evaluations run with `think: false` for fast CPU responses (`4c842ec`, `0c5c727`).
- **Action Card policy** (`2ce7dc3`):
  - Only agents actually blocked on a prompt can raise a card.
  - `Critical` urgency is reserved for destructive commands detected deterministically; `Critical` suggested by the LLM is downgraded to `Normal`.
  - Stale cards are cleared on the next tick.
  - Forbidden-command matching respects word boundaries.
- **Top bar** (`7a8c17b`):
  - The supervisor pill shows only `Idle`, `Paused`, `Evaluating`, or `Watching (N)`.
  - The mode switcher and the Poll Now / LLM buttons stay fixed in the center.
- **Herdr mod design** (`56f11ae`, `58213b9`): aligned with the landing-page design, with multi-theme GPUI support and better readability.

### Fixed
- **Supervisor auto-reply policy**, hardened over ten rounds of host testing:
  - The LLM never approves a permission request; only the read-only fast path may. An LLM reply on a permission menu becomes an operator alert.
  - Auto-replies type only single keys: `y`/`n` (only on yes/no prompts), a menu number, or Enter. Free text becomes an alert.
  - Questions (`kind=question`) are never auto-answered. A blocked agent with a visible prompt and an LLM `NO_OP` raises an alert.
  - Blocked agents are evaluated first (oldest first), with at most `max_evals_per_tick` evaluations per tick (default 1).
  - A failed `hc send` is logged, shown as the card's error, and labelled `AUTO_REPLY_FAILED`; it no longer counts as a reply.
  - When a guardrail turns an LLM reply into an alert, the card says `[Sugestão da LLM não enviada]`. Stray CJK characters from small models are removed from card text.
- **Fleet attach:** the attach tab uses the terminal address, not its name (terminals created by agents have no name). A click focuses a pane only if it is in the current list; otherwise it attaches.
- **LLM pill:** Hugging Face cache models show the snapshot name instead of the blob hash.
- **Host test fixes** (`50db329` and the following commit):
  - **Antigravity (`agy`) detection:** agy sessions are recognized as agents instead of plain shells.
  - **Supervisor LLM evaluations:** they now finish:
    - `-np 1` gives a single slot with the whole context;
    - free-text fields are length-limited and `max_tokens` is set to 384;
    - evaluation errors are shown in the Inspector instead of being dropped.
  - **Blocked agents:** they show a **Pending prompt** card that does not depend on the LLM:
    - Approve / Deny / Open in Semi-Auto, only Open in Alert Only / Paused;
    - destructive prompts cannot be approved, and the new `cockpit supervisor respond` refuses them server-side.
  - **Container runtime:** in auto mode, the next runtime is tried when the first fails (e.g. broken rootless Podman), and the one that worked is remembered.
  - **Fleet cards:** show the agent's icon.
  - **Clicking a background session:**
    - a live session now opens an attach tab whenever it has no visible pane, including `hc run --bg` sessions;
    - a finished session opens its transcript.

    The Inspector shows which path the click took.
- **Background agents:** clicking a `herdr-core` session that has no pane now attaches it in a new tab. `herdr-core attach` / `hc attach` clients are no longer listed as duplicate agents (`27ad04e`).
- **Agent list:**
  - Plain shells left behind after an agent exits are no longer shown as agents.
  - Finished terminals are normalized to `Done`/`Error` instead of staying `Blocked` or `Working`.
  - Idle sessions no longer trigger LLM evaluations (`3adf6b3`).
- **Supervisor auto-replies** (`0c5c727`):
  - Background-agent selection was fixed.
  - Auto-replies use `hc send --force` and reset the agent's `cockpit.*` status labels.
- **Providers and setup** (`d414a7a`):
  - The provider status field was fixed.
  - `hc connect` terminal discovery was fixed.
  - The hooks `PATH` and state handling were fixed.
- **Mod rendering:**
  - The invalid `transparent` color was replaced with a theme background token (`df32ab8`).
  - Vertical text clipping in previews was fixed (`60b2e87`).

## [0.1.0] - 2026-11-13

Initial import (`1d2563c`).

### Added
- Single Rust binary `cockpit` containing the CLI, a Unix Domain Socket daemon (`cockpit daemon`), the Ratatui TUI (`cockpit tui`), and the Herdr Desktop mod and `cockpit` layout.
- **Attention-first Fleet Queue:**
  - Tracks live `hc` terminal states and `OSC 7501` agent hooks.
  - Pins `blocked` agents (`auth` > `permission` > `question`) to the top and rolls blocked subagents up to their parent session.
  - Groups sessions into `Needs You` / `Active` / `Done`.
- **Read-only context correlation:**
  - Sources: GitHub PRs and CI, GitLab MRs, Jira and Linear tickets, Slack threads, and local `RUNBOOK.md` runbooks.
  - Inputs: each agent's worktree, branch, commit subject, and `cockpit.*` labels.
- **Durable FSM Supervisor:**
  - Watches sessions deterministically and evaluates blocked agents with a local Ollama model (CPU-only `num_gpu: 0`, auto-unload after 180 s idle).
  - Modes: Semi-Auto, Alert Only, Paused.
  - Operator Action Cards, a per-agent loop breaker, and an Auto-Reply Audit Log.
  - State persisted in `~/.local/state/cockpit/supervisor_state.json`.
- CLI commands `status`, `watch`, `inspect`, `label`, `daemon`, `supervisor …`, `setup`, `doctor`, and `uninstall [--purge]`.

[Unreleased]: https://github.com/fabriciof807/Tchelvis-Supervisor/compare/v0.2.0...main
[0.2.0]: https://github.com/fabriciof807/Tchelvis-Supervisor/compare/1d2563c...v0.2.0
[0.1.0]: https://github.com/fabriciof807/Tchelvis-Supervisor/tree/1d2563c
