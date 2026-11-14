# Changelog

All notable changes to Tchelvis Supervisor (binary `cockpit`, to be renamed `tchelvis`) are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> No version has been tagged or published yet. The first tagged release is planned for roadmap
> milestone M5a (rename to `tchelvis` + release pipeline). Until then, `0.1.0` is the version of the
> initial import, and everything after it is listed under **Unreleased**.

## [Unreleased]

### Added
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

[Unreleased]: https://github.com/fabriciof807/Tchelvis-Supervisor/compare/1d2563c...main
[0.1.0]: https://github.com/fabriciof807/Tchelvis-Supervisor/tree/1d2563c
