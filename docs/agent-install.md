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

> **Binary name:** the binary is currently `cockpit`. It will be renamed to `tchelvis` in a future
> release (see [ROADMAP.md](../ROADMAP.md), decision D1). Before running any command, check which one
> exists (`command -v tchelvis cockpit`) and use that name everywhere below.

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
   (`COCKPIT_LLM_API_KEY`), not in `supervisor.toml` or the shell rc files you edit.
5. **Linux only** (`x86_64`, `aarch64`). On other OSes, stop and tell the user it is not supported yet.
6. **Report what you verified.** Finish with the [verification commands](#step-5--verify), and report their real output.

---

## Components

| ID | Component | Needed for | Installed by |
| :-- | :-- | :-- | :-- |
| **C1** | `cockpit` CLI/TUI binary | Everything else | [Procedure A](#procedure-a--install-or-update-the-clitui-binary-c1) |
| **C2** | Integrations: shell helpers, agent hooks (Claude Code, OpenCode, Pi), Herdr Desktop mod + layout, optional systemd unit | Fleet status from agents; Herdr Desktop UI | [Procedure B](#procedure-b--install-integrations-c2) |
| **C3** | Supervisor LLM container (`llama.cpp`, `cockpit-supervisor-llm`) | Supervisor auto-replies / triage with a local model | [Procedure C](#procedure-c--set-up-the-supervisor-llm-container-c3) |
| **C4** | Alternative LLM backend (Ollama, LM Studio, OpenAI-compatible API) | Users who do **not** want the container | [Procedure D](#procedure-d--use-an-alternative-llm-backend-instead-of-c3) |
| **C5** | `ai-usagebar` (optional, third party) | The **Usage** popover / `cockpit usage` | Not installed by this project; skip unless the user asks |

External prerequisites (never installed by this guide):
- **Herdr** with the `hc` CLI `0.1.20+` in `PATH`, plus `herdr-app` for the desktop UI.
- **Rust toolchain** (`cargo` 1.75+) to build C1, until release packages exist.
- **A container runtime** for C3: Kiri, Podman, or Docker (auto-detected in that order).

---

## Step 0 — Detect what is already installed

Run these commands and keep the results:

```bash
uname -sm                                   # must be Linux x86_64 / aarch64
command -v tchelvis cockpit; which -a cockpit 2>/dev/null
cockpit --version 2>/dev/null               # e.g. "cockpit 0.1.0"
cockpit supervisor --help 2>/dev/null | grep -qw load && echo "C3-capable binary" || echo "binary too old for C3"
command -v hc && hc --version
command -v kiri podman docker               # container runtime for C3
command -v cargo && cargo --version         # needed only to build C1
cockpit doctor --json 2>/dev/null           # checks: hc_runtime, hooks, herdr_mod, storage_ipc, supervisor
cockpit supervisor status --json 2>/dev/null   # LLM state (see fields below)
ls ~/.config/cockpit/supervisor.toml 2>/dev/null
```

How to read the results:

| Signal | Meaning |
| :-- | :-- |
| `cockpit --version` works **and** `supervisor --help` lists `load` | C1 is installed and recent enough for C3 |
| `cockpit` exists but has no `load` subcommand | C1 is outdated → [Procedure A](#procedure-a--install-or-update-the-clitui-binary-c1) (update) |
| `which -a cockpit` shows more than one path | An older copy may shadow the new one (commonly `~/.cargo/bin/cockpit`). Tell the user. |
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

There are no release binaries yet, so build from source:

```bash
# Fresh clone (or `git -C <existing-clone> pull` to update)
git clone https://github.com/fabriciof807/Tchelvis-Supervisor.git
cd Tchelvis-Supervisor
cargo build --release
mkdir -p ~/.local/bin
install -m 0755 target/release/cockpit ~/.local/bin/cockpit
```

Then:
- Make sure `~/.local/bin` is in `PATH`.
- If `which -a cockpit` lists another copy first, ask the user whether to replace or remove it.
- After an update, restart any running daemons so they pick up the new binary:
  `pkill -f "cockpit daemon"; pkill -f "cockpit supervisor watch"`.

## Procedure B — Install integrations (C2)

Valid targets: `shell`, `claude`, `opencode`, `pi`, `herdr-mod`, `systemd`. Without `--only`, every
target except `systemd` is applied. Agent targets are skipped when that agent's config directory does
not exist, unless you pass that target alone with `--only`.

```bash
cockpit setup --dry-run                        # 1. preview; show the plan to the user
cockpit setup --set-default-layout             # 2. apply everything (after confirmation)
# Partial examples:
cockpit setup --only herdr-mod --set-default-layout
cockpit setup --only claude
cockpit setup --skip shell
cockpit setup --verify                         # check installed asset hashes without changing files
```

`setup` records every change in a manifest, so `cockpit setup --undo` rolls it back. Restart Herdr
Desktop after installing `herdr-mod`. The UI needs the collector running: `cockpit daemon` (or
`--systemd`, which writes a user unit).

| Target | Files touched |
| :-- | :-- |
| `shell` | `~/.config/cockpit/hooks/shell.{sh,fish,nu}`, plus a source block in `~/.bashrc`, `~/.zshrc`, `~/.config/fish/config.fish`, `~/.config/nushell/config.nu` |
| `claude` | `~/.claude/settings.json` (hooks) |
| `opencode` | `~/.config/opencode/plugins/cockpit.js` |
| `pi` | `~/.pi/agent/extensions/cockpit.js` |
| `herdr-mod` | `~/.config/herdr-app/mods/cockpit/`, `~/.config/herdr-app/layouts/cockpit.json`; with `--set-default-layout`, also `~/.config/herdr-app/settings.json` |
| `systemd` | `~/.config/systemd/user/cockpit.service` |

## Procedure C — Set up the supervisor LLM container (C3)

Requires C1 with the `load` subcommand and one of `kiri`, `podman`, or `docker` in `PATH`. The
container is created on first load; you never run `docker run` yourself.

1. **Pick the model source.** The first one that exists wins:
   1. `[llm_container] model_path`: a GGUF file on the host.
   2. `[llm_container] hf_repo`: a Hugging Face GGUF repo (e.g. `owner/repo-GGUF:Q4_K_M`), downloaded once into `~/.cache/cockpit/llama.cpp`.
   3. An existing Ollama download of the configured model (default `lfm3:1.7b`), found in `$OLLAMA_MODELS`, `~/.ollama/models`, or `/usr/share/ollama/.ollama/models`.

   **If the user already ran `ollama pull lfm3:1.7b`, skip to step 3.** Otherwise ask the user which
   GGUF file or Hugging Face repo to use. Do not guess a model on their behalf.
2. **Write the config only when you need `model_path` or `hf_repo`.**
   - Current limitation: `~/.config/cockpit/supervisor.toml` is parsed as a whole. If any top-level key is missing, the entire file is silently ignored.
   - If the file exists, edit only the `[llm_container]` table and keep everything else.
   - If it does not exist, copy the **complete** template from the README ([Configuration reference](../README.md#5-configuration-reference)) and set `model_path` or `hf_repo` there.
3. **(Optional) Pre-pull the image** so the first load is quick:
   `docker pull ghcr.io/ggml-org/llama.cpp:server` (`podman pull …`; GPU users: `:server-cuda`).
4. **Load and confirm:**
   ```bash
   cockpit supervisor load            # create/start + wait for /health (first run may take minutes)
   cockpit supervisor status --json   # expect llm_container_state "running", llm_connected true
   ```
5. **Decide the final state with the user.** The LLM is meant to be off until needed:
   `cockpit supervisor unload` stops the container (0 B RAM/VRAM). The user can start it again from the
   Herdr top bar (**Load LLM**) or with `cockpit supervisor load`.

Useful switches:
- **GPU offload:** `cockpit supervisor gpu on|off`. Requires the NVIDIA container toolkit (Docker) or CDI (Podman).
- **Force a runtime:** `COCKPIT_CONTAINER_RUNTIME=podman`.
- **Disable container mode:** `COCKPIT_LLM_CONTAINER=off`.
- **Port:** the default `11435` can be changed with `[llm_container] port`.

## Procedure D — Use an alternative LLM backend instead of C3

Set container mode off and point to the backend the user already runs. Use the table in the README:
[Alternative LLM backends](../README.md#6-alternative-llm-backends). Example (Ollama):

```bash
export COCKPIT_LLM_CONTAINER=off        # or [llm_container] mode = "off" in supervisor.toml
ollama pull lfm3:1.7b
cockpit supervisor status --json        # expect llm_managed false, llm_connected true
```

For cloud APIs, the key goes in the user's environment (`COCKPIT_LLM_API_KEY`), never on disk.

---

## Step 5 — Verify

```bash
cockpit --version
cockpit doctor                      # every check "ok", except "supervisor" may warn while the LLM is unloaded
cockpit supervisor status --json    # LLM fields match the chosen path
cockpit status                      # fleet view (needs Herdr / hc running)
```

Report the actual output to the user, including any `warn` lines and their details.

## Troubleshooting

| Symptom | Fix |
| :-- | :-- |
| `Modelo GGUF não encontrado` on `load` | No model source found → set `model_path` or `hf_repo` (Procedure C, steps 1–2) |
| `Nenhum runtime de container encontrado` | Install or enable Kiri, Podman, or Docker, or use Procedure D |
| `load` times out | The first image pull or model download can be slow. Rerun `load`, check `docker logs cockpit-supervisor-llm` |
| Port `11435` already in use | Set `[llm_container] port` to a free port |
| `supervisor.toml` changes have no effect | The file is missing a top-level key and was ignored (Procedure C, step 2) |
| UI still shows old behavior after an update | An old binary shadows the new one (`which -a cockpit`), or Herdr Desktop / daemons were not restarted |

## Uninstall

```bash
cockpit uninstall --dry-run                 # preview
cockpit uninstall                           # roll back hooks, shell blocks, and the Herdr mod (keeps config/cache)
cockpit uninstall --purge && hash -r        # also delete cockpit dirs and the binary
docker rm -f cockpit-supervisor-llm         # remove the LLM container (podman/kiri: same syntax)
rm -rf ~/.cache/cockpit/llama.cpp           # remove downloaded models (ask first)
```
