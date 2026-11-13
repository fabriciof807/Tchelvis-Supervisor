# Handoff — cockpit (Unified Cockpit + Durable Supervisor)

## Current State (Phase 2 Integration Complete)
`cockpit` now unifies the **Cockpit** Fleet Attention Queue & Context Correlator with the **Pi-Durable FSM Supervisor** in a single Rust crate, CLI, Ratatui TUI, and Herdr Desktop Mod.

### Key Integrated Components
- **`src/supervisor/`**: Deterministic watcher, Ollama client (`num_gpu: 0` CPU-only default + 180s idle `keep_alive: 0` unload), guardrails, durable state store (`~/.local/state/cockpit/supervisor_state.json`), and Ratatui TUI (`cockpit tui`).
- **CLI (`src/main.rs`)**:
  - `cockpit tui`: Launches the interactive Ratatui Cockpit & Durable Supervisor TUI.
  - `cockpit supervisor <status|tick|approve|ignore|mode|unload|watch>`: Exposes `DesktopModSnapshot` / `OnceOutput` JSON on stdout for Herdr Desktop Mod and terminal workflows.
  - `VALID_LABEL_KEYS` extended with `cockpit.supervisor_state` and `cockpit.supervisor_decision`.
- **Herdr Desktop Mod & Layout (`assets/herdr-app/`)**:
  - `layouts/cockpit.json`: Root `column` layout placing `cockpit-pill` (Top-Bar Pill) above `workspace-row` (`spaces`, `cockpit-fleet`, `main` terminal, `cockpit-inspector`, `status`).
  - `mods/cockpit/main.js`: Renders the Top-Bar Pill, Fleet Queue, Inspector Action Cards, and Auto-Reply Audit Log.
- **Setup & Doctor (`src/setup.rs`, `src/doctor.rs`)**:
  - `cockpit setup --set-default-layout` installs hooks, unified `cockpit` mod/layout, sets default layout in `settings.json` / `shared.json`, and cleans up legacy standalone `herdr-supervisor` mods/layouts (`~/.config/herdr-app/mods/supervisor*`, `layouts/supervisor.json`).
  - `cockpit doctor` validates the `cockpit-pill` root column layout schema and reports the `supervisor` check (`ollama_host`, `model_name`, `num_gpu=0 (CPU-only)`, `mode`).
