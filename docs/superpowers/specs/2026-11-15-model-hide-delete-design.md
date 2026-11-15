# Hide and delete supervisor models — design

**Date:** 2026-11-15 · **Status:** approved (approach A) · **Size:** small change

## Intent

The user wants both a cleaner model list and disk space back. Each model in the supervisor
model list gets two actions: **Hide** (leaves the list, stays on disk) and **Delete** (removed
from the machine, only after explicit confirmation).

## Goals / non-goals / anti-goals

- Goals: hide/unhide and delete from the CLI and from the mod's "Supervisor model" card.
- Non-goals: a models screen in the Ratatui TUI (`tchelvis tui`); downloading models.
- Anti-goals: deleting anything without a confirmation that shows path and size; deleting the
  model the supervisor uses or has loaded; touching Ollama's blob store directly; leaving a
  half-deleted HF cache entry.

## Approaches

- **A — CLI + mod card (chosen).** New `model hide|unhide|rm` subcommands; the card calls them.
  Smallest surface, reuses the existing list/set plumbing.
- B — A plus a TUI models screen. Same outcome, much more code for an interface the user does
  not use for models.
- C — Delete only, no hide. Simpler, but a model another app needs (e.g. in Ollama) would stay
  in the list forever or have to be deleted.

## Design

**Hide** — persisted in `supervisor.toml` as `[llm_container] hidden_models = ["<id>", …]`
(written with `config_write`, like `model_filter`).
- `tchelvis supervisor model hide <id>` / `unhide <id>`.
- `model list` omits hidden ids; `--all` includes them. Each row carries `hidden: bool`, and
  the JSON output gains `user_hidden_count` (separate from the filter's `hidden_count`).
- Hiding the selected model is refused (it would vanish while in use).

**Delete** — `tchelvis supervisor model rm <id> [--yes] [--json]`.
- Prints source, path(s) and size, then asks `y/N`; `--yes` skips the prompt (the mod uses it
  after its own confirmation). Non-interactive stdin without `--yes` refuses.
- Refused when the id is the effective model, `fallback_model_id`, or the model the running
  container was started with (`tchelvis.model_id` label).
- Per source:
  - **Ollama** (`ollama:<name>`): `ollama rm <name>` if the CLI exists, else the Ollama HTTP
    API `DELETE /api/delete` on the configured Ollama URL. The confirmation warns that the
    model also leaves Ollama for every other app. Blobs are never deleted by hand.
  - **HF cache / llama.cpp cache in HF layout** (path under `…/models--org--repo/snapshots/`):
    delete the snapshot symlink(s) of the model (all shards), then each target blob that no
    other snapshot symlink in the same repo still points to. If the repo has no snapshot files
    left, remove the whole `models--…` directory.
  - **Flat files** (llama.cpp cache root, LM Studio, configured folders): delete the `.gguf`
    file and its sibling shards. Never delete directories outside the HF layout.
- On success, the id is also removed from `hidden_models`.

**Mod card** — each row gets `Hide` (or `Show` when listed via "show hidden") and `Delete`.
Delete turns into a second button `Delete 1.1 GB?` for a few seconds; the second click runs
`model rm <id> --yes --json` and refreshes the list. Errors appear in the card. The in-use row
has no Delete/Hide.

## Left to the builder

Exact placement and styling of the row buttons; error wording; how the shard set is computed
(reuse the scanner's shard logic).

## Follow-up (separate, not in this change)

Decision-quality benchmark: a fixed set of real terminal screens with the expected decision,
scored per model, plus a per-model "reasoning on/off" switch. Tracked in `ROADMAP.md`.
