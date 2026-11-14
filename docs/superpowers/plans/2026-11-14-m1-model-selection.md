# M1 — Supervisor Model Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user pick a GGUF model already on the machine for the managed `llama.cpp` container
from a Herdr mod picker card. The choice is persisted in `supervisor.toml`.

**Architecture:**
- Two new pure, read-only modules: a GGUF header reader (`gguf.rs`) and a local model scanner and list builder (`models.rs`).
- A TOML writer based on `toml_edit` (`config_write.rs`).
- A `cockpit.model_id` container label, so a model change recreates the container.
- `cockpit supervisor model list|set|filter|reset` CLI ops.
- A picker card in the right drawer of the mod, reusing the AI Usage "only-drawer" mechanism.

**Tech Stack:** Rust 2021 (`serde`, `toml` 0.8, `toml_edit` 0.22, `regex`, `anyhow`, `tempfile` for tests). Herdr GPUI mod in JS (`assets/herdr-app/mods/cockpit/main.js`).

**Spec:** `docs/superpowers/specs/2026-11-14-m1-model-selection-design.md`. Read it fully before starting; section numbers below (§) refer to it.

## Global Constraints

- Linux only. No new crates except `toml_edit = "0.22"`, which is already in `Cargo.lock` through `toml` 0.8.
- Scanning is strictly read-only. It never writes, moves, or locks anything in model stores.
- Unit tests are deterministic: temp dirs, synthetic GGUF bytes, the fake `ContainerCli`. No network, real containers, or sleeps.
- `max_params` values are `2b | 4b | 8b | all`. The default is `4b`. Thresholds: `2b ≤ 2.5e9`, `4b ≤ 4.5e9`, `8b ≤ 8.5e9`.
- Resolution precedence in container mode: TOML `[llm_container] model_id` > `model_path` > `hf_repo` > Ollama-store lookup of `ollama_model` (env-fed).
- Model ids: `ollama:<display_name>`, `file:<canonical path>`, `hf:<org>/<repo>[:quant]`.
- Source labels and dedupe priority: `Ollama` > `LM Studio` > `HF cache` > `llama.cpp cache` > `Folder`. `Config` is used only for the synthetic row.
- Folder scan: max depth 4, skip hidden dirs, do not follow symlinked dirs, at most 2,000 files per root.
- GGUF safety limits: string > 1 MiB, array len > 10M, KV count > 100k, tensor count > 1M, or more than 16 MiB read → error.
- `[llm_container] fallback_model_id` is reserved: parse it, never read it.
- UI copy comes from spec §7. Pt-BR CLI error strings stay consistent with the existing ones.
- **Delegation rules (AGENTS.md):** the worker does **not** commit. Each task ends with "leave changes uncommitted". The coordinator reviews and commits.

## Review Focus

1. **Unreadable store** (e.g. `/usr/share/ollama` owned by another user, `EACCES`): the scan skips that root and still lists the other sources. No error and no panic. → Task 3 test `scan_skips_unreadable_root`.
2. **A `.gguf` that is truncated, corrupt, or not GGUF at all**: listed with params `?` and kind `chat`, not dropped, no panic. → Task 3 test `scan_lists_corrupt_gguf_with_unknown_params`.
3. **`supervisor.toml` with invalid TOML syntax**: the writer returns an error and leaves the file byte-for-byte unchanged. It never overwrites the user's broken file. → Task 6 test `write_refuses_invalid_toml`.
4. **The selected Ollama model was deleted** (`ollama rm`): `resolve_model_source` errors and names it. The list still shows it as a `Config` row marked `in_use`, so the user can switch away. → Task 4 test `missing_effective_gets_config_row`, Task 5 test `resolve_missing_ollama_selection_errors`.
5. **Model paths with spaces or `:`** in folder or file names: the id round-trips through `ModelId::parse` and the container label without truncation. → Task 5 test `model_id_roundtrip_with_spaces_and_colons`.

---

### Task 1: Partial `supervisor.toml` + config fields (M0 prerequisite)

**Files:**
- Modify: `src/supervisor/config.rs` (struct attrs, `load()`, new path helper)
- Modify: `src/supervisor/container.rs` (`LlmContainerConfig` fields + `Default`)

**Interfaces:**
- Produces:
  - `pub fn supervisor_config_path() -> PathBuf`, and the testable `pub fn supervisor_config_path_in(home: &Path) -> PathBuf`: cockpit path if it exists, else legacy (`~/.config/herdr/supervisor.toml`) if it exists, else the cockpit path.
  - `LlmContainerConfig` gains:
    - `model_id: Option<String>` (default `None`);
    - `model_filter: String` (default `"4b"`);
    - `model_dirs: Vec<PathBuf>` (default empty);
    - `fallback_model_id: Option<String>` (default `None`; doc comment: "reserved for M1b; not read").

- [ ] **Step 1: Write failing tests** in `config.rs` tests:
  - `partial_toml_with_only_llm_container_parses`:
    - parse `"[llm_container]\nmodel_id = \"ollama:qwen2.5:3b\"\nmodel_dirs = [\"/m\"]\n"` with `toml::from_str::<SupervisorConfig>`;
    - assert `llm_container.model_id == Some("ollama:qwen2.5:3b")`, `model_dirs == [PathBuf::from("/m")]`, `model_filter == "4b"`, `poll_interval_secs == 12`, `hc.list_sessions_cmd == "hc ls --json"`, `llm_container.port == 11435`.
  - `supervisor_config_path_prefers_cockpit_then_legacy`, with a tempdir HOME, checked in three states:
    - neither file exists → the cockpit path;
    - only the legacy file → the legacy path;
    - both → the cockpit path.
- [ ] **Step 2:** Run `cargo test --quiet partial_toml supervisor_config_path`. Expected: FAIL (does not compile / parse error).
- [ ] **Step 3: Implement.**
  - Add `#[serde(default)]` at struct level on `SupervisorConfig` and `HcAdapterConfig` (both already implement `Default`).
  - Add the new `LlmContainerConfig` fields.
  - `load()` uses `supervisor_config_path_in`.
  - When the file exists but fails to parse, keep the defaults and emit `tracing::warn!` with the path and the error.
- [ ] **Step 4:** Run `cargo test --quiet`. Expected: all pass, including the existing `test_old_toml_without_llm_container_parses`.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 2: GGUF header reader

**Files:**
- Create: `src/supervisor/gguf.rs` (tests inline)
- Modify: `src/supervisor/mod.rs` (`pub mod gguf;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct GgufInfo { pub architecture: Option<String>, pub name: Option<String>,
      pub size_label: Option<String>, pub general_type: Option<String>, pub file_type: Option<u32>,
      pub parameter_count: u64, pub has_pooling: bool }
  pub fn read_gguf_info<R: std::io::Read>(r: R) -> anyhow::Result<GgufInfo>
  pub fn read_gguf_info_path(path: &std::path::Path) -> anyhow::Result<GgufInfo>
  pub fn file_type_label(ft: u32) -> Option<&'static str>
  ```

**Format** (all little-endian; only v2/v3; reject anything else):
- Header: magic `b"GGUF"`, `u32` version, `u64` tensor_count, `u64` kv_count.
- KV pair: string key, `u32` value type, value. Value types:
  - `0` u8, `1` i8, `2` u16, `3` i16, `4` u32, `5` i32, `6` f32, `7` bool;
  - `8` string (`u64` len + bytes);
  - `9` array (`u32` elem type, `u64` count, elems);
  - `10` u64, `11` i64, `12` f64.
- Tensor info: string name, `u32` n_dims, `n_dims × u64` dims, `u32` type, `u64` offset.
- What to extract:
  - `parameter_count` = Σ over tensors of the product of dims.
  - Keys `general.architecture`, `general.name`, `general.size_label`, `general.type`, and `general.file_type` (u32).
  - `has_pooling` = any key ending in `.pooling_type` with an integer value ≠ 0.
  - Unneeded strings and arrays are skipped by length, never allocated.
- Apply the Global Constraints limits. `read_gguf_info_path` wraps a `BufReader` in `.take(16 * 1024 * 1024)`.
- `file_type_label` values:
  - `0` F32, `1` F16, `2` Q4_0, `3` Q4_1, `7` Q8_0, `8` Q5_0, `9` Q5_1;
  - `10` Q2_K, `11` Q3_K_S, `12` Q3_K_M, `13` Q3_K_L, `14` Q4_K_S, `15` Q4_K_M;
  - `16` Q5_K_S, `17` Q5_K_M, `18` Q6_K, `32` BF16;
  - anything else → `None`.

- [ ] **Step 1: Write failing tests.** Add a test-only `GgufBuilder`: `.kv_str(k,v)`, `.kv_u32(k,v)`, `.kv_array_str(k,&[..])`, `.tensor(name,&[dims])`, `.bytes()`. Tests:
  - `reads_metadata_and_counts_params`: arch `"llama"`, name `"Tiny"`, size_label `"1.7B"`, file_type 15, an array KV `tokenizer.ggml.tokens` with 3 strings, tensors `[4096,32000]` and `[4096]` → `parameter_count == 131_076_096`, `file_type_label(15) == Some("Q4_K_M")`.
  - `detects_pooling`: `bert.pooling_type` u32 1 → `has_pooling`; value 0 → not.
  - `rejects_bad_magic_and_v1`: `b"GGML…"` → Err; version 1 → Err.
  - `truncated_input_errors_without_panic`: the valid bytes cut at every length `0..len` → each is `Err`, none panics.
  - `limits_enforced`: a string len of 2 MiB → Err; kv_count 200_000 → Err.
- [ ] **Step 2:** Run `cargo test --quiet gguf`. Expected: FAIL.
- [ ] **Step 3: Implement** in `gguf.rs` per the format above.
- [ ] **Step 4:** Run `cargo test --quiet gguf`. Expected: PASS.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 3: Local model scanner

**Files:**
- Create: `src/supervisor/models.rs` (tests inline)
- Modify: `src/supervisor/mod.rs` (`pub mod models;`)

**Interfaces:**
- Consumes: `gguf::read_gguf_info_path`, `gguf::file_type_label` (Task 2); `LlmContainerConfig.model_dirs` (Task 1).
- Produces:
  ```rust
  #[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)] #[serde(rename_all = "lowercase")]
  pub enum ModelKind { Chat, Embedding, Projector }
  #[derive(Serialize, Clone, Debug, PartialEq)]
  pub struct LocalModel { pub id: String, pub display_name: String, pub source: String,
      pub secondary_label: Option<String>, pub path: PathBuf, pub size_bytes: u64,
      pub params: Option<u64>, pub params_label: String, pub quant: Option<String>,
      pub kind: ModelKind, pub in_use: bool }
  pub struct ScanRoots { pub ollama: Vec<PathBuf>, pub hf_hub: Vec<PathBuf>,
      pub llama_cpp: Vec<PathBuf>, pub lm_studio: Vec<PathBuf>, pub folders: Vec<PathBuf> }
  impl ScanRoots { pub fn from_env_and_config(cfg: &LlmContainerConfig) -> Self }
  pub fn scan_local_models(roots: &ScanRoots) -> Vec<LocalModel>
  pub fn parse_params_label(s: &str) -> Option<u64>   // "1.7B"→1_700_000_000, "135M"→135_000_000
  pub fn format_params(n: u64) -> String              // ≥1e9 "{:.1}B" ("1.7B"), else "{}M" rounded
  ```

**Behavior:**
- **`from_env_and_config` defaults:**
  - `ollama`: the existing `container::default_ollama_roots()`.
  - `hf_hub`: `$HF_HOME/hub`, `~/.cache/huggingface/hub`.
  - `llama_cpp`: `$LLAMA_CACHE`, `~/.cache/llama.cpp`, `~/.cache/cockpit/llama.cpp`.
  - `lm_studio`: `~/.lmstudio/models`, `~/.cache/lm-studio/models`.
  - `folders`: `COCKPIT_MODEL_DIRS` (colon-separated) first, then `cfg.model_dirs`. Expand a leading `~`.
- **Ollama:** walk `manifests/<registry>/<ns>/<name>/<tag>`.
  - Display name: `name:tag` when registry is `registry.ollama.ai` and namespace is `library`; otherwise `registry/ns/name:tag`.
  - Model layer `application/vnd.ollama.image.model` → `blobs/<digest with ':'→'-'>`. Size comes from the layer `size`.
  - Config layer JSON:
    - `model_type` → params label and params (via `parse_params_label`);
    - `file_type` → quant;
    - `model_family` `bert` | `nomic-bert` → Embedding.
  - If `model_type` is missing, use the GGUF header for params and kind.
- **HF hub:** `models--<org>--<repo>/snapshots/<rev>/**/*.gguf`. `secondary_label` is `<org>/<repo>`. The `llama_cpp` roots are scanned both flat (`*.gguf`) and with the hub layout. **LM Studio:** `<publisher>/<repo>/*.gguf`, with secondary label `<publisher>/<repo>`.
- **Folder:** see Global Constraints.
- **File-based entries** (all sources except Ollama):
  - id is `file:<canonical path>`;
  - the display name is the file stem without `.gguf`;
  - params come from the header (`size_label` via `parse_params_label`, else `parameter_count`, with 0 meaning unknown);
  - the quant comes from `file_type_label`.
- **Kind:** Projector if `architecture == "clip"`, or `general_type == "mmproj"`, or the filename contains `mmproj`. Embedding if the architecture is in `["bert","nomic-bert","jina-bert-v2","t5encoder"]` or `has_pooling`. Otherwise Chat. If the header read fails: params unknown, kind Chat, quant `None`, and the entry is kept.
- **Split shards:** match `-(\d{5})-of-(\d{5})\.gguf$` (`regex`).
  - Keep only shard `00001`. Its display name drops the suffix.
  - Size = sum of the sibling shards that exist.
  - Params come only from `size_label`.
- **Dedupe:** by canonical path. When two sources see the same file, the source higher in the priority list wins.
- **Ordering:** by source priority, then case-insensitive display name. `params_label` is `"?"` when params are unknown.

- [ ] **Step 1: Write failing tests** (tempdir fixtures; write GGUF bytes with the Task 2 test builder, exposed as `pub(crate)` under `#[cfg(test)]`):
  - `scans_ollama_library_and_other_registry`: two manifests (library `lfm3:1.7b` with `model_type "1.7B"`, `file_type "Q4_K_M"`; `hf.co/org/repo:Q8_0`) → display names `lfm3:1.7b` and `hf.co/org/repo:Q8_0`, `params == Some(1_700_000_000)`, `quant == Some("Q4_K_M")`, source `"Ollama"`.
  - `ollama_bert_family_is_embedding`.
  - `scans_hf_hub_snapshot_symlink_and_dedupes_with_llama_cpp_cache`: a hub snapshot symlink to a blob, and the same canonical file reachable from a `llama_cpp` root → one entry, source `"HF cache"`, `secondary_label == Some("org/repo")`.
  - `scans_lm_studio_tree`.
  - `folder_scan_respects_depth_hidden_and_symlink_dirs`: a file at depth 5 is excluded; `.hidden/x.gguf` is excluded; a symlinked dir loop terminates and its files are excluded.
  - `split_shards_listed_once_with_summed_size`.
  - `kind_detection_projector_and_embedding`: an `mmproj-x.gguf` file → Projector; a `has_pooling` header → Embedding.
  - `scan_lists_corrupt_gguf_with_unknown_params` (Review Focus 2): a `junk.gguf` file containing random bytes → present, `params == None`, `params_label == "?"`, kind Chat.
  - `scan_skips_unreadable_root` (Review Focus 1): chmod a root to `0o000` (skip the test if running as root) → other roots still listed, no panic.
  - `params_label_roundtrip`: `parse_params_label("1.7B") == Some(1_700_000_000)`, `("135M") == Some(135_000_000)`, `format_params(3_090_000_000) == "3.1B"`, `format_params(135_000_000) == "135M"`.
- [ ] **Step 2:** Run `cargo test --quiet models::`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run `cargo test --quiet models::`. Expected: PASS.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 4: Filter and list assembly

**Files:**
- Modify: `src/supervisor/models.rs`

**Interfaces:**
- Consumes: `LocalModel`, `ModelKind` (Task 3).
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum MaxParams { B2, B4, B8, All }
  impl MaxParams { pub fn parse(s: &str) -> Option<Self>; pub fn as_str(&self) -> &'static str; pub fn limit(&self) -> Option<u64> }
  #[derive(Clone, Debug, PartialEq)] pub struct EffectiveModel { pub id: String, pub display_name: String, pub path: Option<PathBuf> }
  #[derive(Serialize, Debug)] pub struct ModelListOutput { pub models: Vec<LocalModel>, pub hidden_count: usize,
      pub filter: String, pub selected_id: Option<String>, pub managed: bool, pub env_override_ignored: Option<String> }
  pub fn build_model_list(scanned: Vec<LocalModel>, filter: MaxParams, effective: Option<&EffectiveModel>, show_all: bool) -> (Vec<LocalModel>, usize)
  ```

**Behavior:**
- Set `in_use = true` on the row whose id equals `effective.id`.
- If no scanned row matches, append a synthetic row: source `"Config"`, `in_use: true`, params `?`, kind Chat, `size_bytes` from the file when `path` exists, else 0.
- Unless `show_all`:
  - drop non-Chat rows;
  - drop rows with `params > limit`, except the row that is in use;
  - **keep** rows with unknown params.
- `hidden_count` = rows dropped. It is 0 when `show_all`.

- [ ] **Step 1: Write failing tests:**
  - `filter_thresholds`: params 2.4e9 kept by `B2`; 2.6e9 dropped by `B2` and kept by `B4`; 4.4e9 kept by `B4`; 8.6e9 dropped by `B8`; everything kept by `All`. Check `hidden_count` each time.
  - `in_use_always_shown`: a 7B model in use with filter `B4` → present, `in_use == true`.
  - `unknown_params_shown`.
  - `embeddings_hidden_and_counted`: 1 embedding + 1 projector → both hidden, `hidden_count == 2`; with `show_all` both present and `hidden_count == 0`.
  - `missing_effective_gets_config_row` (Review Focus 4): effective `ollama:gone:1b` absent from the scan → the last row has source `"Config"`, `id == "ollama:gone:1b"`, `in_use`.
  - `max_params_parse`: `"4b"`/`"4B"` → `B4`; `"all"` → `All`; `"5b"` → `None`; `B4.limit() == Some(4_500_000_000)`.
- [ ] **Step 2:** Run `cargo test --quiet models::`. Expected: new tests FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run `cargo test --quiet models::`. Expected: PASS.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 5: Model id, resolution precedence, effective model, env note

**Files:**
- Modify: `src/supervisor/container.rs` (`ModelId`, `resolve_model_source`)
- Modify: `src/supervisor/models.rs` (`effective_model`, `env_override_note`)
- Modify: `src/supervisor/tui.rs:88` and `src/supervisor/ollama.rs` (`probe_llm_health` `status.model`): in container mode, `llm_model` = the effective display name.

**Interfaces:**
- Consumes: `LlmContainerConfig.model_id` (Task 1), `EffectiveModel` (Task 4), `ScanRoots` (Task 3).
- Produces:
  ```rust
  // container.rs
  #[derive(Clone, Debug, PartialEq, Eq)] pub enum ModelId { Ollama(String), File(PathBuf), Hf(String) }
  impl ModelId { pub fn parse(s: &str) -> anyhow::Result<Self>; pub fn as_id(&self) -> String }  // split on the FIRST ':' only
  // resolve_model_source keeps its signature; model_id is checked first
  // models.rs
  pub fn effective_model(cfg: &SupervisorConfig, ollama_roots: &[PathBuf]) -> Option<EffectiveModel>
  pub fn env_override_note(model_id_set: bool, effective_display: &str, cockpit_llm_model: Option<&str>, ollama_model_env: Option<&str>) -> Option<String>
  ```

**Behavior:**
- **`resolve_model_source`**, new first branch when `cfg.model_id` is Some:
  - `Ollama(name)` → the existing `resolve_ollama_gguf_blob(name, roots)`, else `Err("Modelo selecionado não encontrado: <name>")`.
  - `File(p)` → if it exists, `ModelSource::File(p)`, else `Err("Modelo selecionado não encontrado: <path>")`.
  - `Hf(r)` → `ModelSource::HfRepo(r)`.
  - The remaining branches are unchanged.
- **`effective_model`:**
  - `model_id` → display name: the Ollama name, the file stem, or the repo.
  - Else `model_path` → `file:<path>`.
  - Else `hf_repo` → `hf:<repo>`.
  - Else, if `ollama_model` is non-empty, `ollama:<ollama_model>` (path = the resolved blob if found).
  - Otherwise `None`.
- **`env_override_note`:** returns `Some("<VAR>=<value>")` only when `model_id_set` and an env value is set and differs from `effective_display`. `COCKPIT_LLM_MODEL` is checked before `OLLAMA_MODEL`.

- [ ] **Step 1: Write failing tests:**
  - `model_id_roundtrip_with_spaces_and_colons` (Review Focus 5): `file:/m/my model:v2.gguf` → `File("/m/my model:v2.gguf")` → `as_id()` is identical; `ollama:hf.co/org/repo:Q8_0` → `Ollama("hf.co/org/repo:Q8_0")`; `bogus` → Err.
  - `resolve_prefers_model_id_over_model_path_and_hf_repo`.
  - `resolve_missing_ollama_selection_errors` (Review Focus 4): the error message contains the model name.
  - `resolve_missing_file_selection_errors`.
  - `effective_model_precedence` across the four branches.
  - `env_override_note_cases`:
    - no `model_id` → `None`;
    - env equals the effective model → `None`;
    - `OLLAMA_MODEL` differs → `Some("OLLAMA_MODEL=lfm3:1.7b")`;
    - both env vars set → reports `COCKPIT_LLM_MODEL`.
- [ ] **Step 2:** Run `cargo test --quiet`. Expected: new tests FAIL.
- [ ] **Step 3: Implement.** The snapshot `llm_model` shows the effective display name when `container_mode_active()`.
- [ ] **Step 4:** Run `cargo test --quiet`. Expected: PASS.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 6: `supervisor.toml` writer (`toml_edit`)

**Files:**
- Create: `src/supervisor/config_write.rs` (tests inline)
- Modify: `src/supervisor/mod.rs` (`pub mod config_write;`), `Cargo.toml` (`toml_edit = "0.22"`)

**Interfaces:**
- Produces:
  ```rust
  pub fn write_model_selection(path: &Path, model_id: &str, ollama_name: Option<&str>) -> anyhow::Result<()>
  pub fn write_model_filter(path: &Path, filter: &str) -> anyhow::Result<()>
  pub fn clear_model_selection(path: &Path) -> anyhow::Result<()>
  ```

**Behavior:**
- Read the file, or start from an empty document if it is missing.
- Parse with `toml_edit::DocumentMut`. A parse error → return `Err` and do **not** write.
- Ensure the `[llm_container]` table exists, set or remove the keys, and write atomically (`<file>.tmp` + rename), creating parent dirs.
- `write_model_selection` sets `llm_container.model_id`, and also the top-level `ollama_model` when `ollama_name` is Some.
- `clear_model_selection` removes only `llm_container.model_id`.

- [ ] **Step 1: Write failing tests:**
  - `preserves_comments_and_unrelated_keys`: the input `"# keep me\npoll_interval_secs = 30\n\n[llm_container]\nport = 11500 # mine\n"` → after `write_model_selection(…, "file:/m/a.gguf", None)` the output still contains `# keep me`, `poll_interval_secs = 30`, and `port = 11500 # mine`, plus `model_id = "file:/m/a.gguf"`.
  - `sets_ollama_model_for_ollama_selection`.
  - `creates_missing_file_and_parent_dirs`, and the result parses as `SupervisorConfig` (needs Task 1).
  - `clear_removes_only_model_id`.
  - `write_filter_sets_model_filter`.
  - `write_refuses_invalid_toml` (Review Focus 3): the input `"poll_interval_secs = = 3"` → `Err`, and the file bytes are unchanged.
- [ ] **Step 2:** Run `cargo test --quiet config_write`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run `cargo test --quiet config_write`. Expected: PASS.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 7: `cockpit.model_id` label → recreate on model change

**Files:**
- Modify: `src/supervisor/container.rs` (`build_run_args`, `LlmContainer::start`, new label reader)
- Modify: `src/supervisor/mod.rs` (`load_model`, `set_gpu` callers pass the effective id)

**Interfaces:**
- Consumes: `models::effective_model` (Task 5).
- Produces:
  - `pub fn build_run_args(cfg, runtime, ngl, model, hf_cache_dir, model_id: &str) -> Vec<String>`, which adds `--label cockpit.model_id=<model_id>`.
  - `pub fn container_model_id(&self) -> Option<String>`.
  - `pub fn start(&self, ngl: u32, model: &ModelSource, model_id: &str) -> Result<()>`: recreate when `container_ngl() != Some(ngl)` **or** `container_model_id().as_deref() != Some(model_id)`. Running/Stopped/Missing otherwise behave as today.

- [ ] **Step 1: Write failing tests** with the existing `FakeCli`:
  - `run_args_include_model_id_label`: the args contain `"cockpit.model_id=ollama:lfm3:1.7b"`.
  - `stopped_with_same_ngl_but_other_model_recreates`: the recorded argv includes `rm -f` followed by `run`.
  - `running_with_same_labels_is_noop`: no `rm`/`run` recorded.
  - Update the existing start/stop tests to the new signature.
- [ ] **Step 2:** Run `cargo test --quiet container`. Expected: FAIL.
- [ ] **Step 3: Implement.** Update the callers in `mod.rs`. Pass the `effective_model(...).id`, or `""` when there is none.
- [ ] **Step 4:** Run `cargo test --quiet`. Expected: PASS.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 8: Supervisor ops + `cockpit supervisor model …` CLI

**Files:**
- Modify: `src/supervisor/mod.rs` (ops), `src/main.rs` (`SupervisorCommands::Model` + dispatch)

**Interfaces:**
- Consumes: Tasks 3–7.
- Produces:
  ```rust
  pub fn list_models(filter_override: Option<MaxParams>, show_all: bool) -> Result<ModelListOutput>
  pub fn set_model(id: &str) -> Result<DesktopModSnapshot>
  pub fn set_model_filter(filter: MaxParams) -> Result<ModelListOutput>
  pub fn reset_model() -> Result<DesktopModSnapshot>
  ```
- CLI (clap):
  - `model list [--json] [--max-params <2b|4b|8b|all>] [--all]`;
  - `model set <ID>`;
  - `model filter <2b|4b|8b|all>`;
  - `model reset`.

  `list` without `--json` prints a table with the columns `·` (● if in use), NAME, PARAMS, SOURCE, SIZE, QUANT, ID. The other commands print JSON like `load` does.

**Behavior:**
- **`list_models`:**
  - If not `container_mode_active()` → `managed: false` and an empty list.
  - Otherwise: scan with `ScanRoots::from_env_and_config`, compute `effective_model`, run `build_model_list`, and fill `selected_id` (= `cfg.llm_container.model_id`), `filter`, and `env_override_ignored`.
  - Filter = `filter_override`, else `MaxParams::parse(&cfg.llm_container.model_filter)`, else `B4`.
- **`set_model`:**
  1. Container mode inactive → `bail!("Model selection applies to the managed llama.cpp container (container mode is off or no runtime was found)")`.
  2. The id must be a fresh-scan id **or** the current effective id. Otherwise → `bail!("Modelo não encontrado: <id>")`.
  3. Same as the current `model_id` → no-op, return the snapshot.
  4. Otherwise `write_model_selection(&supervisor_config_path(), id, ollama name if Ollama)`, then reload the config.
  5. If the container is Running → `start(ngl, &resolve…, id)` + `wait_healthy(…, 90 s)`. On failure: `stop()`, then `Err` naming the display name.
  6. Return the enriched snapshot.
- **`reset_model`:** `clear_model_selection`, then the same Running → recreate rule.
- **`set_model_filter`:** `write_model_filter`, then return `list_models(None, false)`.

- [ ] **Step 1: Write failing test** in `models.rs` or `mod.rs`: `set_model_rejects_unknown_id`. Factor the validation into the pure fn `validate_model_choice(id: &str, scanned_ids: &[String], effective_id: Option<&str>) -> Result<()>` and test it: an unknown id → Err containing the id; a scanned id → Ok; the effective id when absent from the scan → Ok.
- [ ] **Step 2:** Run `cargo test --quiet validate_model_choice`. Expected: FAIL.
- [ ] **Step 3: Implement** the ops and the CLI.
- [ ] **Step 4: Verify.** Run `cargo test --quiet && cargo build --release`, then with a temp HOME and a fixture folder:
  ```bash
  H=$(mktemp -d); mkdir -p $H/models; cp <any small .gguf or Task-3 fixture bytes> "$H/models/a model.gguf"
  HOME=$H COCKPIT_LLM_CONTAINER=on COCKPIT_MODEL_DIRS=$H/models ./target/release/cockpit supervisor model list --json
  HOME=$H COCKPIT_LLM_CONTAINER=on COCKPIT_MODEL_DIRS=$H/models ./target/release/cockpit supervisor model set "file:$H/models/a model.gguf"
  cat $H/.config/cockpit/supervisor.toml      # contains model_id
  HOME=$H COCKPIT_LLM_CONTAINER=on ./target/release/cockpit supervisor model filter 2b
  HOME=$H COCKPIT_LLM_CONTAINER=on ./target/release/cockpit supervisor model reset
  ```
  Expected:
  - `list` shows the file with `in_use: false`, then `true` after `set`.
  - The TOML has `model_id` after `set`, and `model_filter = "2b"` after `filter`.
  - `reset` removes `model_id`.
  - With `COCKPIT_LLM_CONTAINER=off`, `list` returns `managed: false` and `set` errors.
- [ ] **Step 5:** Leave changes uncommitted.

### Task 9: Herdr mod picker card

**Files:**
- Modify: `assets/herdr-app/mods/cockpit/main.js`

**Interfaces:**
- Consumes: the CLI from Task 8 (JSON shape `ModelListOutput`; `set`/`reset` print a snapshot).
- Produces: methods `toggleModelPicker(cx)`, `closeModelPicker(cx)`, `fetchModelList(cx)`, `selectModel(cx, id)`, `setModelFilter(cx, f)`, `renderModelPicker(cx, t)`; shared keys `cockpit.modelPickerOpen`, `cockpit.modelPickerOnlyDrawer`, `cockpit.modelList`.

**Behavior** (spec §7; mirror `toggleUsagePopup` / `closeUsagePopup` / `renderUsagePopover`):
- **Entry points:**
  - wrap the top-bar LLM `pillBadge` (`main.js` ~2118) in a clickable element with id `ck-llm-pill`;
  - make the Inspector "Supervisor & Guardrails" LLM card (the row showing `<model> (llama-cpp · CPU)` + state badge) clickable, with id `ck-inspector-llm-card`.

  Both call `toggleModelPicker`.
- **Opening:**
  - Drawer hidden → open it in model-only mode (`run("layout.toggle", { id: "cockpit-inspector" })`). Closing the card hides it again.
  - Drawer open → render the card at the top of the current view.
  - Opening the picker closes the AI Usage card and vice versa.
- **Exec calls:**
  - On open and on `↻ Rescan`: `exec("cockpit", ["supervisor","model","list","--json"], {timeout: 30000})`.
  - The "N hidden by filter" click adds `--all` for this card session (not persisted). Non-chat rows are then shown dimmed and are not clickable.
  - Filter segment: `["supervisor","model","filter",f]`.
  - Row click: `["supervisor","model","set",id]` with `{timeout: 300000}`. While in flight, the row shows `◌ Switching…` and other row clicks are ignored. On success, `setShared("supervisor.snapshot", parsed)` and refetch the list.
- **States:**
  - loading: `Scanning…`;
  - empty: the copy from spec §7;
  - `managed: false`: a one-line note;
  - errors: stderr, shown in the card;
  - `env_override_ignored`: a warning line `"<VAR=value> is set in the environment and ignored in container mode"`.
- **Rows:** a radio (`●` in use / `○`), display name, params label, source, `fmtBytes(size_bytes)`, and the quant when present.
- **Theme:** no `"transparent"` colors, no `.truncate()` on `v_flex`, and keep the top-bar center cluster pinned (`7a8c17b`).

- [ ] **Step 1:** Implement.
- [ ] **Step 2:** Run `node --check assets/herdr-app/mods/cockpit/main.js`. Expected: no output, exit 0.
- [ ] **Step 3:** Run `grep -n "ck-llm-pill\|ck-inspector-llm-card\|renderModelPicker" assets/herdr-app/mods/cockpit/main.js`. Expected: each one is present.
- [ ] **Step 4:** Leave changes uncommitted.

### Task 10: Full verification and container smoke

**Files:** none (verification only). Remove any temp files and containers you create.

- [ ] **Step 1:** Format only the files you touched (`rustfmt --edition 2021 <files>`; never run a repo-wide `cargo fmt`), then run `cargo test --quiet`, `cargo clippy --all-targets 2>&1 | grep -E "gguf.rs|models.rs|config_write.rs"` (expected: no warnings in the new files), and `node --check …/main.js`.
- [ ] **Step 2: Container smoke** (optional; needs network; report "not run" if impossible).
  - Setup: temp HOME; download two tiny GGUFs into `$H/models`, e.g. from `bartowski/SmolLM2-135M-Instruct-GGUF` and `bartowski/SmolLM2-360M-Instruct-GGUF` via `https://huggingface.co/<repo>/resolve/main/<file>`.
  - Run with `COCKPIT_LLM_CONTAINER=on COCKPIT_MODEL_DIRS=$H/models` and a unique `[llm_container] name`/`port` in `$H/.config/cockpit/supervisor.toml`.
  - Sequence: `model set <A>` → `load` → `status` (running, `llm_model` = A) → `model set <B>` while running → the container is recreated (`docker inspect -f '{{index .Config.Labels "cockpit.model_id"}}'` = B) → healthy → `unload` → `model set <A>` while stopped → the container stays stopped.
  - Afterwards: `docker rm -f` the smoke container.
- [ ] **Step 3:** Write the return file (path given in the brief) with the actual outputs.
