# Cockpit

A local-first, read-only dashboard, CLI, and Durable FSM Supervisor for tracking and orchestrating coding agents running through [Herdr](https://herdr.dev) (`herdr-core` / `hc 0.1.20+`).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

## Overview & Architecture

Cockpit unifies an **Attention-First Fleet Queue & Context Correlator** with a **Durable FSM Supervisor** in a single Rust binary—giving you full visibility and safe control over parallel coding agents without mutating your repositories or sending data to a cloud backend.

- **`cockpit` Unified Binary**: Rust CLI + Unix Domain Socket (UDS) Daemon (`cockpit daemon`) + Durable FSM Supervisor (`src/supervisor/`) + Ratatui TUI (`cockpit tui`) + Herdr Desktop Mod (`assets/herdr-app/`).
- **Smart Agent Session Detection**: Automatically monitors active coding agent sessions (`claude`, `opencode`, `pi`, `codex`, `aider`, `goose`, `cursor-agent`, `amp`, active `OSC 7501` agent status hooks, or `cockpit.*` labeled sessions) while ignoring plain idle shells (`zsh`, `bash`, `fish`, `sh`, `nu`, `dash`, `tmux`, `screen`) and supervisor/cockpit UI processes.
- **Zero-VRAM Durable Supervisor**: Evaluates stalled or blocked agents using a deterministic watcher + local LLM (`lfm3:1.7b` via Ollama with `num_gpu: 0` CPU-only inference by default, or `llama.cpp`, `LM Studio`, and OpenAI-compatible cloud APIs), automatically unloading Ollama models (`keep_alive: 0`) after **180s** of zero active sessions.
- **State Persistence**: Supervisor state is persisted in `~/.local/state/cockpit/supervisor_state.json` with automatic pruning of closed or plain-shell terminals.

---

## Use Cases

### Use Case 1: Multi-Agent Fleet Triage Across Git Worktrees
When running Claude Code, OpenCode, Pi, Codex, Aider, or Goose in parallel across multiple git worktrees, attention fragments quickly. Cockpit continuously tracks `hc` terminal states and `OSC 7501` hooks, pinning `blocked` agents (`auth` > `permission` > `question`) at the top of the **Fleet Queue**, rolling up blocked subagents to their parent session, and separating `Needs You`, `Active`, and `Done` sessions so you immediately know which terminal needs you next.

### Use Case 2: Zero-VRAM Autonomous Supervision with Guardrails
In `Semi-Auto (Whitelist)` mode, the Durable Supervisor can automatically reply to safe, whitelisted prompts while enforcing strict guardrails:
- **Zero-VRAM Inference**: Defaults to CPU-only Ollama (`num_gpu: 0`) so GPU VRAM stays free for your local models and workloads, and unloads automatically after 180 seconds of inactivity.
- **Interactive Action Cards**: Dangerous or non-whitelisted commands (such as `rm -rf` or `git push --force`) are never auto-sent. Instead, they are downgraded to an **Action Card** where you can inspect, edit, approve, or ignore the suggested payload.
- **Loop-Breaker & Audit Log**: Repeated auto-replies to the same agent are capped by a per-agent loop-breaker and recorded in the **Auto-Reply Audit Log** visible in both the TUI and Herdr Desktop Inspector.

### Use Case 3: Automatic PR, CI, Ticket, Slack & Runbook Correlation
Cockpit inspects each agent's git worktree, branch name, commit subject, and explicit `cockpit.*` labels to automatically link:
- **GitHub PRs / GitLab MRs**, CI check runs, reviews, and deployments
- **Jira & Linear tickets** (e.g., `ENG-142` extracted from branch names or labels, with a denylist for false positives like `UTF-8` or `SHA-256`)
- **Slack threads** and local Markdown operational runbooks (`RUNBOOK.md`) matched by frontmatter or failing CI job names—all strictly read-only.

### Use Case 4: Unified Herdr Desktop Mod + Terminal TUI
Manage your fleet from **Herdr Desktop** (`herdr-app`) using the 4-zone `cockpit` layout (Top-Bar Pill, Fleet Queue, Inspector, and Status Bar), or switch seamlessly to `cockpit tui` when working over SSH or inside containers.

---

## How to Install

### Prerequisites

- **Linux** (`x86_64` or `aarch64`)
- **[Herdr](https://herdr.dev)** (`hc` CLI `0.1.20+` in `PATH`; optional `herdr-app` for the desktop UI)
- **Rust toolchain** (`cargo` 1.75+) to build from source
- **Ollama** (or `llama.cpp`, `LM Studio`, or an OpenAI-compatible API; optional, for Durable Supervisor LLM evaluations; defaults to `http://localhost:11434` with `lfm3:1.7b`)

### Step-by-Step Build & Install from Source

```bash
# 1. Clone the repository
git clone git@github.com:fabriciof807/cockpit.git
# Or via HTTPS:
# git clone https://github.com/fabriciof807/cockpit.git
cd cockpit

# 2. Build the release binary and install to ~/.local/bin
cargo build --release
mkdir -p ~/.local/bin
install -m 0755 target/release/cockpit ~/.local/bin/cockpit

# 3. Configure shell helpers, agent hooks, and the Herdr Desktop mod/layout
cockpit setup --set-default-layout

# 4. Verify the installation and environment health
cockpit doctor
```

### Installing & Configuring Ollama (Recommended)

```bash
# Install Ollama on Linux and pull the default Cockpit supervisor model
curl -fsSL https://ollama.com/install.sh | sh
ollama pull lfm3:1.7b
```

If you run `cockpit` inside an `incus` container while Ollama runs on the host, expose Ollama on all interfaces so the container can reach it:

```bash
sudo systemctl edit ollama.service
# Add under [Service]:
# Environment="OLLAMA_HOST=0.0.0.0:11434"
sudo systemctl daemon-reload && sudo systemctl restart ollama
```

### Host / Container (`incus`) Workflow

If you build `cockpit` inside an `incus` container (for example `ai-box`) and run Herdr Desktop on the host, pull the compiled binary onto the host and run setup there:

```bash
# Pull the release binary from the incus container onto the host
incus file pull ai-box/home/ubuntu/.local/bin/cockpit ~/.local/bin/cockpit
chmod +x ~/.local/bin/cockpit

# Configure hooks and set the unified cockpit layout as default
cockpit setup --set-default-layout
cockpit doctor
```

---

## How to Use

### 1. Herdr Desktop Mod (`cockpit` layout)

After running `cockpit setup --set-default-layout`, start the background collector daemon and restart **Herdr Desktop** (`herdr-app`):

```bash
cockpit daemon
```

Select the **`cockpit`** layout in Herdr Desktop to use the 4-zone interface alongside your terminal panes:
- **Top-Bar Pill (`cockpit-pill`, 36px)** above `spaces`: Displays `Supervisor: Watching (N)` / `Idle (0 MB)`, interactive mode switcher (`Semi-Auto (Whitelist)`, `Alert Only (Read-Only)`, `Paused`), `⚡ Poll Now`, `⏏ Unload LLM`, `Inspector ▷`, and `Switch Layout`.
- **Left Sidebar (`cockpit-fleet`)**: Attention-first Fleet Queue (`Needs You` / `Active` / `Done`), `+ Agent` quick-launcher, and filter tabs.
- **Right Panel (`cockpit-inspector`)**:
  - **Action Cards**: Approve, edit the command payload, or ignore blocked/dangerous actions.
  - **Durable Supervisor & Auto-Reply Audit Log**: Live supervisor state, rolling summary, and recorded auto-replies.
  - **Output Preview (Last 25 Lines)**: Sanitized preview of the agent's latest terminal output with TUI borders and stale supervisor lines stripped.
  - **Controls & Correlation**: Focus, Peek, Pin, Interrupt, Split Shell, Send Message, `cockpit.*` labels, PR/CI/Ticket/Slack/Runbook cards, and Doctor Health summary.
- **Bottom Bar (`cockpit-status`)**: Active supervisor mode, CPU-only (`num_gpu: 0`) / VRAM badge, workspace branch, fleet summary, and provider health badges.

### 2. Interactive TUI (`cockpit tui`)

Launch the Ratatui terminal dashboard directly in any terminal:

```bash
cockpit tui
```

**Keyboard Shortcuts:**

| Key | Action |
| :--- | :--- |
| `j` / `Down`, `k` / `Up` | Select next / previous agent in the fleet |
| `m` | Cycle supervisor mode (`Semi-Auto` → `Alert Only` → `Paused`) |
| `t` or `r` | Trigger an immediate supervisor poll tick |
| `u` | Unload the Ollama LLM model (`keep_alive: 0`) |
| `a` | Approve and send the selected Action Card payload |
| `e` | Edit the selected Action Card command payload (`Enter` to save, `Esc` to cancel) |
| `i` | Ignore the selected Action Card |
| `l` | Toggle the Auto-Reply Audit Log view |
| `q` | Quit the TUI |

### 3. CLI Commands

```bash
# View the live coding agent fleet in your terminal
cockpit status

# Stream live updates as agents change state
cockpit watch

# Inspect full PR, CI, ticket, Slack, and runbook details for a terminal (e.g., t1)
cockpit inspect t1

# Manually pin a ticket, PR, or Slack thread to a terminal
cockpit label t1 issue=ENG-142 pr=88

# Start the UDS background collector daemon for Herdr Desktop
cockpit daemon

# Durable Supervisor CLI commands
cockpit supervisor status --json
cockpit supervisor tick --json
cockpit supervisor approve <agent_id> [payload...]
cockpit supervisor ignore <agent_id>
cockpit supervisor mode <semi-auto|alert-only|paused|cycle>
cockpit supervisor unload
cockpit supervisor watch
```

### 4. Configuration & LLM Backends

Cockpit supports **Ollama** (default), **`llama.cpp` (`llama-server`)**, **LM Studio**, and **OpenAI-compatible Cloud APIs** via `~/.config/cockpit/supervisor.toml` or environment variables:

- **Environment Variables**:
  - `COCKPIT_LLM_BACKEND`: `ollama` (default), `llama-cpp`, `lm-studio`, or `openai-compatible`
  - `COCKPIT_LLM_HOST` / `OLLAMA_HOST` (default: `http://localhost:11434`, or backend default)
  - `OLLAMA_FALLBACK_HOST` (Ollama fallback endpoint)
  - `COCKPIT_LLM_MODEL` / `OLLAMA_MODEL` (default: `lfm3:1.7b`)
  - `COCKPIT_LLM_API_KEY` (or `OPENAI_API_KEY` / `OPENROUTER_API_KEY`)
  - `COCKPIT_ALLOW_GPU` (default: `false`, enforcing `num_gpu: 0` on Ollama)
  - `HC_BIN` (override path to `hc` binary)

- **Backend Examples (`~/.config/cockpit/supervisor.toml`)**:

  1. **Ollama (Default, Local Zero-VRAM)**:
     ```toml
     llm_backend = "ollama"
     ollama_host = "http://localhost:11434"
     ollama_model = "lfm3:1.7b"
     allow_gpu = false
     ```

  2. **`llama.cpp` (`llama-server`)**:
     ```toml
     llm_backend = "llama-cpp"
     ollama_host = "http://localhost:8080"
     ollama_model = "lfm3:1.7b"
     ```

  3. **LM Studio**:
     ```toml
     llm_backend = "lm-studio"
     ollama_host = "http://localhost:1234"
     ollama_model = "lfm3:1.7b"
     ```

  4. **External / Cloud API (OpenAI, OpenRouter, Groq, DeepSeek, Gemini)**:
     ```toml
     llm_backend = "openai-compatible"
     ollama_host = "https://openrouter.ai/api/v1" # or "https://api.openai.com"
     ollama_model = "openai/gpt-4o-mini"
     ```
     ```bash
     export COCKPIT_LLM_API_KEY="sk-..."
     ```

- **Provider Integrations (`~/.config/cockpit/config.toml`)**:
  Cockpit works out of the box with local git worktrees, local `RUNBOOK.md` files, and authenticated `gh` / `glab` CLIs. To enable additional read-only integrations, export credentials in your environment (Cockpit never stores plaintext tokens on disk):

  | Provider | Configuration |
  | :--- | :--- |
  | **GitHub** | Uses `gh auth status` automatically, or `export GITHUB_TOKEN=...` |
  | **GitLab** | Uses `glab auth status` automatically, or `export GITLAB_TOKEN=...` |
  | **Jira** | Add `[jira] base_url = "https://your-org.atlassian.net"` in `~/.config/cockpit/config.toml` and `export COCKPIT_JIRA_TOKEN=...` |
  | **Linear** | `export COCKPIT_LINEAR_TOKEN=...` |
  | **Slack** | `export COCKPIT_SLACK_TOKEN=...` |

### 5. Uninstall

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
