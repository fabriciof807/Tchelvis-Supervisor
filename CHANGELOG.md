# Changelog

All notable changes to Tchelvis Supervisor (binary `tchelvis`, previously `cockpit`) are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
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
- **Host-matched API keys for `openai-compatible`.** Provider environment variables are now matched against the target host (`openai.com` → `OPENAI_API_KEY`, `openrouter.ai` → `OPENROUTER_API_KEY`, `deepseek.com` → `DEEPSEEK_API_KEY`, `opencode.ai` → `OPENCODE_API_KEY`); an `OPENAI_API_KEY` is no longer sent to non-OpenAI hosts. Explicit `TCHELVIS_LLM_API_KEY` (and `TCHELVIS_FALLBACK_API_KEY` / key file for fallback) still take precedence.

### Fixed
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
