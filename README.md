# Tchelvis Supervisor

A local-first fleet dashboard, CLI, and Durable FSM Supervisor for tracking and orchestrating coding agents running through [Herdr](https://herdr.dev) (`herdr-core` / `hc 0.1.20+`). It ships as a single binary named `cockpit`.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> **AI agents:** to install, update, or partially install this project (e.g. only the LLM container
> when the CLI is already installed), follow [`docs/agent-install.md`](docs/agent-install.md).
> An index for LLMs is available at [`llms.txt`](llms.txt).

---

## Overview & Architecture

Tchelvis Supervisor combines an **Attention-First Fleet Queue & Context Correlator** with a **Durable FSM Supervisor** in one Rust binary. You can see what your parallel coding agents are doing and step in safely. Your repositories are never modified and no data is sent to a cloud backend.

- **`cockpit` binary**: Rust CLI, Unix Domain Socket daemon (`cockpit daemon`), Durable FSM Supervisor (`src/supervisor/`), Ratatui TUI (`cockpit tui`), and a Herdr Desktop mod (`assets/herdr-app/`).
- **Agent session detection**: Monitors active coding agent sessions (`claude`, `opencode`, `pi`, `codex`, `aider`, `goose`, `cursor-agent`, `amp`, `OSC 7501` status hooks, or `cockpit.*` labeled sessions). Plain idle shells (`zsh`, `bash`, `fish`, `sh`, `nu`, `dash`, `tmux`, `screen`) and supervisor/UI processes are ignored.
- **Containerized supervisor LLM**: Stalled or blocked agents are triaged by a deterministic watcher plus a small local model (`lfm3:1.7b` by default). The model is served by **`llama.cpp` (`llama-server`) in a managed container** (`cockpit-supervisor-llm`). The container only runs after you explicitly click **Load LLM**. When stopped, it uses **0 B of RAM/VRAM**. Ollama, LM Studio, and OpenAI-compatible APIs remain available as alternative backends.
- **State persistence**: Supervisor state lives in `~/.local/state/cockpit/supervisor_state.json`. Closed and plain-shell terminals are pruned automatically.

---

## Use Cases

### 1. Multi-agent fleet triage across git worktrees
Running Claude Code, OpenCode, Pi, Codex, Aider, or Goose in parallel across several worktrees fragments your attention. The supervisor tracks `hc` terminal states and `OSC 7501` hooks continuously:
- `blocked` agents are pinned to the top of the **Fleet Queue**, ordered `auth` > `permission` > `question`.
- Blocked subagents are rolled up to their parent session.
- Sessions are split into `Needs You`, `Active`, and `Done`, so you know which terminal needs you next.

### 2. Zero-idle autonomous supervision with guardrails
In `Semi-Auto (Whitelist)` mode, the supervisor can reply to safe, whitelisted prompts while enforcing strict guardrails:
- **Explicit LLM lifecycle**: The supervisor LLM is a container that only you start (**Load LLM** / **Poll Now**) and stop (**Unload LLM**). Background ticks never start it. It also stops automatically after `idle_unload_secs` (default 180 s) with no active sessions. Inference is CPU-only by default, which keeps GPU VRAM free.
- **Action Cards**: Dangerous or non-whitelisted commands such as `rm -rf` or `git push --force` are never sent automatically. They become an **Action Card**, where you can inspect, edit, approve, or ignore the suggested payload. Only agents that are actually blocked on a prompt produce cards.
- **Loop breaker and audit log**: Repeated auto-replies to the same agent are capped per agent. Every auto-reply is recorded in the **Auto-Reply Audit Log** (TUI and Herdr Desktop Inspector).

### 3. Automatic PR, CI, ticket, Slack, and runbook correlation
Each agent's git worktree, branch name, commit subject, and `cockpit.*` labels are used to link related items, all read-only:
- **GitHub PRs / GitLab MRs**, CI check runs, reviews, and deployments.
- **Jira and Linear tickets**, e.g. `ENG-142` taken from branch names or labels. A denylist filters false positives such as `UTF-8` or `SHA-256`.
- **Slack threads** and local Markdown runbooks (`RUNBOOK.md`), matched by frontmatter or by failing CI job name.

### 4. Herdr Desktop mod + terminal TUI
Manage your fleet from **Herdr Desktop** (`herdr-app`) with the multi-zone `cockpit` layout (Top Bar, Fleet Queue, Inspector, Status Bar). Over SSH or inside containers, use `cockpit tui` instead.

---

## How to Install

### Prerequisites

- **Linux** (`x86_64` or `aarch64`)
- **[Herdr](https://herdr.dev)**: the `hc` CLI `0.1.20+` must be in `PATH`. `herdr-app` is optional and provides the desktop UI.
- **Rust toolchain** (`cargo` 1.75+) to build from source
- **A container runtime** for the supervisor LLM: [Kiri](https://runkiri.com), [Podman](https://podman.io), or [Docker](https://docs.docker.com/engine/install/). They are auto-detected in that order.
  - For GPU offload you also need NVIDIA container support: `nvidia-container-toolkit` for Docker, or CDI for Podman.
- **A GGUF model** for the supervisor. See [Supervisor LLM setup](#supervisor-llm-setup-llamacpp-container).

Without a container runtime, the supervisor falls back to the configured HTTP backend (Ollama by default). See [Alternative LLM backends](#alternative-llm-backends).

### Build and install from source

```bash
# 1. Clone the repository
git clone https://github.com/fabriciof807/Tchelvis-Supervisor.git
cd Tchelvis-Supervisor

# 2. Build the release binary and install it to ~/.local/bin
cargo build --release
mkdir -p ~/.local/bin
install -m 0755 target/release/cockpit ~/.local/bin/cockpit

# 3. Install shell helpers, agent hooks, and the Herdr Desktop mod/layout
cockpit setup --set-default-layout

# 4. (Optional) Pre-pull the llama.cpp server image used by the supervisor
docker pull ghcr.io/ggml-org/llama.cpp:server        # or: podman pull ...

# 5. Verify the installation and environment health
cockpit doctor
```

> If `which -a cockpit` lists an older binary (e.g. `~/.cargo/bin/cockpit`) ahead of `~/.local/bin/cockpit`, replace or remove it. The Herdr mod calls plain `cockpit`.

### Supervisor LLM setup (`llama.cpp` container)

The supervisor manages one named container. You do not need to create it yourself:

| Setting | Default |
| :--- | :--- |
| Container name | `cockpit-supervisor-llm` |
| Image (CPU) | `ghcr.io/ggml-org/llama.cpp:server` |
| Image (GPU) | `ghcr.io/ggml-org/llama.cpp:server-cuda` (`--gpus all` on Docker/Kiri, `--device nvidia.com/gpu=all` on Podman) |
| Endpoint | `http://127.0.0.1:11435` (OpenAI-compatible `/v1/chat/completions`) |
| Context size | `4096` |

The GGUF model is resolved in this order:

1. **`model_path`**: a GGUF file on the host. Its directory is mounted read-only at `/models`.
2. **`hf_repo`**: a Hugging Face repository passed to `llama-server -hf`. Downloads are cached in `~/.cache/cockpit/llama.cpp`, so recreating the container does not download the model again.
3. **Existing Ollama download**: if you already ran `ollama pull lfm3:1.7b`, its GGUF blob is reused directly from `$OLLAMA_MODELS`, `~/.ollama/models`, or `/usr/share/ollama/.ollama/models`.

Load, check, and unload the model:

```bash
cockpit supervisor load      # create/start the container and wait for /health
cockpit supervisor status --json | jq '{llm_managed, llm_runtime, llm_container_state, llm_status_detail}'
cockpit supervisor unload    # stop the container (0 B RAM/VRAM)
```

Container settings live in the `[llm_container]` table of `~/.config/cockpit/supervisor.toml`. See [Configuration reference](#configuration-reference).

---

## How to Use

### 1. Herdr Desktop mod (`cockpit` layout)

After `cockpit setup --set-default-layout`, start the background collector daemon and restart **Herdr Desktop** (`herdr-app`):

```bash
cockpit daemon
```

Select the **`cockpit`** layout in Herdr Desktop:
- **Top Bar (`cockpit-pill`)**:
  - Supervisor state: `Idle` / `Watching (N)` / `Evaluating`.
  - LLM status: a disabled state while the container is stopped, a starting state while the model loads, and the runtime plus memory use (e.g. `llama.cpp · 1.2 GB RAM`) when ready.
  - Clickable `num_gpu` CPU/GPU toggle and the mode switcher: `Semi-Auto (Whitelist)`, `Alert Only (Read-Only)`, `Paused`.
  - **Poll Now**: loads the LLM first if the container is stopped.
  - A single **Load LLM ⇄ Unload LLM** button.
  - View switcher (Home, Inspector, Doctor) and **Switch Layout**.
- **Left Sidebar (`cockpit-fleet`)**: Attention-first Fleet Queue (`Needs You` / `Active` / `Done`), Home toggle, `+ Agent` quick launcher, and filter tabs. Clicking an agent with no visible pane (a background `herdr-core` session) attaches it in a new tab.
- **Right Drawer (`cockpit-inspector`)**: Home, Inspector, and Doctor views.
  - **Home**: KPI cards (Projects, Agents, Daily Tasks, Supervisor), Daily Tasks with priority and project tags, Projects in Development (live Herdr Spaces + git status), and Immediate Attention cards.
  - **Inspector**:
    - **Action Cards**: approve, edit the payload, or ignore.
    - **Supervisor state**: rolling summary and Auto-Reply Audit Log.
    - **Output Preview**: the last 25 lines, sanitized.
    - **Controls**: Focus, Peek, Pin, Interrupt, Split Shell, Send Message, Attach Tab, Kill.
    - **Correlation**: `cockpit.*` labels and PR / CI / ticket / Slack / runbook cards.
  - **Doctor**: environment health checks.
- **Bottom Bar (`cockpit-status`)**: supervisor mode, CPU/GPU badge, workspace branch, fleet summary, provider health, and quick `Usage · Home · Inspector · Doctor` toggles. **Usage** opens a compact **AI Usage** popover (powered by `ai-usagebar`, if installed) with per-provider tabs, quota and elapsed-time bars, and reset countdowns.

### 2. Interactive TUI (`cockpit tui`)

```bash
cockpit tui
```

| Key | Action |
| :--- | :--- |
| `j` / `Down`, `k` / `Up` | Select next / previous agent |
| `m` | Cycle supervisor mode (`Semi-Auto` → `Alert Only` → `Paused`) |
| `t` or `r` | Trigger an immediate supervisor tick |
| `u` | Unload the LLM (stops the managed container, or sends `keep_alive: 0` to Ollama) |
| `a` | Approve and send the selected Action Card payload |
| `e` | Edit the selected Action Card payload (`Enter` saves, `Esc` cancels) |
| `i` | Ignore the selected Action Card |
| `l` | Toggle the Auto-Reply Audit Log |
| `q` | Quit |

### 3. CLI commands

```bash
# Fleet
cockpit status                       # live agent fleet
cockpit watch                        # stream updates as agents change state
cockpit inspect t1                   # PR, CI, ticket, Slack, and runbook details for terminal t1
cockpit label t1 issue=ENG-142 pr=88 # manually pin a ticket / PR / Slack thread
cockpit daemon                       # UDS background collector for Herdr Desktop

# AI account usage & quotas (via ai-usagebar)
cockpit usage [--json] [--detect]

# Home overview & daily tasks
cockpit home [--json]
cockpit task list [--json]
cockpit task add [--project <name>] [--priority <high|normal|low>] [--json] <title...>
cockpit task toggle <id> [--json]
cockpit task remove <id> [--json]
cockpit task clear-done [--json]

# Durable Supervisor
cockpit supervisor status --json
cockpit supervisor tick --json
cockpit supervisor load                      # start the LLM container and wait until healthy
cockpit supervisor unload                    # stop the LLM container
cockpit supervisor gpu <on|off> [--no-warm]  # toggle GPU offload
cockpit supervisor approve <agent_id> [payload...]
cockpit supervisor ignore <agent_id>
cockpit supervisor mode <semi-auto|alert-only|paused|cycle>
cockpit supervisor watch                     # long-running watcher daemon
```

### 4. GPU offload

Run `cockpit supervisor gpu on|off`, or click the `num_gpu` pill in the top bar, to switch between CPU-only inference (`-ngl 0`, the default, zero VRAM) and full GPU offload (`-ngl 99` with the CUDA image).
- The choice is saved in `~/.local/state/cockpit/supervisor_gpu`.
- If the container is running, it is recreated with the new setting. `--no-warm` stops it instead.
- `COCKPIT_ALLOW_GPU` takes precedence over the saved choice.
- The long-running `cockpit supervisor watch` daemon reads its configuration only at startup, so restart it after switching.

### 5. Configuration reference

`~/.config/cockpit/supervisor.toml`:

```toml
llm_backend = "ollama"            # used only when the container mode is inactive
ollama_host = "http://localhost:11434"
ollama_model = "lfm3:1.7b"        # model name; also used to find an existing Ollama GGUF
temperature = 0.1
allow_gpu = false
gpu_layers = 99
ollama_timeout_secs = 45
poll_interval_secs = 12
idle_unload_secs = 180
inactivity_trigger_secs = 20
capture_lines = 25
max_auto_replies = 2
state_file_path = "/home/<you>/.local/state/cockpit/supervisor_state.json"
enable_desktop_notifications = true

[hc]
hc_bin = "hc"
list_sessions_cmd = "hc ls --json"
capture_output_cmd = "hc read {agent_id} --lines {lines} --json"
send_input_cmd = "hc send {agent_id} {payload}"
sync_hc_labels = true

[llm_container]
mode = "auto"                     # auto (on when a runtime is found) | on | off
runtime = "auto"                  # auto | kiri | podman | docker
name = "cockpit-supervisor-llm"
image = "ghcr.io/ggml-org/llama.cpp:server"
image_gpu = "ghcr.io/ggml-org/llama.cpp:server-cuda"
port = 11435
ctx_size = 4096
# model_path = "/path/to/model.gguf"
# hf_repo = "owner/repo-GGUF:Q4_K_M"
extra_args = []
```

> **Note:** `supervisor.toml` is currently parsed as a whole. If a top-level key is missing, the file is ignored and defaults are used. Keep every key shown above. The `[llm_container]` table itself is optional.

| Environment variable | Description | Default |
| :--- | :--- | :--- |
| `COCKPIT_LLM_CONTAINER` | Container mode: `auto`, `on`, `off` (also `1` / `0`) | `auto` |
| `COCKPIT_CONTAINER_RUNTIME` | Force `kiri`, `podman`, or `docker` | auto-detect |
| `COCKPIT_LLM_BACKEND` | Backend when the container mode is inactive: `ollama`, `llama-cpp`, `lm-studio`, `openai-compatible` | `ollama` |
| `COCKPIT_LLM_HOST` / `OLLAMA_HOST` | Base URL for that backend | `http://localhost:11434` |
| `OLLAMA_FALLBACK_HOST` | Fallback Ollama host when the primary is unreachable | none |
| `COCKPIT_LLM_MODEL` / `OLLAMA_MODEL` | Model identifier | `lfm3:1.7b` |
| `COCKPIT_LLM_API_KEY` | Bearer token for `openai-compatible` (also reads `OPENAI_API_KEY` / `OPENROUTER_API_KEY`) | none |
| `COCKPIT_ALLOW_GPU` | Allow GPU offload; overrides `cockpit supervisor gpu` | `false` |
| `OLLAMA_MODELS` | Extra Ollama model store searched for GGUF blobs | none |
| `HC_BIN` | Path to the `hc` binary | `hc` |

### 6. Alternative LLM backends

Set `COCKPIT_LLM_CONTAINER=off` (or `[llm_container] mode = "off"`) to use an HTTP backend you manage yourself:

| Backend | `llm_backend` | Typical `ollama_host` | Notes |
| :--- | :--- | :--- | :--- |
| **Ollama** | `ollama` | `http://localhost:11434` | `ollama pull lfm3:1.7b`; CPU-only by default via `num_gpu: 0`, unloaded with `keep_alive: 0` after 180 s idle. |
| **llama.cpp (self-managed)** | `llama-cpp` | `http://localhost:8080` | `llama-server -m model.gguf --port 8080 -ngl 0` |
| **LM Studio** | `lm-studio` | `http://localhost:1234` | Start the Local Server with an instruction model that supports structured JSON. |
| **OpenAI-compatible / cloud** | `openai-compatible` | e.g. `https://openrouter.ai/api/v1` | Works with OpenRouter, OpenAI, Groq, Gemini (OpenAI endpoint), DeepSeek. Export `COCKPIT_LLM_API_KEY`; keys are never written to disk unless you set `api_key` yourself. |

Verify the connection with `cockpit supervisor status --json` or `cockpit doctor`.

### 7. Optional provider integrations (`~/.config/cockpit/config.toml`)

Local git worktrees, `RUNBOOK.md` files, and authenticated `gh` / `glab` CLIs work out of the box. To enable more read-only integrations, export credentials in your environment. Plaintext tokens are never stored on disk.

| Provider | Configuration |
| :--- | :--- |
| **GitHub** | Uses `gh auth status`, or `export GITHUB_TOKEN=...` |
| **GitLab** | Uses `glab auth status`, or `export GITLAB_TOKEN=...` |
| **Jira** | `[jira] base_url = "https://your-org.atlassian.net"` in `config.toml` and `export COCKPIT_JIRA_TOKEN=...` |
| **Linear** | `export COCKPIT_LINEAR_TOKEN=...` |
| **Slack** | `export COCKPIT_SLACK_TOKEN=...` |

### 8. Uninstall

```bash
# Roll back hooks, shell blocks, and Herdr Desktop mod files (keeps cache/config)
cockpit uninstall

# Full removal: also deletes ~/.config/cockpit, ~/.local/share/cockpit,
# ~/.local/state/cockpit, and the cockpit binary itself
cockpit uninstall --purge && hash -r

# Remove the supervisor LLM container and its download cache
docker rm -f cockpit-supervisor-llm && rm -rf ~/.cache/cockpit/llama.cpp
```

---

## License

This project is licensed under the [MIT License](LICENSE).
