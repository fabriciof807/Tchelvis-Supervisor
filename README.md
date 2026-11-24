# Tchelvis Supervisor

A local-first fleet dashboard, CLI, and Durable FSM Supervisor for tracking and orchestrating coding agents running through [Herdr](https://herdr.dev) (`herdr-core` / `hc 0.1.20+`). It ships as a single binary named `tchelvis`.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE-APACHE)

> **AI agents:** to install, update, or partially install this project (e.g. only the LLM container
> when the CLI is already installed), follow [`docs/agent-install.md`](docs/agent-install.md).
> An index for LLMs is available at [`llms.txt`](llms.txt).

---

## Overview & Architecture

Tchelvis Supervisor combines an **Attention-First Fleet Queue & Context Correlator** with a **Durable FSM Supervisor** in one Rust binary. You can see what your parallel coding agents are doing and step in safely. Your repositories are never modified and no data is sent to a cloud backend.

- **`tchelvis` binary**: Rust CLI, Unix Domain Socket daemon (`tchelvis daemon`), Durable FSM Supervisor (`src/supervisor/`), Ratatui TUI (`tchelvis tui`), and a Herdr Desktop mod (`assets/herdr-app/`).
- **Agent session detection**: Monitors active coding agent sessions (`claude`, `opencode`, `pi`, `codex`, `aider`, `goose`, `cursor-agent`, `amp`, `OSC 7501` status hooks, or `cockpit.*` labeled sessions). Plain idle shells (`zsh`, `bash`, `fish`, `sh`, `nu`, `dash`, `tmux`, `screen`) and supervisor/UI processes are ignored.
- **Containerized supervisor LLM**: Stalled or blocked agents are triaged by a deterministic watcher plus a small local model (`lfm3:1.7b` by default). The model is served by **`llama.cpp` (`llama-server`) in a managed container** (`tchelvis-supervisor-llm`). The container only runs after you explicitly click **Load LLM**. When stopped, it uses **0 B of RAM/VRAM**. Ollama, LM Studio, and OpenAI-compatible APIs remain available as alternative backends.
- **State persistence**: Supervisor state lives in `~/.local/state/tchelvis/supervisor_state.json`. Closed and plain-shell terminals are pruned automatically. Multi-agent orchestration state (runs, DAG tasks, fenced dispatches, and mailbox messages) lives in `~/.local/share/tchelvis/orch.db` (SQLite WAL), reconciled deterministically by `tchelvis daemon` with zero LLM dependency.

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
Manage your fleet from **Herdr Desktop** (`herdr-app`) with the multi-zone `tchelvis` layout (Top Bar, Fleet Queue, Inspector, Status Bar). Over SSH or inside containers, use `tchelvis tui` instead.

### 5. Event-driven multi-agent orchestration (`tchelvis orch`)
A coordinator agent running inside a Herdr terminal can delegate work to parallel child coding agents without polling loops or copy-pasting:
- Invoke **`/orchestration-tchelvis`** inside Claude Code, OpenCode, Pi, Codex, or Gemini CLI (or run `tchelvis orch skill`) to launch workers in one command (`tchelvis orch launch`), define DAG tasks, and manage recovery (`send -> retry -> stop`) in shared or isolated git worktrees (`--worktree current|new-child`).
- `tchelvis daemon` manages `orch.db` (SQLite WAL) and wakes coordinators or workers atomically via `hc send --if idle`—including a `1500 ms` post-turn silent-stop safety net and a `3000 ms` blocked-prompt fallback that work even when the local supervisor LLM is unloaded.
- Coordinators with active child dispatches surface as **`Waiting on children (N)`** in `tchelvis status` and the Herdr Desktop mod.

---

## How to Install

### Prerequisites

- **Linux** (`x86_64` or `aarch64`)
- **[Herdr](https://herdr.dev)**: the `hc` CLI `0.1.20+` must be in `PATH`. `herdr-app` is optional and provides the desktop UI.
- **A container runtime** for the supervisor LLM: [Kiri](https://runkiri.com), [Podman](https://podman.io), or [Docker](https://docs.docker.com/engine/install/). They are auto-detected in that order.
  - For GPU offload you also need NVIDIA container support: `nvidia-container-toolkit` for Docker, or CDI for Podman.
- **A GGUF model** for the supervisor. See [Supervisor LLM setup](#supervisor-llm-setup-llamacpp-container).
- **Rust toolchain** (`cargo` 1.75+) — only needed if building from source.

Without a container runtime, the supervisor falls back to the configured HTTP backend (Ollama by default). See [Alternative LLM backends](#alternative-llm-backends).

### Quick install (prebuilt binary)

Install the latest release into `~/.local/bin` and run `tchelvis setup --set-default-layout`:

```bash
# Stable release
curl -fsSL https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/install.sh | sh

# Nightly build (from main)
curl -fsSL https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/install.sh | sh -s -- --channel nightly
```

To update an existing installation:

```bash
tchelvis update                       # updates on the remembered channel (default: stable)
tchelvis update --channel nightly     # switch to or update the nightly channel
tchelvis update --check               # check for updates without installing
```

### Build and install from source (alternative)

```bash
# 1. Clone the repository
git clone https://github.com/fabriciof807/Tchelvis-Supervisor.git
cd Tchelvis-Supervisor

# 2. Build the release binary and install it to ~/.local/bin
cargo build --release
mkdir -p ~/.local/bin
install -m 0755 target/release/tchelvis ~/.local/bin/tchelvis

# 3. Install shell helpers, agent hooks, /orchestration-tchelvis skill, and the Herdr Desktop mod/layout
tchelvis setup --set-default-layout

# 4. (Optional) Pre-pull the llama.cpp server image used by the supervisor
docker pull ghcr.io/ggml-org/llama.cpp:server        # or: podman pull ...

# 5. Verify the installation and environment health
tchelvis doctor
```

> `tchelvis setup` (also run automatically by `tchelvis update`) applies the `shell`, `claude`, `opencode`, `pi`, `herdr-mod`, and `skills` targets by default (plus `systemd` with `--systemd`; filter with `--only` / `--skip`). The `skills` target installs **`/orchestration-tchelvis`** for Claude Code, OpenCode, Pi, Codex, Gemini CLI, and `~/.agents/skills`, and `tchelvis doctor` verifies `orch.db`, `tchelvis daemon`, and skill installation alongside `hc` and LLM health.
>
> If `which -a tchelvis` lists an older binary (e.g. `~/.cargo/bin/tchelvis`) ahead of `~/.local/bin/tchelvis`, replace or remove it. The Herdr mod calls plain `tchelvis`.

### Supervisor LLM setup (`llama.cpp` container)

The supervisor manages one named container. You do not need to create it yourself:

| Setting | Default |
| :--- | :--- |
| Container name | `tchelvis-supervisor-llm` |
| Image (CPU) | `ghcr.io/ggml-org/llama.cpp:server` |
| Image (GPU) | `ghcr.io/ggml-org/llama.cpp:server-cuda` (`--gpus all` on Docker/Kiri, `--device nvidia.com/gpu=all` on Podman) |
| Endpoint | `http://127.0.0.1:11435` (OpenAI-compatible `/v1/chat/completions`) |
| Context size | `4096` |

The GGUF model is resolved in this order:

1. **`model_path`**: a GGUF file on the host. Its directory is mounted read-only at `/models`.
2. **`hf_repo`**: a Hugging Face repository passed to `llama-server -hf`. Downloads are cached in `~/.cache/tchelvis/llama.cpp`, so recreating the container does not download the model again.
3. **Existing Ollama download**: if you already ran `ollama pull lfm3:1.7b`, its GGUF blob is reused directly from `$OLLAMA_MODELS`, `~/.ollama/models`, or `/usr/share/ollama/.ollama/models`.

Load, check, and unload the model:

```bash
tchelvis supervisor load      # create/start the container and wait for /health
tchelvis supervisor status --json | jq '{llm_managed, llm_runtime, llm_container_state, llm_status_detail}'
tchelvis supervisor unload    # stop the container (0 B RAM/VRAM)
```

Container settings live in the `[llm_container]` table of `~/.config/tchelvis/supervisor.toml`. See [Configuration reference](#configuration-reference).

#### API fallback model (optional, container mode)

When the managed `llama.cpp` container fails to load or stops answering `/health`, the supervisor can fall back automatically to an OpenAI-compatible API endpoint until you ask to return to the primary model:

```bash
printf '%s' "$OPENROUTER_API_KEY" | tchelvis supervisor fallback key   # saves ~/.config/tchelvis/fallback_api_key (0600)
tchelvis supervisor fallback set --host https://openrouter.ai/api --model openai/gpt-4.1-mini
tchelvis supervisor fallback status
tchelvis supervisor fallback off                                       # try returning to the primary container
```

Terminal output is never sent to a remote API unless both `[fallback]` `host` and `model` are set and a key resolves (`TCHELVIS_FALLBACK_API_KEY`, `~/.config/tchelvis/fallback_api_key`, `TCHELVIS_LLM_API_KEY`, or a host-matched provider key: `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `OPENCODE_API_KEY`).

---

## How to Use

### 1. Herdr Desktop mod (`tchelvis` layout)

After `tchelvis setup --set-default-layout`, start the background collector daemon and restart **Herdr Desktop** (`herdr-app`):

```bash
tchelvis daemon
```

Select the **`tchelvis`** layout in Herdr Desktop:
- **Top Bar (`tchelvis-pill`)**:
  - Supervisor state: `Idle` / `Watching (N)` / `Evaluating`.
  - LLM status: a disabled state while the container is stopped, a starting state while the model loads, and the runtime plus memory use (e.g. `llama.cpp · 1.2 GB RAM`) when ready.
  - Clickable `num_gpu` CPU/GPU toggle and the mode switcher: `Semi-Auto (Whitelist)`, `Alert Only (Read-Only)`, `Paused`.
  - **Poll Now**: loads the LLM first if the container is stopped.
  - A single **Load LLM ⇄ Unload LLM** button.
  - View switcher (Home, Inspector, Doctor) and **Switch Layout**.
- **Left Sidebar (`tchelvis-fleet`)**: Attention-first Fleet Queue (`Needs You` / `Active` / `Done`), Home toggle, `+ Agent` quick launcher, and filter tabs. Coordinators with active child dispatches show a `Waiting on children (N)` badge. Clicking an agent with no visible pane (a background `herdr-core` session) attaches it in a new tab.
- **Right Drawer (`tchelvis-inspector`)**: Home, Inspector, and Doctor views.
  - **Home**: KPI cards (Projects, Agents, Daily Tasks, Supervisor), Daily Tasks with priority and project tags, Projects in Development (live Herdr Spaces + git status), and Immediate Attention cards.
  - **Inspector**:
    - **Action Cards**: approve, edit the payload, or ignore.
    - **Supervisor state**: rolling summary and Auto-Reply Audit Log.
    - **Output Preview**: the last 25 lines, sanitized.
    - **Controls**: Focus, Peek, Pin, Interrupt, Split Shell, Send Message, Attach Tab, Kill.
    - **Correlation**: `cockpit.*` labels and PR / CI / ticket / Slack / runbook cards.
  - **Doctor**: environment health checks.
- **Bottom Bar (`tchelvis-status`)**: supervisor mode, CPU/GPU badge, workspace branch, fleet summary, provider health, and quick `Usage · Home · Inspector · Doctor` toggles. **Usage** opens a compact **AI Usage** popover (powered by `ai-usagebar`, if installed) with per-provider tabs, quota and elapsed-time bars, and reset countdowns.

### 2. Interactive TUI (`tchelvis tui`)

```bash
tchelvis tui
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
tchelvis status                       # live agent fleet
tchelvis watch                        # stream updates as agents change state
tchelvis inspect t1                   # PR, CI, ticket, Slack, and runbook details for terminal t1
tchelvis label t1 issue=ENG-142 pr=88 # manually pin a ticket / PR / Slack thread
tchelvis daemon                       # UDS background collector for Herdr Desktop

# AI account usage & quotas (via ai-usagebar)
tchelvis usage [--json] [--detect]

# Home overview & daily tasks
tchelvis home [--json]
tchelvis task list [--json]
tchelvis task add [--project <name>] [--priority <high|normal|low>] [--json] <title...>
tchelvis task toggle <id> [--json]
tchelvis task remove <id> [--json]
tchelvis task clear-done [--json]

# Durable Supervisor
tchelvis supervisor status --json
tchelvis supervisor tick --json
tchelvis supervisor load                      # start the LLM container and wait until healthy
tchelvis supervisor unload                    # stop the LLM container
tchelvis supervisor gpu <on|off> [--no-warm]  # toggle GPU offload
tchelvis supervisor approve <agent_id> [payload...]
tchelvis supervisor ignore <agent_id>
tchelvis supervisor mode <semi-auto|alert-only|paused|cycle>
tchelvis supervisor watch                     # long-running watcher daemon
tchelvis supervisor fallback status [--json]  # fallback config & state (never outputs the key)
tchelvis supervisor fallback key              # read fallback API key from stdin → 0600 key file
tchelvis supervisor fallback set [--host <url>] [--model <name>]
tchelvis supervisor fallback models [--json] [--filter <text>]
tchelvis supervisor fallback off              # try returning to the primary local model
tchelvis supervisor fallback clear            # remove [fallback] and deactivate
tchelvis supervisor rules [--json]            # show effective [guardrails] rules, sources, and warnings
tchelvis supervisor rules test "just test"    # dry-run fixed guardrail rules (never sends anything)

# Self-update
tchelvis update [--channel stable|nightly] [--check] [--no-setup] [--force]
```

#### Multi-Agent Orchestration (`tchelvis orch`)

Event-driven parent-child agent orchestration over Herdr (`hc`) terminals, backed by `~/.local/share/tchelvis/orch.db` (SQLite WAL) and `tchelvis daemon`. Reconciliation is 100% deterministic (zero-LLM), so coordinator/worker orchestration works identically even when the local supervisor LLM is unloaded.

- **Invoke inside coding agents**: Run **`/orchestration-tchelvis`** in Claude Code, OpenCode, Pi, Codex, or Gemini CLI (installed automatically by `tchelvis setup` / `tchelvis update` via the `skills` target into agent skill paths and `~/.agents/skills`), or print the guide on demand with `tchelvis orch skill`.
- **Automatic wakeup (no polling loops)**: Coordinators dispatch workers and end their turn immediately. `tchelvis daemon` wakes the target terminal with a single-line inbox pointer via Herdr's guarded `hc send --if idle` (never `--force`).
- **Safety nets when workers stop or block**:
  - **`1500 ms` post-turn silent-stop safety net**: If a dispatched worker returns to its prompt (`idle` / `done`) for `>= 1500 ms` without calling `worker_done` or `ask`, `tchelvis daemon` captures the last 25 sanitized lines and wakes the coordinator with an `escalation`.
  - **`3000 ms` blocked-prompt fallback**: If a worker stays `blocked` on a prompt for `>= 3000 ms` (for example, when the local supervisor LLM is unloaded or the command is outside the read-only whitelist), `tchelvis daemon` escalates to the coordinator so the worker never stalls silently.
- **Fleet visibility**: Coordinators waiting on active dispatches render as **`Waiting on children (N)`** in `tchelvis status` and the Herdr Desktop mod.
- **Recovery path (`send -> retry -> stop`)**: Silence is never proof of exit. Nudge live workers with `tchelvis orch send --to dispatch:<id> --body "..."`, re-launch dead/stuck attempts with `tchelvis orch retry <dispatch_id>`, or cancel cleanly with `tchelvis orch stop <dispatch_id>`.

| Subcommand | Purpose & Key Flags |
| :--- | :--- |
| `tchelvis orch launch` | One-shot run/task/dispatch launch with `PATH` agent preflight (`--agent <agent\|cmd> --prompt <str> [--title <str>] [--run <id>] [--worktree current\|new-child]`) |
| `tchelvis orch run-create` | Create a run bound to the coordinator terminal (`[--title <str>] [--coordinator <term>]`) |
| `tchelvis orch task-create` | Add a DAG task (`--run <id> --id <task> --title <str> --prompt <str> [--deps <csv\|json>]`) |
| `tchelvis orch dispatch` | Spawn or attach a worker (`--run <id> --task <task> (--agent <claude\|codex\|gemini\|opencode\|pi\|agy\|cmd> \| --terminal <term>) [--worktree current\|new-child]`) |
| `tchelvis orch send` | Worker report to parent (`--type <worker_done\|escalation\|status> [--outcome succeeded\|failed] --dispatch-id <id> --body <str>`) or parent directive to worker (`--to dispatch:<id> --body <str>`) |
| `tchelvis orch ask` | Worker asks a blocking question and ends turn (`--dispatch-id <id> --question <str>`) |
| `tchelvis orch reply` | Coordinator answers an open question and wakes the worker (`--question-id <id> --body <str>`) |
| `tchelvis orch retry` | Supersede an attempt and relaunch the same task/agent/worktree mode (`<dispatch_id>`) |
| `tchelvis orch stop` | Close worker terminal cleanly, mark dispatch stopped, and restore task to retryable without deleting dirty worktrees (`<dispatch_id>`) |
| `tchelvis orch check` | Read and acknowledge pending inbox messages (`[--run <id>] [--terminal <term>] [--peek]`) |
| `tchelvis orch status` | Inspect run DAG tasks, dispatches, recorded messages, open questions, and unread counts (`[--run <id>]`) |
| `tchelvis orch release` | Opt-in cleanup of a finished worker terminal and clean/merged `--worktree new-child` (`--dispatch-id <id> [--force]`; removes worktree only and keeps `orch/<run>/<task>` branch) |
| `tchelvis orch skill` | Print the self-contained Markdown guide for coordinator and worker agents |

`--agent` takes a preset (`claude`, `claude-code`, `codex`, `gemini`, `opencode`, `pi`, `agy`/`antigravity`) or a full command, split once with POSIX shell quoting. Extra flags go inside the same string, so a model name with spaces stays one argument:

```bash
tchelvis orch launch --agent "agy --model 'GPT-OSS 120B (Medium)'" --prompt "..."
```

`agy`/`antigravity` get `-i` appended when it is missing, because agy only reads the prompt through `-i`. Any other command, including a wrapper script with its own name, receives the worker prompt as its last positional argument.

All mutating `tchelvis orch` subcommands also accept `--request-id <id>` (idempotency receipt) and `--json`:

```bash
# Coordinator: quick one-shot launch in an isolated git worktree, then end turn
tchelvis orch launch --agent claude --title "Implement JWT" \
  --prompt "Add JWT middleware and run cargo test." --worktree new-child

# Coordinator: or create a DAG explicitly, check inbox, reply, message, retry, or stop
tchelvis orch run-create --title "Auth refactor"
tchelvis orch dispatch --run <run_id> --task jwt --title "Implement JWT" \
  --prompt "Add JWT middleware and run cargo test." --agent claude --worktree new-child
tchelvis orch check --run <run_id>
tchelvis orch reply --question-id <q_id> --body "Default token expiry to 15m."
tchelvis orch send --to dispatch:<dispatch_id> --body "Also cover token refresh tests."
tchelvis orch retry <dispatch_id>
tchelvis orch stop <dispatch_id>
tchelvis orch status --run <run_id>
tchelvis orch release --dispatch-id <dispatch_id>

# Worker: report completion, failure, or ask a blocking question
tchelvis orch send --type worker_done --outcome succeeded --dispatch-id <dispatch_id> --body "Implemented JWT and passed cargo test"
tchelvis orch ask --dispatch-id <dispatch_id> --question "Should token expiry default to 15m or 1h?"
```

#### Declarative Multi-Agent Loops (`tchelvis loop`)

Run fixed, repeatable multi-agent topologies (such as `worker-reviewer-critic`, `designer-worker-reviewer`, `planner-worker-tester-critic`, or `fast-worker-reviewer`) in clean headless CLI sessions inside a mandatory sandbox. The caller spawns `--detach` in one Herdr terminal, ends its turn, and `tchelvis daemon` wakes the caller **once** via `orch.db` when the loop finishes (`PASS`, `ERROR`, `ESCALATE`, or `CANCELLED`).

- **Built-in presets and custom workflows:** Four presets (`worker-reviewer-critic`, `designer-worker-reviewer`, `planner-worker-tester-critic`, `fast-worker-reviewer`) work even without a `.loops/` folder; `tchelvis loop init` scaffolds `~/.config/tchelvis/loops.toml` (`0600`) and `<repo>/.loops/`, and `tchelvis loop new <name> --stages ...` creates a validated workflow TOML plus any missing custom role files.
- **Model precedence & repository confirmation:** Stage `provider` and `model` come together from one level (never mixed across levels): (1) `--use <stage>=<cli>[:<model>]`, (2) `[stages.<workflow>.<stage>]` in `.loops/models.toml`, (3) the workflow TOML stage, (4) `[defaults.<role>]` in `.loops/models.toml`, (5) `[defaults.<role>]` in `~/.config/tchelvis/loops.toml`. Any choice coming from repository files requires explicit confirmation (`tchelvis loop models confirm <workflow>` for `--detach` or non-interactive use), tracked by canonical repo path + content hash in `~/.local/state/tchelvis/loops/confirmed.json`.
- **Important operational guarantees & security notes:**
  - **The loop never merges or pushes:** each run clones an isolated workspace at `HEAD` into `~/.local/state/tchelvis/loops/<run-id>/workspace` and commits final changes only to a local `loop/<workflow>-<run-id>` branch, leaving your checkout and `.gitignore` untouched.
  - **`auto_approve_in_sandbox` must be turned on by the user:** `~/.config/tchelvis/loops.toml` defaults to `auto_approve_in_sandbox = false`; `tchelvis loop run` refuses until you review the sandbox boundary and set `auto_approve_in_sandbox = true`. `loops.toml` is only read from `~/.config/tchelvis/`, never from the repository.
  - **`bwrap` shares the network and localhost:** `bubblewrap` (`bwrap`) is the default sandbox (never silently replaced; `inherited` is opt-in inside an existing container and `incus` is planned). `bwrap` keeps `/` read-only, mounts clean `$HOME` / `/tmp` / `/run`, isolates PID/IPC/UTS namespaces, and binds only the run workspace writable, **but network access, `localhost` services, and abstract sockets remain shared**.
  - **Every model is really called once before a loop starts and that spends tokens:** after account/git read-only checks pass and before any workspace, branch, or Herdr terminal is created, `tchelvis loop run` (and `tchelvis loop check --live`) sends `Reply with exactly: OK` once in parallel to every distinct `provider:model` pair in the workflow (using `--tools ""` on `claude`), spending real API tokens/quota to catch bad model names, missing auth, or sandbox failures upfront.
  - **An exported `OPENCODE_API_KEY` is visible to every program started from that shell:** `tchelvis` never reads the OpenCode database or saves keys on disk; it resolves `OPENCODE_API_KEY` from the environment (or interactive `$SHELL -ic`), passes it via process environment only to `opencode` stages and probes, and redacts it from `state.json`, `report.md`, and errors—however, exporting `OPENCODE_API_KEY` in a shell exposes it to any child program started from that shell.

| Subcommand | Purpose & Key Flags |
| :--- | :--- |
| `tchelvis loop init` | Initialize `~/.config/tchelvis/loops.toml` (`0600`) and `<repo>/.loops/` (preserves edited files) |
| `tchelvis loop new <name> --stages <csv>` | Generate a validated `.loops/workflows/<name>.toml` and missing custom role `.md` files |
| `tchelvis loop list [--runs]` | List available workflows (repo > global > preset) or recorded loop runs in `orch.db` |
| `tchelvis loop check <workflow> [--live] [--use ...]` | Validate workflow, config, sandbox, accounts, and `models.toml` references (`--live` also probes models) |
| `tchelvis loop models <workflow>` | Show resolved CLI, model, and source level for every stage |
| `tchelvis loop models set <role>=<cli[:model]> (--project\|--global)` | Update `.loops/models.toml` or `~/.config/tchelvis/loops.toml` preserving comments |
| `tchelvis loop models confirm <workflow>` | Record confirmation for repository-sourced model choices |
| `tchelvis loop auth` / `auth login <cli>` | Check CLI credentials (`claude`, `agy`, `opencode`) or open login in a Herdr terminal |
| `tchelvis loop run <workflow> [prompt \| --input-file <path>]` | Probe models and run headless stages (`[--from-head] [--max-cycles N] [--use ...] [--detach] [--verbose]`) |
| `tchelvis loop status [run_id] [--json]` | Inspect current or final run state (`PASS`/`ERROR`/`ESCALATE`/`CANCELLED`) |
| `tchelvis loop result [run_id]` | Print the canonical 5-line run summary (exit `0`=PASS, `1`=ERROR, `2`=usage, `3`=ESCALATE, `4`=CANCELLED) |
| `tchelvis loop logs [run_id] [--stage <id>]` | Print sanitized per-stage logs |
| `tchelvis loop report [run_id]` | Print sanitized `report.md` (with preflight probes, usage totals, timeline, and sandbox disclosures) |
| `tchelvis loop stop <run_id>` | Send `SIGINT` to cancel an active loop cleanly |
| `tchelvis loop clean [run_id] [--yes]` | Remove finished run directories and clones while keeping `loop/*` git branches |

### 4. GPU offload

Run `tchelvis supervisor gpu on|off`, or click the `num_gpu` pill in the top bar, to switch between CPU-only inference (`-ngl 0`, the default, zero VRAM) and full GPU offload (`-ngl 99` with the CUDA image).
- The choice is saved in `~/.local/state/tchelvis/supervisor_gpu`.
- If the container is running, it is recreated with the new setting. `--no-warm` stops it instead.
- `TCHELVIS_ALLOW_GPU` takes precedence over the saved choice.
- The long-running `tchelvis supervisor watch` daemon reads its configuration only at startup, so restart it after switching.

### 5. Configuration reference

`~/.config/tchelvis/supervisor.toml`:

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
max_evals_per_tick = 1        # LLM evaluations per tick; blocked terminals go first
capture_lines = 25
max_auto_replies = 2
state_file_path = "/home/<you>/.local/state/tchelvis/supervisor_state.json"
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
name = "tchelvis-supervisor-llm"
image = "ghcr.io/ggml-org/llama.cpp:server"
image_gpu = "ghcr.io/ggml-org/llama.cpp:server-cuda"
port = 11435
ctx_size = 4096
# model_path = "/path/to/model.gguf"
# hf_repo = "owner/repo-GGUF:Q4_K_M"
extra_args = []

# [fallback]
# host = "https://openrouter.ai/api"
# model = "openai/gpt-4.1-mini"
```

> **Note:** Partial `supervisor.toml` files are supported; any omitted keys fall back to their defaults.

#### Guardrails (`[guardrails]`)

Add an optional `[guardrails]` table to `~/.config/tchelvis/supervisor.toml` to allow or deny commands deterministically without weakening the built-in safety floor:

```toml
[guardrails]
# Exact command. "*" at the end accepts additional arguments.
allow_commands = ["just test", "cargo test *", "uv run pytest *", "git branch"]

# Beats everything above. Matched anywhere in the pending request.
deny_commands = ["cargo publish", "npm publish", "docker rm", "git commit"]

# Matched by path segment, case-insensitive.
deny_paths = [".kube", ".aws", "prod/secrets"]

# Default: ["Read"]. [] disables. Only Read is supported for now; other names are ignored with a warning.
allow_tools = ["Read"]

builtin_read_only  = true   # ls, cat, rg, git status...
builtin_build_test = true   # cargo test, pytest... (only on [y/N] answered by the LLM)
llm_auto_reply     = true   # false: the LLM only summarizes and alerts
```

Decision order on every tick (from highest priority to lowest):

1. **Immutable safety floor:** `FORBIDDEN_TOKENS`, `curl | sh`, `wget | sh`, `> /`, shell metacharacters and brace/variable expansions, dot-segment globs (`cat .e*`), sensitive paths, dangerous `rg`/`git` options, `kind=question`, single-key replies only, and never "always allow" / "auto mode". Nothing below can override the floor.
2. **`deny_commands` / `deny_paths`:** beat every allow rule, including built-ins and LLM replies.
3. **Fixed rules:** built-in read-only (`builtin_read_only`), `allow_commands`, and `allow_tools`.
4. **LLM:** subject to steps 1, 2, and `llm_auto_reply`. `AlertOnly`, `Paused`, and `max_auto_replies` still apply.

> **Important:**
> - **An allowed command really runs.** Approving a command via `allow_commands` sends the approval key to the agent and runs whatever that command executes (for example, `just test` runs whatever the `justfile` says). `[guardrails]` is not a sandbox.
> - **An invalid `[guardrails]` stops all automatic replies** (both fixed rules and LLM auto-replies) until you fix it, while keeping the rest of `supervisor.toml` active. Unknown keys, wrong types, invalid `deny_*` entries, broken TOML, or an unreadable file mark `[guardrails]` invalid and fail `tchelvis doctor`. Invalid `allow_*` entries are ignored with a warning.

Dry-run and inspect rules locally (never sends anything to any terminal):

```bash
tchelvis supervisor rules                     # effective rules, origin, warnings, and status
tchelvis supervisor rules --json
tchelvis supervisor rules test "just test"    # exit 0 = approved, 1 = not approved, 2 = usage error
tchelvis supervisor rules test "src/main.rs" --tool Read
tchelvis supervisor rules test --screen screen.txt --msg "approve Bash: just test" --kind permission
```

`rules test` covers the fixed rules only: `requires_llm` means no fixed rule approves the request, and the LLM never answers permission requests. `deny_*` entries match text, so they cannot see through globs, scripts or variables resolved at run time.

| Environment variable | Description | Default |
| :--- | :--- | :--- |
| `TCHELVIS_LLM_CONTAINER` | Container mode: `auto`, `on`, `off` (also `1` / `0`) | `auto` |
| `TCHELVIS_CONTAINER_RUNTIME` | Force `kiri`, `podman`, or `docker` | auto-detect |
| `TCHELVIS_LLM_BACKEND` | Backend when the container mode is inactive: `ollama`, `llama-cpp`, `lm-studio`, `openai-compatible` | `ollama` |
| `TCHELVIS_LLM_HOST` / `OLLAMA_HOST` | Base URL for that backend | `http://localhost:11434` |
| `OLLAMA_FALLBACK_HOST` | Fallback Ollama host when the primary is unreachable | none |
| `TCHELVIS_LLM_MODEL` / `OLLAMA_MODEL` | Model identifier | `lfm3:1.7b` |
| `TCHELVIS_LLM_API_KEY` | Bearer token for `openai-compatible` (or host-matched `OPENAI_API_KEY` / `OPENROUTER_API_KEY` / `DEEPSEEK_API_KEY` / `OPENCODE_API_KEY`) | none |
| `TCHELVIS_FALLBACK_API_KEY` | Bearer token override for `[fallback]` (else `~/.config/tchelvis/fallback_api_key`, `TCHELVIS_LLM_API_KEY`, or host-matched provider var) | none |
| `TCHELVIS_ALLOW_GPU` | Allow GPU offload; overrides `tchelvis supervisor gpu` | `false` |
| `OLLAMA_MODELS` | Extra Ollama model store searched for GGUF blobs | none |
| `HC_BIN` | Path to the `hc` binary | `hc` |

### 6. Alternative LLM backends

Set `TCHELVIS_LLM_CONTAINER=off` (or `[llm_container] mode = "off"`) to use an HTTP backend you manage yourself:

| Backend | `llm_backend` | Typical `ollama_host` | Notes |
| :--- | :--- | :--- | :--- |
| **Ollama** | `ollama` | `http://localhost:11434` | `ollama pull lfm3:1.7b`; CPU-only by default via `num_gpu: 0`, unloaded with `keep_alive: 0` after 180 s idle. |
| **llama.cpp (self-managed)** | `llama-cpp` | `http://localhost:8080` | `llama-server -m model.gguf --port 8080 -ngl 0` |
| **LM Studio** | `lm-studio` | `http://localhost:1234` | Start the Local Server with an instruction model that supports structured JSON. |
| **OpenAI-compatible / cloud** | `openai-compatible` | e.g. `https://openrouter.ai/api/v1` | Works with OpenRouter, OpenAI, Groq, Gemini (OpenAI endpoint), DeepSeek. Export `TCHELVIS_LLM_API_KEY`; keys are never written to disk unless you set `api_key` yourself. |

Verify the connection with `tchelvis supervisor status --json` or `tchelvis doctor`.

### 7. Optional provider integrations (`~/.config/tchelvis/config.toml`)

Local git worktrees, `RUNBOOK.md` files, and authenticated `gh` / `glab` CLIs work out of the box. To enable more read-only integrations, export credentials in your environment. Plaintext tokens are never stored on disk.

| Provider | Configuration |
| :--- | :--- |
| **GitHub** | Uses `gh auth status`, or `export GITHUB_TOKEN=...` |
| **GitLab** | Uses `glab auth status`, or `export GITLAB_TOKEN=...` |
| **Jira** | `[jira] base_url = "https://your-org.atlassian.net"` in `config.toml` and `export TCHELVIS_JIRA_TOKEN=...` |
| **Linear** | `export TCHELVIS_LINEAR_TOKEN=...` |
| **Slack** | `export TCHELVIS_SLACK_TOKEN=...` |

### 8. Uninstall

```bash
# Roll back hooks, shell blocks, and Herdr Desktop mod files (keeps cache/config)
tchelvis uninstall

# Full removal: also deletes ~/.config/tchelvis, ~/.local/share/tchelvis,
# ~/.local/state/tchelvis, and the tchelvis binary itself
tchelvis uninstall --purge && hash -r

# Remove the supervisor LLM container and its download cache
docker rm -f tchelvis-supervisor-llm && rm -rf ~/.cache/tchelvis/llama.cpp
```

---

## License

Dual-licensed under either:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.

Icons in `assets/herdr-app/mods/tchelvis/icons/` are from [Lucide](https://lucide.dev) (ISC License; see [LICENSE-lucide.txt](assets/herdr-app/mods/tchelvis/icons/LICENSE-lucide.txt)).
