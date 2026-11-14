# Roadmap — Tchelvis Supervisor

Direction, milestone order, and dependencies. Behavior details and acceptance criteria for each
milestone belong in its own spec/plan when the milestone starts. Session-level status lives in
[`HANDOFF.md`](HANDOFF.md).

_Last updated: 2026-11-14_

## Dependency graph

```
M0 Container validation & config hardening
 ├─► M1 Model selection (Herdr mod) ──► M1b Fallback model
 └─► M5a Rename to `tchelvis` + release pipeline ──► M5b Packages: AUR · Homebrew (Linux)
M2 Icon system (replace emojis)
 └─► M3 Compact layout & simpler tools
      └─► M4 Inspector multi-terminal view
```

M0 → M1 and M2 → M3 → M4 are the two main tracks. They can interleave because they touch different
code: M0/M1 are mostly `src/supervisor/`; M2–M4 are mostly `assets/herdr-app/mods/cockpit/main.js`.
M1's mod picker should use M2's icons if M2 has landed. M5a can start any time after M0, in parallel
with the UI track.

## Milestones

| # | Milestone | Depends on | Status |
| :-- | :-- | :-- | :-- |
| M0 | Container validation & config hardening | — | Host validation in progress; partial TOML done (M1) |
| M1 | Model selection in the Herdr mod ([design](docs/superpowers/specs/2026-11-14-m1-model-selection-design.md)) | M0 | **Implemented**; UI to validate on host |
| M1b | Fallback model when the chosen one stops working | M1 | Planned |
| M2 | Icon system replacing emojis | — | Planned |
| M3 | Compact layout & simpler tools | M2 | Planned |
| M4 | Inspector multi-terminal view | M3 | Planned |
| M5a | Rename to `tchelvis` + release pipeline | M0 | Planned |
| M5b | Distribution: AUR, Homebrew (Linux) | M5a | Planned |

### M0 — Container validation & config hardening
Make the `llama.cpp` container path (commit `6a38eee`) reliable on the real host before building on it.
- Validate on the host:
  - `supervisor load` with the `lfm3:1.7b` GGUF resolved from the Ollama store, including whether the image supports the LFM architecture.
  - `json_schema` structured output in real evaluations.
  - GPU image and `--gpus all`.
  - Kiri/Podman, if installed.
- Accept partial config: make every `SupervisorConfig` field `#[serde(default)]`, so a `supervisor.toml` with only `[llm_container]` works. M1 needs this to persist the chosen model.
- Make the `supervisor watch` daemon notice config changes (GPU now, the model in M1). Today it reads config only at startup.

**Done when:** load/unload works on the host with the default model, and a partial TOML is honored.

### M1 — Model selection (Herdr mod)
Let the user point the container at a GGUF model already on the machine, without downloading anything
or editing TOML by hand. Design: [M1 spec](docs/superpowers/specs/2026-11-14-m1-model-selection-design.md).
- Scan the Ollama store, the HF / llama.cpp / LM Studio caches, and the user's folders. Filter by
  parameter count (default ≤ 4B) and hide embedding and projector models.
- CLI: `cockpit supervisor model list|set|filter|reset`. The mod opens a picker card in the right
  drawer from the LLM pill or the Inspector's LLM card.
- The choice is written into `supervisor.toml` (`[llm_container] model_id`, plus `ollama_model`), so
  there is a single source of truth. Switching recreates a running container.
- Phase 2 (later): paste a Hugging Face repo or link to download and serve it.

**Done when:** the user can switch models from the mod, and the choice survives restarts.

### M1b — Fallback model
Let the user choose a second model that is used automatically when the chosen one stops working.
"Stops working" has to be defined first: a failed load, a failed health check, or repeated
evaluation/JSON errors. M1 reserves the `[llm_container] fallback_model_id` key.
Design: [M1 spec](docs/superpowers/specs/2026-11-14-m1-model-selection-design.md) (non-goals).

### M2 — Icon system (replace emojis)
- One icon helper in the mod, built on the existing `svg()` primitive: a single source of icon paths,
  sized and tinted from theme tokens so it works across GPUI themes.
- Replace every emoji in the mod. Today that is ~15 distinct glyphs, e.g. 🏠 🔍 🩺 ⚡ ⚠ 🧠 🔑 🚀 📋 📁.
- TUI: switch only where the terminal font can't render the glyphs reliably. Keep plain-text
  fallbacks.

**Done when:** no emoji remains in the mod UI and the icons render in light and dark themes.

### M3 — Compact layout & simpler tools
Builds on M2, since icon buttons are what make denser layouts possible.
- Compact the `+ Agent` quick launcher and the other oversized areas, using icon buttons and tighter
  spacing.
- Simplify tools that need too many steps. **Starts with a short audit** listing the specific flows to
  simplify, agreed with the user (see D4).

**Done when:** the audited flows are simplified and the quick launcher fits in its compact form.

### M4 — Inspector multi-terminal view
Show more than one terminal at a time in the Inspector, e.g. stacked previews or a grid of the
selected and blocked agents. Depends on M3's space budget. Each extra preview costs an `hc read`, so
refresh cadence and load need design.

**Done when:** the user can watch several agents' output at once without leaving the Inspector.

### M5a — Rename to `tchelvis` + release pipeline
- **Rename (D1)**: binary/package `cockpit` → `tchelvis` across the CLI, the Herdr mod (`exec("cockpit", …)`),
  `setup`/`uninstall`, agent hooks, layout/mod ids, and docs. Migrate existing installs: move or read
  `~/.config/cockpit`, `~/.local/state/cockpit`, `~/.local/share/cockpit`, the `cockpit-supervisor-llm`
  container name, and installed hooks/mod files, and remove the old binary on `setup`. Keep `cockpit.*` hc
  labels readable during the transition.
- **Release pipeline**: SemVer from `0.1.0`, git tags, moving `CHANGELOG.md` "Unreleased" into the release section, and GitHub Actions that build and attach
  Linux `x86_64`/`aarch64` binaries with checksums (Linux only, D2).

### M5b — Distribution: AUR, Homebrew (Linux)
Users install the TUI/CLI binary through a package manager. From there, `tchelvis setup` and
`tchelvis supervisor load` set up and run the LLM container (D3).
- **AUR**: `tchelvis-bin` from release artifacts; optionally a `tchelvis` source package.
- **Homebrew (Linux)**: a tap (`fabriciof807/homebrew-tap`) with a Linux-only formula.
- **Docker image**: deferred (D3).

## Decisions

| ID | Decision | Date |
| :-- | :-- | :-- |
| D1 | Rename the binary/package from `cockpit` to **`tchelvis`** to avoid the clash with the Cockpit Project. Done in M5a, before the first public release. | 2026-11-14 |
| D2 | **Linux only** for now (`x86_64`, `aarch64`); Homebrew means Linuxbrew. macOS is out of scope. | 2026-11-14 |
| D3 | **No Docker image for the CLI for now.** Users install the TUI/CLI natively and use it to manage the `llama.cpp` LLM container. Revisit later. | 2026-11-14 |
| D4 | The list of tools to simplify comes from a short audit at the start of M3, agreed with the user. | 2026-11-14 |
