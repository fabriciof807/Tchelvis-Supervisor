# Agent Install Guide — Tchelvis Supervisor

This page is written for **AI coding agents** (Claude Code, Codex, OpenCode, Pi, Cursor, etc.) that were
given this repository's URL and asked to install, update, or partially install Tchelvis Supervisor on
the user's machine. Human-oriented documentation lives in the [README](../README.md).

- Repository: `https://github.com/fabriciof807/Tchelvis-Supervisor`
- Raw files (fetch these directly instead of rendering GitHub pages):
  - This guide: `https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/docs/agent-install.md`
  - README (full reference): `https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/README.md`
  - Changelog: `https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/CHANGELOG.md`
  - Index for LLMs: `https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/llms.txt`

---

## Rules for agents

1. **Detect before installing.** Run [Step 0](#step-0--detect-what-is-already-installed) and install only
   the missing components the user asked for. Never reinstall or overwrite a working component without
   being asked.
2. **Preview, then confirm, before changing user files.** `setup` edits shell rc files
   (`~/.bashrc`, `~/.zshrc`, fish, nushell), agent configs (`~/.claude/settings.json`, OpenCode and Pi plugins), and
   Herdr Desktop files. Run it with `--dry-run` first, show the user the planned changes, and continue only after
   they confirm.
3. **No `sudo` and no system packages** unless the user explicitly agrees. Everything here installs into
   the user's home directory.
4. **Never write secrets to disk.** API keys for cloud LLM backends belong in the user's environment
   (`TCHELVIS_LLM_API_KEY`), not in `supervisor.toml` or the shell rc files you edit.
5. **Linux only** (`x86_64`, `aarch64`). On other OSes, stop and tell the user it is not supported yet.
6. **Report what you verified.** Finish with the [verification commands](#step-5--verify), and report their real output.

---

## Components

| ID | Component | Needed for | Installed by |
| :-- | :-- | :-- | :-- |
| **C1** | `tchelvis` CLI/TUI binary | Everything else | [Procedure A](#procedure-a--install-or-update-the-clitui-binary-c1) |
| **C2** | Integrations: shell helpers, agent hooks (Claude Code, OpenCode, Pi), Herdr Desktop mod + layout, optional systemd unit | Fleet status from agents; Herdr Desktop UI | [Procedure B](#procedure-b--install-integrations-c2) |
| **C3** | Supervisor LLM container (`llama.cpp`, `tchelvis-supervisor-llm`) | Supervisor auto-replies / triage with a local model | [Procedure C](#procedure-c--set-up-the-supervisor-llm-container-c3) |
| **C4** | Alternative LLM backend (Ollama, LM Studio, OpenAI-compatible API) | Users who do **not** want the container | [Procedure D](#procedure-d--use-an-alternative-llm-backend-instead-of-c3) |
| **C5** | `ai-usagebar` (optional, third party) | The **Usage** popover / `tchelvis usage` | Not installed by this project; skip unless the user asks |

External prerequisites (never installed by this guide):
- **Herdr** with the `hc` CLI `0.1.20+` in `PATH`, plus `herdr-app` for the desktop UI.
- **A container runtime** for C3: Kiri, Podman, or Docker (auto-detected in that order).
- **Rust toolchain** (`cargo` 1.75+) — only needed if building from source instead of using `install.sh`.

---

## Step 0 — Detect what is already installed

Run these commands and keep the results:

```bash
uname -sm                                   # must be Linux x86_64 / aarch64
command -v tchelvis; which -a tchelvis 2>/dev/null
tchelvis --version 2>/dev/null              # e.g. "tchelvis 0.2.0 (nightly a1b2c3d)"
tchelvis supervisor --help 2>/dev/null | grep -qw load && echo "C3-capable binary" || echo "binary too old for C3"
command -v hc && hc --version
command -v kiri podman docker               # container runtime for C3
tchelvis doctor --json 2>/dev/null          # checks: hc_runtime, hooks, herdr_mod, storage_ipc, supervisor
tchelvis supervisor status --json 2>/dev/null  # LLM state (see fields below)
ls ~/.config/tchelvis/supervisor.toml 2>/dev/null
```

How to read the results:

| Signal | Meaning |
| :-- | :-- |
| `tchelvis --version` works **and** `supervisor --help` lists `load` | C1 is installed and recent enough for C3 |
| `tchelvis` exists but has no `load` subcommand | C1 is outdated → [Procedure A](#procedure-a--install-or-update-the-clitui-binary-c1) (update) |
| `which -a tchelvis` shows more than one path | An older copy may shadow the new one (commonly `~/.cargo/bin/tchelvis`). Tell the user. |
| `doctor --json` → `hooks: ok`, `herdr_mod: ok` | C2 is installed |
| `status --json` → `llm_managed: true` | Container mode is active (a runtime was found) |
| `status --json` → `llm_container_state` = `running` / `stopped` / `missing` | C3 container state. `missing` means it was never created |
| `status --json` → `llm_managed: false`, `llm_connected: true` | An alternative backend (C4) is in use and reachable |

---

## Step 1 — Choose a path

| User situation / request | Do |
| :-- | :-- |
| Fresh install, wants everything | A → B → C → verify |
| **CLI/TUI already installed, wants only the LLM container** | (A, only if the binary is too old) → C → verify |
| CLI installed, wants only the Herdr Desktop mod/layout | B with `--only herdr-mod` |
| CLI installed, wants hooks for one agent only | B with `--only claude` (or `opencode`, `pi`, `shell`) |
| Update to the latest version | A (update) → `setup --verify`, then B if hashes differ → verify |
| Does not want any container | A → B → D |
| Remove everything | [Uninstall](#uninstall) |

---

## Procedure A — Install or update the CLI/TUI binary (C1)

Install the prebuilt binary using `install.sh` (installs to `~/.local/bin/tchelvis` and runs `tchelvis setup --set-default-layout`):

```bash
# Stable release
curl -fsSL https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/install.sh | sh

# Nightly build (from main)
curl -fsSL https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/install.sh | sh -s -- --channel nightly
```

To update an existing `tchelvis` installation:

```bash
tchelvis update                       # updates on the remembered channel (default: stable)
tchelvis update --channel nightly     # switch to or update the nightly channel
```

`tchelvis update` verifies the SHA-256 checksum, replaces the binary atomically, runs `tchelvis setup --set-default-layout`, and restarts any running `tchelvis` daemons it stopped.

Then:
- Make sure `~/.local/bin` is in `PATH`.
- If `which -a tchelvis` lists another copy first, ask the user whether to replace or remove it.

## Procedure B — Install integrations (C2)

Valid targets: `shell`, `claude`, `opencode`, `pi`, `herdr-mod`, `skills`, `systemd`. Without `--only`,
every target except `systemd` is applied. Agent targets are skipped when that agent's config directory
does not exist, unless you pass that target alone with `--only`.

```bash
tchelvis setup --dry-run                        # 1. preview; show the plan to the user
tchelvis setup --set-default-layout             # 2. apply everything (after confirmation)
# Partial examples:
tchelvis setup --only herdr-mod --set-default-layout
tchelvis setup --only claude
tchelvis setup --skip shell
tchelvis setup --verify                         # check installed asset hashes without changing files
```

`setup` records every change in a manifest, so `tchelvis setup --undo` rolls it back. Restart Herdr
Desktop after installing `herdr-mod`. The UI needs the collector running: `tchelvis daemon` (or
`--systemd`, which writes a user unit).

| Target | Files touched |
| :-- | :-- |
| `shell` | `~/.config/tchelvis/hooks/shell.{sh,fish,nu}`, plus a source block in `~/.bashrc`, `~/.zshrc`, `~/.config/fish/config.fish`, `~/.config/nushell/config.nu` |
| `claude` | `~/.claude/settings.json` (hooks) |
| `opencode` | `~/.config/opencode/plugins/tchelvis.js` |
| `pi` | `~/.pi/agent/extensions/tchelvis.js` |
| `herdr-mod` | `~/.config/herdr-app/mods/tchelvis/`, `~/.config/herdr-app/layouts/tchelvis.json`; with `--set-default-layout`, also `~/.config/herdr-app/settings.json` |
| `skills` | `<agent>/skills/orchestration-tchelvis/SKILL.md` for `claude`, `opencode`, `pi`, `agents`, `codex`, and `gemini`; OpenCode also gets `~/.config/opencode/commands/orchestration-tchelvis.md` |
| `systemd` | `~/.config/systemd/user/tchelvis.service` |

## Procedure C — Set up the supervisor LLM container (C3)

Requires C1 with the `load` subcommand and one of `kiri`, `podman`, or `docker` in `PATH`. The
container is created on first load; you never run `docker run` yourself.

1. **Pick the model source.** The first one that exists wins:
   1. `[llm_container] model_path`: a GGUF file on the host.
   2. `[llm_container] hf_repo`: a Hugging Face GGUF repo (e.g. `owner/repo-GGUF:Q4_K_M`), downloaded once into `~/.cache/tchelvis/llama.cpp`.
   3. An existing Ollama download of the configured model (default `lfm3:1.7b`), found in `$OLLAMA_MODELS`, `~/.ollama/models`, or `/usr/share/ollama/.ollama/models`.

   **If the user already ran `ollama pull lfm3:1.7b`, skip to step 3.** Otherwise ask the user which
   GGUF file or Hugging Face repo to use. Do not guess a model on their behalf.
2. **Write the config only when you need `model_path` or `hf_repo`.**
   - Partial `~/.config/tchelvis/supervisor.toml` files are supported; any omitted keys fall back to their defaults.
   - If the file exists, edit only the `[llm_container]` table and keep everything else.
   - If it does not exist, create `~/.config/tchelvis/supervisor.toml` with `[llm_container]` and set `model_path` or `hf_repo`.
3. **(Optional) Pre-pull the image** so the first load is quick:
   `docker pull ghcr.io/ggml-org/llama.cpp:server` (`podman pull …`; GPU users: `:server-cuda`).
4. **Load and confirm:**
   ```bash
   tchelvis supervisor load            # create/start + wait for /health (first run may take minutes)
   tchelvis supervisor status --json   # expect llm_container_state "running", llm_connected true
   ```
5. **Decide the final state with the user.** The LLM is meant to be off until needed:
   `tchelvis supervisor unload` stops the container (0 B RAM/VRAM). The user can start it again from the
   Herdr top bar (**Load LLM**) or with `tchelvis supervisor load`.

Useful switches:
- **GPU offload:** `tchelvis supervisor gpu on|off`. Requires the NVIDIA container toolkit (Docker) or CDI (Podman).
- **Force a runtime:** `TCHELVIS_CONTAINER_RUNTIME=podman`.
- **Disable container mode:** `TCHELVIS_LLM_CONTAINER=off`.
- **Port:** the default `11435` can be changed with `[llm_container] port`.

## Procedure D — Use an alternative LLM backend instead of C3

Set container mode off and point to the backend the user already runs. Use the table in the README:
[Alternative LLM backends](../README.md#6-alternative-llm-backends). Example (Ollama):

```bash
export TCHELVIS_LLM_CONTAINER=off        # or [llm_container] mode = "off" in supervisor.toml
ollama pull lfm3:1.7b
tchelvis supervisor status --json        # expect llm_managed false, llm_connected true
```

For cloud APIs, the key goes in the user's environment (`TCHELVIS_LLM_API_KEY`), never on disk.

---

## Step 5 — Verify

```bash
tchelvis --version
tchelvis doctor                      # every check "ok", except "supervisor" may warn while the LLM is unloaded
tchelvis supervisor status --json    # LLM fields match the chosen path
tchelvis status                      # fleet view (needs Herdr / hc running)
```

Report the actual output to the user, including any `warn` lines and their details.

## Troubleshooting

| Symptom | Fix |
| :-- | :-- |
| `Modelo GGUF não encontrado` on `load` | No model source found → set `model_path` or `hf_repo` (Procedure C, steps 1–2) |
| `Nenhum runtime de container encontrado` | Install or enable Kiri, Podman, or Docker, or use Procedure D |
| `load` times out | The first image pull or model download can be slow. Rerun `load`, check `docker logs tchelvis-supervisor-llm` |
| Port `11435` already in use | Set `[llm_container] port` to a free port |
| UI still shows old behavior after an update | An old binary shadows the new one (`which -a tchelvis`), or Herdr Desktop / daemons were not restarted |

## Uninstall

```bash
tchelvis uninstall --dry-run                 # preview
tchelvis uninstall                           # roll back hooks, shell blocks, and the Herdr mod (keeps config/cache)
tchelvis uninstall --purge && hash -r        # also delete tchelvis dirs and the binary
docker rm -f tchelvis-supervisor-llm         # remove the LLM container (podman/kiri: same syntax)
rm -rf ~/.cache/tchelvis/llama.cpp           # remove downloaded models (ask first)
```
