# M1 — Supervisor model selection (design)

_Date: 2026-11-14 · Roadmap: [M1](../../../ROADMAP.md) · Status: awaiting user approval_

## Intent

Different users want different supervisor models, and many already have one on their machine (pulled
with Ollama, downloaded by LM Studio, cached by Hugging Face / llama.cpp, or a loose `.gguf` file).
They should be able to point the managed `llama.cpp` container at one of those models from the
Herdr Desktop mod, **without downloading anything new and without editing TOML**.

This milestone covers **phase 1: pick a model already on the machine**. Phase 2 (paste a Hugging Face
link/repo and let the container download and serve it) comes later. The container already supports
`-hf`, so the selection format must make room for it now.

## Goals
- List the GGUF chat models already on the machine, from well-known stores and user-configured folders.
- Filter the list so a machine full of test models does not flood the picker. The filter is by
  **parameter count**, default **≤ 4B**.
- Choose a model from a picker card in the Herdr mod. The choice persists across restarts.
- Switching while the LLM is loaded restarts the container with the new model. Switching while it is
  stopped only saves the choice for the next **Load LLM**.
- The same operations are available from the CLI. The mod calls the CLI.

## Non-goals (this milestone)
- Downloading models (phase 2).
- A model picker in `cockpit tui`. The TUI stays as it is.
- A UI for adding or removing model folders. Folders are config/env only for now.
- Non-GGUF formats (safetensors, MLX, ONNX).
- Choosing models for the non-container backends (Ollama, LM Studio, or cloud with
  `COCKPIT_LLM_CONTAINER=off`). In that mode the picker shows a short note instead of the list.
- Ranking or benchmarking model quality for the supervisor's JSON decisions.
- **Fallback model** (user's idea): a second model used automatically when the chosen one stops
  working. It is deferred to a follow-up (roadmap M1b), because "stops working" needs its own definition
  (a failed load, a failed health check, repeated evaluation or JSON errors). M1 only reserves the key name
  `[llm_container] fallback_model_id` (same id format) and does not read it.

## Anti-goals (failure even if it "works")
- The picker is flooded with test, embedding, or projector models.
- Opening the picker feels slow (target: list in well under 1 s on a typical machine).
- The model currently in use disappears from the list because of the filter.
- Scanning writes, moves, or locks anything in the user's model stores. Scanning is strictly read-only.
- A model that cannot be identified silently breaks the list. It must appear with `?` details instead.

## Constraints
- Existing architecture:
  - The mod shells out to `cockpit` (`exec("cockpit", […])`) and parses JSON.
  - The supervisor state lives in `~/.local/state/cockpit/`.
  - The container lifecycle is in `src/supervisor/container.rs`.
- **One place for the model name** (user's request): the picker writes its choice into
  `supervisor.toml` itself, not into a separate state file. That way the config never shows one
  model while the picker shows another (§3).
- Writes to `supervisor.toml` must preserve the user's comments and formatting. Use `toml_edit` for this:
  it is already in the dependency tree through `toml` 0.8, so it adds no new crate to the build.
- No new crates unless unavoidable. The GGUF header reader is hand-written.
- Linux only. Deterministic tests: fixtures in temp dirs, synthetic GGUF bytes, and fake container CLI.
- UI strings follow the mod's existing language. Emojis will be replaced in M2, so use plain glyphs
  consistent with the current top bar.

## Approaches considered
| Approach | Trade-off | Decision |
| :-- | :-- | :-- |
| **A. On-demand scan via CLI.** The mod runs `cockpit supervisor model list --json` when the picker opens and on Rescan. | Simplest. Same pattern as every other mod action. Rescans on every open, but it only reads manifests and GGUF headers. | **Chosen** |
| B. Cached index file. A scan writes `models.json`, and the picker reads it. | Instant open, but adds a cache to keep coherent (staleness, deleted files). Worth it only if A is slow; the switch is invisible to the mod. | Fallback |
| C. Include the list in the supervisor snapshot. | Scanning the disk every tick (12 s) is wasteful. | Rejected |

## Design

### 1. Model discovery (`src/supervisor/models.rs`, new)

The scan is pure and read-only. It returns `Vec<LocalModel>`.

**Sources** (each root is skipped if it does not exist; roots are injectable for tests):

| Source label | Where | How |
| :-- | :-- | :-- |
| `Ollama` | `$OLLAMA_MODELS`, `~/.ollama/models`, `/usr/share/ollama/.ollama/models` (same roots as today's `default_ollama_roots`) | Walk `manifests/<registry>/<namespace>/<name>/<tag>`. For each manifest, take the `application/vnd.ollama.image.model` layer → `blobs/sha256-<hex>`. The config layer (`application/vnd.docker.container.image.v1+json`) gives `model_type` (e.g. `"1.7B"`), `file_type`, and `model_family`. Display name: `registry.ollama.ai/library/<name>/<tag>` → `<name>:<tag>`; other registries → `<registry>/<namespace>/<name>:<tag>`. |
| `HF cache` | `~/.cache/huggingface/hub`, `$HF_HOME/hub` | `models--<org>--<repo>/snapshots/<rev>/**/*.gguf`. Display name = file stem; keep `<org>/<repo>` as a secondary label. |
| `llama.cpp cache` | `$LLAMA_CACHE`, `~/.cache/llama.cpp`, `~/.cache/cockpit/llama.cpp` (the container's own cache) | Flat `*.gguf` files, **and** HF-hub layout (the cockpit cache uses `models--*/snapshots/...`, verified in the smoke test of `6a38eee`). |
| `LM Studio` | `~/.lmstudio/models`, `~/.cache/lm-studio/models` | `<publisher>/<repo>/*.gguf` |
| `Folder` | `[llm_container] model_dirs = ["~/models", …]` and env `COCKPIT_MODEL_DIRS` (colon-separated), env first | Recursive `*.gguf`, **max depth 4**, skip hidden directories, do not follow symlinked directories, stop at 2,000 files per root. |

**Deduplication:** by canonicalized file path. An HF snapshot entry is a symlink to a blob, so
canonicalize before comparing. When two sources see the same file, keep the more specific source
(Ollama > LM Studio > HF cache > llama.cpp cache > Folder).

**Split GGUF** (`*-00001-of-0000N.gguf`): list only shard 1. Size = sum of all shards. Parameters
come from the `general.size_label` metadata key if present, else unknown. (Builder may improve this.)

### 2. GGUF header reader (`src/supervisor/gguf.rs`, new)

A minimal reader for GGUF v2/v3:
- Reads the magic `GGUF`, the version, the tensor count, and the KV count.
- Reads the metadata KV pairs (all value types, including arrays). String/array values that are not
  needed are skipped by length; they are never loaded.
- Reads the tensor infos (name, n_dims, dims, type, offset) **without reading tensor data**.

Output `GgufInfo`: `architecture`, `name`, `size_label`, `file_type`, `parameter_count`
(= Σ product(dims) over tensor infos), plus `kind: Chat | Embedding | Projector`.
- **Projector:** `general.architecture == "clip"`, or `general.type == "mmproj"`, or the filename contains `mmproj`.
- **Embedding:** the architecture is in an embedding set (`bert`, `nomic-bert`, `jina-bert-v2`,
  `t5encoder`, …; builder may extend it), or `<arch>.pooling_type` is present with a non-none value.
- **Safety limits:** abort on string length > 1 MiB, array length > 10M, KV count > 100k, tensor count > 1M,
  or more than 16 MiB read. On any parse error, the model is listed with unknown details
  (params `?`, kind `Chat`) instead of being dropped.

For **Ollama** entries, prefer the config's `model_type` for the parameter label and use the GGUF
header for `kind` and for exact counts when `model_type` is missing. Also treat Ollama
`model_family` values `bert` / `nomic-bert` as embeddings.

### 3. Model identity and persistence

```jsonc
// LocalModel (also the JSON row returned by `model list --json`)
{
  "id": "ollama:lfm3:1.7b",           // stable id (see below)
  "display_name": "lfm3:1.7b",
  "source": "Ollama",                  // Ollama | HF cache | llama.cpp cache | LM Studio | Folder
  "path": "/home/u/.ollama/models/blobs/sha256-…",
  "size_bytes": 1234567890,
  "params": 1700000000,                // null when unknown
  "params_label": "1.7B",              // "?" when unknown
  "quant": "Q4_K_M",                   // null when unknown
  "kind": "chat",                      // chat | embedding | projector
  "in_use": true
}
```

**Stable id:** `ollama:<display_name>` for Ollama, and `file:<canonical path>` for everything else.
Phase 2 will add `hf:<org>/<repo>[:quant]`, which maps to the existing `ModelSource::HfRepo`.

**Persisted selection: written into `supervisor.toml`** (user's request: a single place for the model name).
- **Target file:** the file `SupervisorConfig::load()` reads (`~/.config/cockpit/supervisor.toml`, or the
  legacy `~/.config/herdr/supervisor.toml` if only that one exists). If neither exists, create
  `~/.config/cockpit/supervisor.toml` containing only the keys below. This relies on M0's partial-TOML fix.
- **Edit method:** use `toml_edit`, which keeps comments, key order, and formatting. Write atomically (tmp + rename).
- **Keys written** after the user confirms the switch:
  ```toml
  ollama_model = "qwen2.5:3b"     # set to the display name for Ollama models, so the name is consistent everywhere

  [llm_container]
  model_id = "ollama:qwen2.5:3b"  # or "file:/abs/path.gguf"; phase 2 adds "hf:<org>/<repo>[:quant]"
  model_filter = "4b"             # 2b | 4b | 8b | all
  ```
- **Untouched:** `model_path` / `hf_repo` are left as they are. They are only used when `model_id` is absent (see precedence).
  `model reset` removes `model_id`.
- **Environment variables cannot be rewritten.** `COCKPIT_LLM_MODEL` / `OLLAMA_MODEL` live in the
  user's shell rc files, in systemd units, or in Herdr's process environment, and a child process
  cannot change those. Instead:
  - in container mode, `model_id` in TOML wins over them;
  - when one of them is set and differs from the effective model, `model list` returns an
    `env_override_ignored` note, and the picker shows it as a warning line, e.g.
    "`OLLAMA_MODEL=lfm3:1.7b` is set in the environment and ignored in container mode".

**Resolution precedence** in container mode, a new first step in `resolve_model_source`:
1. TOML `[llm_container] model_id`:
   - `ollama:<name>` → resolve the blob via the Ollama store.
   - `file:<path>` → `ModelSource::File(path)`.
   - **If the selected model no longer exists:** `load` fails with a clear error naming it. It does not silently fall back to a different model.
2. TOML `[llm_container] model_path`.
3. TOML `[llm_container] hf_repo`.
4. Ollama-store lookup of `ollama_model`. The env vars `COCKPIT_LLM_MODEL` / `OLLAMA_MODEL` keep
   feeding this name, as today.

_Coordinator's call:_ the env model name does **not** override `model_id`. `OLLAMA_MODEL` is often set for
Ollama itself, and the mod runs `cockpit` with Herdr's environment rather than the user's shell. If the
env won, a picker that silently does nothing would be the worse failure. The picker surfaces the ignored
variable as a warning (above). To return to `model_path` / `hf_repo` / `ollama_model` behavior, use `model reset`.

**"In use" (`in_use`, the filled radio dot, the pill name)** = the **effective model**: the one this
resolution produces, which is what the next Load runs. While the container is running, it is also
what runs, because `model set` recreates a running container immediately. After a failed health
wait, the container is stopped and the effective model is the new selection.
- **Always in the list:** if the effective model is not found by the scan (a TOML `model_path` outside
  the scanned roots, or a TOML `hf_repo`), the list gets a synthetic row for it with source `Config`. Its
  id is `file:<path>` or `hf:<repo>`.
- **`model set` accepts** any id from the fresh scan, plus the current effective id. This keeps the
  synthetic row selectable.

The snapshot's `llm_model` shows the selected `display_name` when a selection exists, so the pill
matches the picker.

### 4. Switching and the container

- New container label `cockpit.model_id=<id>` (the plain id, so the running model can be read back), alongside `cockpit.ngl`.
- `LlmContainer::start` recreates the container when **either** label differs from the requested
  value. It reuses the existing recreate path.
- **`model set` while the container is Running:** persist the selection → recreate → wait for
  `/health`, then return the snapshot. If the health wait fails, stop the container, keep the new
  selection, and return an error naming the model. The user can pick the previous model from the
  same list (it is still on disk).
- **`model set` while Stopped or Missing:** persist only. The next `load` uses it.
- Selecting the model that is already selected is a no-op.

### 5. Filtering

- `max_params` ∈ `2b | 4b | 8b | all`, default `4b`, persisted as `[llm_container] model_filter` in `supervisor.toml`.
- Thresholds: `2b` = ≤ 2.5e9, `4b` = ≤ 4.5e9, `8b` = ≤ 8.5e9. The headroom keeps a "3.8B" or "4.0B"
  model inside the 4B bucket even when the exact tensor sum lands slightly above the label.
- Embedding and projector models are always hidden from the picker.
- Models with unknown params are **shown** (`?`). They are not hidden by the size filter.
- The model in use is **always shown**, whatever the filter.
- The list response carries `hidden_count` (models removed by the filter or kind) so the picker can
  say "12 hidden". `--all` returns everything (including embeddings, marked by `kind`) for debugging.

### 6. CLI (`cockpit supervisor model …`)

| Command | Behavior |
| :-- | :-- |
| `model list [--json] [--max-params <2b\|4b\|8b\|all>] [--all]` | Scans and applies the filter. With no flags, uses the persisted filter. JSON: `{ "models": [LocalModel], "hidden_count": n, "filter": "4b", "selected_id": "…" \| null, "managed": bool, "env_override_ignored": "OLLAMA_MODEL=…" \| null }`. Human output: an aligned table. |
| `model set <id>` | Validates the id against a fresh scan (errors if not found), persists it, and switches per §4. Prints the supervisor snapshot JSON, like `load` / `gpu`. |
| `model filter <2b\|4b\|8b\|all>` | Persists the filter and prints `model list --json` output. |
| `model reset` | Removes `model_id` from `supervisor.toml` (resolution falls back to `model_path` / `hf_repo` / `ollama_model`). Prints the snapshot. |

When container mode is inactive, `list` returns `managed: false` and an empty list, and `set`
errors with a message explaining that model selection applies to the managed container.

### 7. Herdr mod: model picker card

The mod cannot draw floating overlays outside its layout blocks. The AI Usage "popover" is really the
right drawer (`cockpit-inspector`) opened in a usage-only mode. The model picker reuses that mechanism
(user's choice: option **a**).

- **Two entry points** (user's choice), both clickable:
  1. the **LLM pill** in the top bar (`○ LLM: lfm3:1.7b (DESATIVADO)`);
  2. the **LLM card in the Inspector's "Supervisor & Guardrails" section** (`lfm3:1.7b (llama-cpp · CPU)` + state badge).
- **Opening:**
  - If the drawer is hidden, it opens in a **model-only mode**: only the picker card, at the **top** of the drawer. Closing the card hides the drawer again. This mirrors `toggleUsagePopup` / `closeUsagePopup`.
  - If the drawer is already open (Home, Inspector, or Doctor), the picker card is shown at the top of the current view. Closing it leaves the drawer open.
  - Clicking an entry point while the picker is open closes it (toggle).
- **Card header:** title "Supervisor model", a segmented filter `≤2B | ≤4B | ≤8B | All` (persists via `model filter`), and `×`.
- **Rows:** a radio dot (effective model = filled), the display name, the params label, the source, the size,
  and the quant if there is room. Clicking a row runs `model set <id>` with a long timeout
  (300 s, same as Load). While switching, the row shows `◌ Switching…` and other rows ignore clicks.
- **Footer:**
  - `N hidden by filter`. Clicking it shows all models for this card session only; it does not change the persisted filter.
  - `↻ Rescan`, which re-runs `model list`.
- **States:**
  - Loading: `Scanning…`.
  - Empty: "No local GGUF models found. Add folders with `[llm_container] model_dirs` or pull a model with Ollama."
  - Container mode off: a one-line note.
  - Error: the CLI's stderr.
- The picker never starts the container on its own. Only an explicit row click on a running
  container restarts it (§4).
- No layout JSON change. `cockpit setup` is not needed to get the feature beyond updating the mod files.

## What is left to the builder
- Exact visual sizing of the picker card (follow the AI Usage card; keep the top-bar center cluster pinned, see `7a8c17b`).
- Human-readable table format of `model list`.
- Extending the embedding-architecture set, and better parameter estimates for split GGUFs.
- Whether to show `quant` in the row when space is tight.
- Internal module boundaries beyond `models.rs` / `gguf.rs`.

## Testing
- `gguf.rs`:
  - synthetic v3 headers built in tests (KV of each type, arrays, tensor infos) → correct metadata and `parameter_count`;
  - truncated or corrupt input → error, no panic;
  - limit violations → error.
- `models.rs`, with temp-dir fixtures for each source layout (Ollama manifests + config + blob, HF hub
  with snapshot symlinks, LM Studio tree, flat folder, depth limit, hidden dirs, symlink loop, split
  shards):
  - correct display names and sources;
  - deduplication;
  - kind detection;
  - filter thresholds;
  - the in-use model always shown.
- Precedence: TOML `model_id` > TOML `model_path` > `hf_repo` > Ollama lookup of `ollama_model` (env-fed).
- TOML writer (`toml_edit`):
  - comments and unrelated keys are preserved;
  - a missing file gets created;
  - the legacy path is used when it is the only file;
  - `ollama_model` is updated for Ollama selections;
  - `reset` removes `model_id`.
- Env warning: `env_override_ignored` is set when the env model differs from the effective model.
- A missing selected model gives an error. The effective model outside the scan appears as a `Config` row.
- Container: a label mismatch on `cockpit.model_id` triggers a recreate (fake `ContainerCli`).
- `node --check` on the mod. A manual smoke test with Docker: two GGUFs, switch while running, switch
  while stopped.

## Dependencies
- Builds on `6a38eee` (container lifecycle).
- **M0's partial-TOML fix is required**: the picker may create or extend `supervisor.toml` with only a few keys.
