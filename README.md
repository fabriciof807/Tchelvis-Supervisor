# Cockpit

A local-first, read-only dashboard, CLI, and Durable FSM Supervisor for tracking and orchestrating coding agents running through [Herdr](https://herdr.dev) (`herdr-core` / `hc 0.1.20+`).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## Why Cockpit? (Use Case)

When running multiple coding agents in parallel (Claude Code, OpenCode, Pi, Codex, Aider, Goose, Cursor, Amp) across different git worktrees and terminals, context quickly fragments:

- **Which agent needs me right now?** One agent is waiting on an OAuth prompt, another is asking an architectural question, and three are running tests in the background.
- **What is each agent actually working on?** Which GitHub PR/GitLab MR, CI check run, Jira/Linear ticket, Slack thread, or operational runbook belongs to terminal `t7`?

**Cockpit** solves this without mutating your repositories or sending data to a cloud backend:

1. **Attention-First Fleet Queue**: Tracks live `hc` terminal states and `OSC 7501` agent hooks, pinning `blocked` agents (`auth` > `permission` > `question`) at the top of the queue and rolling up blocked subagents to their parent session.
2. **Automatic Context Correlation**: Inspects each agent's git worktree, branch, commit subject, and explicit `cockpit.*` labels to link open PRs/MRs, CI status, reviews, deployments, Jira/Linear tickets, Slack threads, and local Markdown runbooks (`RUNBOOK.md`).
3. **Durable FSM Supervisor (Zero-VRAM)**: Evaluates stuck/blocked agents with a local Ollama model (`num_gpu: 0` CPU-only inference by default, plus automatic `keep_alive: 0` unload after 180s of zero active sessions), surfaces **Action Cards** for operator approval, and maintains an **Auto-Reply Audit Log** with a loop-breaker guardrail.
4. **Dual Interface (Herdr Desktop Mod + TUI/CLI)**: Works inside **Herdr Desktop** (`herdr-app`) with a **Top-Bar Pill** above `spaces`, Fleet Queue, Inspector, and Status bar, as well as an interactive Ratatui dashboard (`cockpit tui`) and CLI (`status`, `watch`, `inspect`, `supervisor`).

---

## Architecture Overview

Cockpit unifies the **Fleet Attention Queue & Context Correlator** with the **Durable FSM Supervisor** in a single Rust binary:

- **`cockpit` Unified Binary**: Rust CLI + Unix Domain Socket (UDS) Daemon + Durable FSM Supervisor + Ratatui TUI + Herdr Desktop Mod (`assets/herdr-app/`).
- **Smart Agent Session Detection**: Automatically monitors active coding agent sessions (`claude`, `opencode`, `pi`, `codex`, `aider`, `goose`, `cursor`, `amp`, active `OSC 7501` status/reported hooks, or `cockpit.*` labeled sessions) while ignoring plain idle shells (`zsh`, `bash`, `fish`, `sh`, `nu`, `dash`, `tmux`) and supervisor/cockpit UI processes.
- **Herdr Desktop Mod (`cockpit` layout)**:
  - **Top-Bar Pill (`cockpit-pill`, 36px)** above `spaces`: Displays `Supervisor: Watching (N)` / `Idle (0 MB)`, interactive mode switcher (`Semi-Auto (Whitelist)`, `Alert Only (Read-Only)`, `Paused`), `⚡ Poll Now`, `⏏ Unload LLM`, `Inspector ▷`, and `Switch Layout`.
  - **Left Sidebar (`cockpit-fleet`)**: Attention-first Fleet Queue (`Needs You` / `Active` / `Done`), `+ Agent` quick-launcher, and filter tabs.
  - **Right Panel (`cockpit-inspector`)**: Operator Action Cards (approve/edit payload or ignore), Durable Supervisor state & rolling summary, Auto-Reply Audit Log, sanitized scrollable Output Preview (last 20–25 lines with TUI border stripping), Agent Controls (Focus, Peek, Pin, Interrupt, Split Shell, Send Message), Correlation & Labels (`cockpit.*`), PR/CI/Ticket/Slack/Runbook cards, and Doctor Health summary.
  - **Bottom Bar (`cockpit-status`)**: Supervisor mode, CPU-only (`num_gpu: 0`) / VRAM badge, workspace branch, fleet summary, and provider health badges.

### Durable Supervisor & Zero-VRAM Ollama (`src/supervisor/`)

The integrated **Durable Supervisor** (`src/supervisor/`) monitors active `hc` sessions using a deterministic watcher + local Ollama LLM (`qwen2.5:1.5b-instruct` by default):

- **Zero-VRAM Guarantee**: Sends `num_gpu: 0` (CPU-only inference) by default so GPU VRAM is preserved for other workloads, and automatically unloads the model (`keep_alive: 0`) after **180s** with zero active sessions (or on demand via `cockpit supervisor unload`).
- **Action Cards & Guardrails**: Unsafe or non-whitelisted actions never auto-send; instead, they generate an **Action Card** in both `cockpit tui` and the Herdr Desktop Inspector for explicit operator approval (`approve` / `ignore`).
- **Auto-Reply Audit Log**: Safe whitelisted auto-replies (`semi-auto` mode) are capped by a per-agent loop-breaker and recorded in an audit log visible in the TUI and Herdr Desktop Mod.
- **Configuration**: Configurable via `~/.config/cockpit/supervisor.toml` and environment variables:
  - `OLLAMA_HOST`
  - `OLLAMA_FALLBACK_HOST`
  - `OLLAMA_MODEL`
  - `COCKPIT_ALLOW_GPU`
  - `HC_BIN`
- **State Persistence**: State is persisted in `~/.local/state/cockpit/supervisor_state.json` with automatic pruning of closed or plain-shell terminals.

---

## Installation

### Prerequisites

- **Linux** (`x86_64` or `aarch64`)
- **[Herdr](https://herdr.dev)** (`hc` CLI `0.1.20+` in `PATH`; optional `herdr-app` for the desktop UI)
- **Rust toolchain** (`cargo` 1.75+) to build from source

### Build & Install from Source

```bash
# 1. Clone the repository
git clone git@github.com:fabriciof807/cockpit.git
# Or via HTTPS:
# git clone https://github.com/fabriciof807/cockpit.git
cd cockpit

# 2. Build and install the binary to ~/.local/bin
cargo build --release
mkdir -p ~/.local/bin
install -m 0755 target/release/cockpit ~/.local/bin/cockpit

# 3. Configure shell helpers, agent hooks, and the Herdr Desktop mod/layout
cockpit setup --set-default-layout

# 4. Verify the installation and environment health
cockpit doctor
```

### Host / Container (`incus`) Workflow

If you build `cockpit` inside an `incus` container (for example `ai-box`) and run Herdr Desktop on the host, pull the compiled binary from the container and run setup on the host:

```bash
# Pull the release binary from the incus container onto the host
incus file pull ai-box/home/ubuntu/.local/bin/cockpit ~/.local/bin/cockpit
chmod +x ~/.local/bin/cockpit

# Configure hooks and set the unified cockpit layout as default
cockpit setup --set-default-layout
cockpit doctor
```

---

## Basic Usage

```bash
# View the live coding agent fleet in your terminal
cockpit status

# Stream live updates as agents change state
cockpit watch

# Inspect full PR, CI, ticket, Slack, and runbook details for a terminal (e.g., t1)
cockpit inspect t1

# Manually pin a ticket, PR, or Slack thread to a terminal
cockpit label t1 issue=ENG-142 pr=88

# Launch the interactive Ratatui Cockpit & Durable Supervisor TUI
cockpit tui

# Durable Supervisor CLI commands
cockpit supervisor status --json
cockpit supervisor tick --json
cockpit supervisor approve <agent_id> [payload...]
cockpit supervisor ignore <agent_id>
cockpit supervisor mode <semi-auto|alert-only|paused|cycle>
cockpit supervisor unload
cockpit supervisor watch
```

### Using in Herdr Desktop

After running `cockpit setup --set-default-layout`, restart **Herdr Desktop** (`herdr-app`) and start the background collector:

```bash
cockpit daemon
```

Select the **`cockpit`** layout in Herdr Desktop to view the **Top-Bar Pill** (above `spaces`), Fleet Attention Queue, Inspector (with Action Cards & Auto-Reply Audit Log), and Status bar alongside your active terminal panes.

---

## Optional Provider Integrations

Cockpit works out of the box with local git worktrees, local `RUNBOOK.md` files, and authenticated `gh` / `glab` CLIs. To enable additional read-only integrations, export credentials in your environment (Cockpit never stores plaintext tokens on disk):

| Provider | Configuration |
| :--- | :--- |
| **GitHub** | Uses `gh auth status` automatically, or `export GITHUB_TOKEN=...` |
| **GitLab** | Uses `glab auth status` automatically, or `export GITLAB_TOKEN=...` |
| **Jira** | Add `[jira] base_url = "https://your-org.atlassian.net"` in `~/.config/cockpit/config.toml` and `export COCKPIT_JIRA_TOKEN=...` |
| **Linear** | `export COCKPIT_LINEAR_TOKEN=...` |
| **Slack** | `export COCKPIT_SLACK_TOKEN=...` |

---

## Uninstall

```bash
# Roll back hooks, shell blocks, and Herdr Desktop mod files (keeps cache/config)
cockpit uninstall

# Full removal: roll back hooks/mods, delete ~/.config/cockpit, ~/.local/share/cockpit,
# ~/.local/state/cockpit, and remove the cockpit binary itself
cockpit uninstall --purge && hash -r
```

---

## License

This project is licensed under the [MIT License](LICENSE).
