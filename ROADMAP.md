# Roadmap — Tchelvis Supervisor

Direction, milestone order, and dependencies. Behavior details and acceptance criteria for each
milestone belong in its own spec/plan when the milestone starts. Session-level status lives in
[`HANDOFF.md`](HANDOFF.md).

_Last updated: 2026-11-24_

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
| M1b | Fallback model when the chosen one stops working ([design](docs/superpowers/specs/2026-11-16-m1b-fallback-model-design.md)) | M1 | **Implemented**; host validation pending |
| M1c | Hide/delete models ([design](docs/superpowers/specs/2026-11-15-model-hide-delete-design.md)) | M1 | **Implemented**; host validation pending |
| M1d | Decision-quality benchmark per model | M1 | Planned |
| M2 | Icon system replacing emojis | — | **Done** (`98b709a`); host visual check pending |
| M3 | Compact layout & simpler tools | M2 | Planned |
| M4 | Inspector multi-terminal view | M3 | Planned |
| M5a | Rename to `tchelvis` + release pipeline | M0 | **Done** — `v0.2.0` published 2026-11-16 |
| M5b | Distribution: AUR, Homebrew (Linux) | M5a | Planned |
| M6 | Delegator mode: multi-agent orchestration (`tchelvis orch`, [design](docs/designs/m6-orchestration.md)) | — | **Implemented** |
| M7 | Kanban board mod: daily tasks grouped by project tag | Daily-tasks tags | Idea (not planned) |
| M8 | Supervisor chat + Pi Durable ([design](docs/superpowers/specs/2026-11-17-supervisor-chat-design.md)) | M1b | **Phase A implemented** (`dd16bd3`, `238191a`, `6c472d3`); **Phase B: adopt Pi Durable** |
| M9 | `files-yazi` mod: Files button opens yazi in a split beside the focused terminal | — | Design agreed (not planned) |
| M10 | User-configurable supervisor `[guardrails]` ([design](docs/superpowers/specs/2026-11-22-supervisor-guardrails-config-design.md)) | M0 | **Implemented**; host validation pending |
| M11 | Declarative multi-agent loops (`tchelvis loop`, [design](docs/superpowers/specs/2026-11-23-loop-engine-design.md)) | M10 | **Phase A implemented** (host/Incus validation & Phases B–C pending) |

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

### M1c — Hide and delete models
Hide a model from the list or delete it from disk (with confirmation), from the CLI and the mod
card. Design: [spec](docs/superpowers/specs/2026-11-15-model-hide-delete-design.md).

### M1d — Decision-quality benchmark
`supervisor bench` measures speed only. Add a fixed set of real terminal screens (permission
prompt, destructive command, agent working, question) with the expected decision, score each
model on it, and add a per-model "reasoning on/off" switch. Reasoning models are viable on GPU
(~3–5 s at 88 tok/s) but slow on CPU. Lets the user pick a supervisor model by accuracy, not by
category.

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
- **Rename (D1)**: binary/package `cockpit` → `tchelvis` across the CLI, the Herdr mod (`exec("tchelvis", …)`),
  `setup`/`uninstall`, agent hooks, layout/mod ids, env prefix (`TCHELVIS_*`), and docs. No automatic
  migration from `cockpit`: users uninstall `cockpit` (`cockpit uninstall --purge`) and clean-install `tchelvis`.
  Keep `cockpit.*` hc labels readable.
- **Release pipeline**: SemVer from `0.1.0` (`0.2.0` first post-rename release), git tags, moving `CHANGELOG.md` "Unreleased" into the release section, and GitHub Actions that build and attach
  Linux `x86_64`/`aarch64` binaries with checksums (Linux only, D2), plus `install.sh` and `tchelvis update`.

### M5b — Distribution: AUR, Homebrew (Linux)
Users install the TUI/CLI binary through a package manager. From there, `tchelvis setup` and
`tchelvis supervisor load` set up and run the LLM container (D3).
- **AUR**: `tchelvis-bin` from release artifacts; optionally a `tchelvis` source package.
- **Homebrew (Linux)**: a tap (`fabriciof807/homebrew-tap`) with a Linux-only formula.
- **Docker image**: deferred (D3).

### M7 — Kanban board mod (idea, not planned)
A separate mod that reads the daily tasks (`tchelvis task list --json`) and shows them as a board,
one column per project tag (plus Geral). Tasks stay general; the tag is the only grouping key
(decision 2026-11-16, see `docs/superpowers/specs/2026-11-16-daily-tasks-input-design.md`).

### M8 — Supervisor chat + Pi Durable
Read-only chat (`tchelvis chat ask` + a Chat tab in the drawer) about active agents and Daily Tasks,
using the supervisor's effective model. Container mode only; shares the local model slot with the
supervisor through `llm_slot.lock` (evaluations defer instead of timing out).
- **Phase A (done):** answer questions only via `tchelvis chat ask`. [Design](docs/superpowers/specs/2026-11-17-supervisor-chat-design.md),
  [plan](docs/superpowers/plans/2026-11-17-supervisor-chat.md).
- **Phase B (planned — adopt Pi Durable `@earendil-works/pi-durable`):** evolve the chat into an
  action-capable, durable assistant backed by Pi Durable (optional Node/Bun worker managed by
  `tchelvis`):
  - **Why Pi Durable:** persistent conversation history across Herdr Desktop restarts (SQLite/JSONL),
    replay-safe tool execution (*effect sandwiches*) for acting on Daily Tasks, `tchelvis orch`, and
    agent prompts (with confirmation cards for small models / destructive actions), background
    context compaction (replacing fixed character budgets), multi-client steering shared across the
    Herdr mod / CLI / TUI, and parallel/forked threads per project or orchestration run.
  - **Integration constraints:** model requests route through `tchelvis daemon` (or an internal local
    proxy) so local `llama.cpp` calls still honor `llm_slot.lock` and `[fallback]` routing; Phase A's
    zero-dependency Rust path remains available for offline/lightweight 350M–1.7B setups without a
    Node/Bun runtime.

### M9 — `files-yazi` mod (design agreed 2026-11-17, not planned)
A new mod, separate from `tchelvis`, instead of replacing Herdr's Files (that button lives in
Herdr's shipped `tabs` mod; changing it would mean forking it with `mod.clone`). Both buttons stay.
- **Button:** a small block to the right of the tab bar (layout row becomes `[tabs | files-yazi]`).
- **Click:** `terminal.open` with `argv: ["yazi"]` and no `folder`, placed to the right — the split
  inherits the focused terminal's folder (Herdr behaviour). Click again closes the yazi pane of the
  current tab (toggle); quitting yazi (`q`) closes it too.
- **yazi is an optional prerequisite**, only for this button: missing → message "yazi não encontrado —
  instale para usar Files", nothing opens; `tchelvis doctor` gets a `yazi` check (WARN, never error);
  README and `install.sh` mention it as optional. `tchelvis setup` installs the mod with the rest.
- **To confirm on the host before planning** (`herdr-app commands`, the `.d.ts` beside
  `~/.config/herdr-app/AGENTS.md`): the `place` value for "right", whether a mod's `run()` returns
  the created terminal id (else find yazi by foreground program among the tab's terminals), and
  `herdr-app check` / `herdr-app shot` for the visual check.

### M10 — User-configurable supervisor `[guardrails]` (implemented; host validation pending)
A `[guardrails]` table in `~/.config/tchelvis/supervisor.toml`: one deterministic `allow_commands`
list (exact by default, trailing `*` for arguments), `deny_commands` / `deny_paths` that always win,
a closed `allow_tools` list, and `tchelvis supervisor rules [test]` for dry runs. The built-in floor
stays immutable and an invalid table turns automatic replies off.
Design: [guardrails config spec](docs/superpowers/specs/2026-11-22-supervisor-guardrails-config-design.md).

### M11 — Declarative multi-agent loops (Phase A implemented; Phases B–C planned)
`tchelvis loop`: a fixed engine runs headless worker/reviewer/critic stages from TOML workflows inside a mandatory sandbox (`bwrap` default, `inherited` inside containers, `incus` planned), exchanging files through symlink-safe `.tchelvis-loop/{in,out}` directories. The caller ends its turn and the daemon wakes it once, when the loop finishes, stalls, errors, or is cancelled. Design: [loop engine spec](docs/superpowers/specs/2026-11-23-loop-engine-design.md).
- **Operational guarantees & security notes:**
  - The loop never merges or pushes (commits only to a local `loop/<workflow>-<run-id>` branch, leaving checkout and `.gitignore` untouched).
  - `auto_approve_in_sandbox` must be turned on by the user in `~/.config/tchelvis/loops.toml`.
  - `bwrap` shares the network and localhost (plus abstract sockets), while keeping `/` read-only and binding only the cloned run workspace writable.
  - Every model is really called once before a loop starts and that spends tokens (`Reply with exactly: OK` preflight probe per distinct `provider:model` pair).
  - An exported `OPENCODE_API_KEY` is visible to every program started from that shell (`tchelvis` only passes it into `opencode` stages/probes via environment and redacts it, never writing it to disk).

### M6 — Delegator mode & multi-agent orchestration (implemented)
Event-driven parent-child agent orchestration (`tchelvis orch`) over Herdr (`hc`) terminals. Design: [M6 spec](docs/designs/m6-orchestration.md).
- **CLI (`tchelvis orch`):** `run-create`, `task-create`, `dispatch`, `send`, `ask`, `reply`, `check`, `status`, `release`, and `skill`. Workers are spawned via `hc run --bg --no-notify --outlive-parent` (or attached to existing terminals) with an auto-generated `[TCHELVIS ORCHESTRATION WORKER CONTRACT]` preamble and `--request-id` idempotency receipts.
- **Durable SQLite mailbox (`orch.db` in WAL mode):** Tracks `runs`, `tasks` (composite `PRIMARY KEY (run_id, id)` with automatic `pending -> ready` / `blocked` DAG propagation and 3-failure circuit breaking), fenced `dispatch_contexts` (`(assignee_terminal, terminal_created_ms)`), `messages`, `question_threads`, and `mutation_receipts`.
- **Atomic guarded pointer delivery (`OrchDeliveryCoordinator` in `tchelvis daemon`):** Serializes single-line pointer wakeups (`[tchelvis] You have N orchestration message(s) in run <run_id>...`) via `hc send --if idle <terminal> "<pointer>"` (non-force), coalescing concurrent child messages and retrying after 30s (`MAX_DELIVERY_ATTEMPTS = 3`).
- **Zero-LLM safety net & unloaded-LLM fallback (`OrchReconciler` in `tchelvis daemon`):** Synthesizes deterministic escalations when a worker returns to its prompt for `>= 1500ms` after `turn > turn_at_dispatch` without `worker_done` (`msg_stop_<dispatch_id>_<turn>`), exits/errors out, or remains `blocked` on a prompt for `>= 3000ms` (`msg_block_<dispatch_id>_<turn>`, giving the supervisor read-only fast path and optional loaded LLM first refusal).
- **Waiting-on-children visibility:** `FleetTracker::active_children_map` populates `active_children` on `AgentRow` and `AgentSummaryRow` so idle coordinators with live child dispatches render `"Waiting on children (N)"` in `tchelvis status` and the Herdr Desktop mod.

## Decisions

| ID | Decision | Date |
| :-- | :-- | :-- |
| D1 | Rename the binary/package from `cockpit` to **`tchelvis`** to avoid the clash with the Cockpit Project. Done in M5a, before the first public release. | 2026-11-14 |
| D2 | **Linux only** for now (`x86_64`, `aarch64`); Homebrew means Linuxbrew. macOS is out of scope. | 2026-11-14 |
| D3 | **No Docker image for the CLI for now.** Users install the TUI/CLI natively and use it to manage the `llama.cpp` LLM container. Revisit later. | 2026-11-14 |
| D4 | The list of tools to simplify comes from a short audit at the start of M3, agreed with the user. | 2026-11-14 |
