# CPU vs GPU Benchmark + GPU Backend Detection — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:**
- Detect the GPU backend (CUDA, Vulkan, or ROCm) for the llama.cpp container.
- Add `tchelvis supervisor bench`, which compares CPU and GPU speed on the supervisor's real request.
- Show it in a card in the Doctor tab of the Herdr mod.

**Architecture:**
- **Backend:** `GpuBackend` detection and per-backend run args live in `container.rs`, with a new `tchelvis.gpu` label on the container. All ngl > 0 start paths go through one function that resolves the backend.
- **New `src/supervisor/bench.rs`:** pure helpers (median, speedup, timings parsing, restore plan) plus the I/O orchestration. Progress and result go to JSON files.
- **Mod:** reads the result and polls the progress file.

**Tech Stack:** Rust (`ureq`, `serde_json`, existing container lifecycle), Herdr GPUI mod JS.

**Spec:** `docs/superpowers/specs/2026-11-15-cpu-gpu-bench-design.md`. Read it fully.

## Global Constraints
- **New `[llm_container]` keys** (`#[serde(default)]`):
  - `gpu_backend = "auto"`;
  - `image_vulkan = "ghcr.io/ggml-org/llama.cpp:server-vulkan"`;
  - `image_rocm = "ghcr.io/ggml-org/llama.cpp:server-rocm"`.
  - Env override: `TCHELVIS_GPU_BACKEND`, read via `envvars::var("GPU_BACKEND")`.
- **Detection order:** `/dev/nvidia0` or `nvidia-smi` → Cuda; else any `/dev/dri/renderD*` → Vulkan; else None.
- **GPU flags:**
  - Cuda: docker/kiri `--gpus all`, podman `--device nvidia.com/gpu=all`.
  - Vulkan: `--device /dev/dri` + `--group-add video` (podman: `--group-add keep-groups`).
  - Rocm: `--device /dev/kfd --device /dev/dri --group-add video --security-opt seccomp=unconfined`.
- **Errors (exact text):**
  - "No GPU detected (gpu_backend=auto)";
  - "Benchmark requires the managed llama.cpp container";
  - "A benchmark is already running".
- **State files under `~/.local/state/tchelvis/`:** `bench.lock`, `bench_progress.json`, `bench_last.json`. All writes are atomic (tmp + rename).
- **Bench defaults:** `--runs 3`, plus 1 warm-up per mode; the median is reported. Health timeout is 300 s per mode, and the mod's exec timeout is 900000 ms.
- **Process rules:** deterministic tests (FakeCli, fixtures). Format only the files you touched; never run a repo-wide `cargo fmt`. The worker never commits.

## Review Focus
1. **The GPU start fails midway** (missing image, device permission, health timeout). The CPU result is still reported, and the container is restored to its prior state. → Task 2 test `gpu_failure_still_restores_and_reports_cpu`.
2. **The container was stopped before the bench.** After the bench it is stopped again, not left running with the GPU model. → Task 2 test `restore_plan_cases`.
3. **A second bench starts while one runs** (mod double-click, CLI plus mod). It is refused with "A benchmark is already running". A stale lock (dead pid) is ignored. → Task 2 test `lock_refuses_live_pid_and_ignores_stale`.
4. **A response lacks `timings`** (older llama-server, or a proxy). That measurement falls back to wall time only, with tok/s shown as `null`, and there is no panic. → Task 2 test `parse_timings_missing_is_none`.
5. **A user-forced `gpu_backend` with no matching device.** It is honored as-is: no silent switch, and any error surfaces in the GPU row. → Task 1 test `forced_backend_wins`.

## Batches
- **B1:** Tasks 1–2 (Rust).
- **B2:** Task 3 (mod).
- The coordinator reviews and commits between batches.

---

### Task 1: GPU backend detection and per-backend run args

**Files:** `src/supervisor/container.rs`, `src/supervisor/mod.rs` (`load_model`, `set_gpu`, `recreate_container_if_running` pass the backend), `src/supervisor/tui.rs` and `src/supervisor/ollama.rs` (snapshot / health fields `gpu_backend`, `gpu_detail`), `src/doctor.rs` (show them in the supervisor row).

**Interfaces:**
- **Produces:**
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)] #[serde(rename_all = "lowercase")]
  pub enum GpuBackend { Cuda, Vulkan, Rocm, None }
  impl GpuBackend { pub fn parse(s: &str) -> Option<Self>; pub fn as_str(&self) -> &'static str }
  pub fn detect_gpu_backend(exists: impl Fn(&Path) -> bool, on_path: impl Fn(&str) -> bool, render_nodes: &[PathBuf]) -> GpuBackend
  pub fn resolve_gpu_backend(cfg: &LlmContainerConfig) -> (GpuBackend, String /* detail, e.g. "vulkan · /dev/dri/renderD128" */)
  // build_run_args gains `gpu: GpuBackend` (used only when ngl > 0) and adds `--label tchelvis.gpu=<backend>`
  // LlmContainer::start(ngl, model, model_id, gpu) recreates when ngl, model_id OR the tchelvis.gpu label differ
  pub fn container_gpu(&self) -> Option<String>
  ```
- **Behavior:**
  - `start` with `ngl > 0` and `GpuBackend::None` → `Err("No GPU detected (gpu_backend=auto)")`.
  - `resolve_gpu_backend`: a forced value is used as-is; `auto` lists `/dev/dri/renderD*`.

- [ ] **Step 1: Write failing tests:**
  - `detect_nvidia_device_or_smi`;
  - `detect_render_node_is_vulkan`;
  - `detect_nothing_is_none`;
  - `forced_backend_wins` (`gpu_backend = "rocm"` with no devices → Rocm);
  - `run_args_per_backend`:
    - Cuda docker contains `--gpus all` and `image_gpu`;
    - Cuda podman contains `--device nvidia.com/gpu=all`;
    - Vulkan docker contains `--device /dev/dri`, `--group-add video` and `image_vulkan`;
    - Rocm contains `--device /dev/kfd` and `image_rocm`;
    - ngl 0 contains no GPU flags and uses `image`;
  - `gpu_label_change_recreates` (FakeCli: stopped container, same ngl and model, label `vulkan` vs requested `cuda` → `rm -f` + `run`);
  - `start_gpu_none_errors`.
- [ ] **Step 2:** Run `cargo test --quiet container`. Expected: FAIL.
- [ ] **Step 3: Implement.** Update all callers of `start` / `build_run_args` / `start_with_fallback` to pass `resolve_gpu_backend(&cfg).0`.
- [ ] **Step 4:** Run `cargo test --quiet`. Expected: PASS.
- [ ] **Step 5:** Leave uncommitted.

### Task 2: `tchelvis supervisor bench`

**Files:** Create `src/supervisor/bench.rs`; modify `src/supervisor/mod.rs` (`pub mod bench;`), `src/main.rs` (`SupervisorCommands::Bench { runs: Option<u32>, json: bool, last: bool, progress: bool }`).

**Interfaces:**
- **Consumes:**
  - `GpuBackend`, `resolve_gpu_backend`, `LlmContainer::start(…, gpu)` (Task 1);
  - `OllamaClient::build_openai_chat_request_body` (`src/supervisor/ollama.rs:476`);
  - `container::wait_healthy`, `resolve_model_source`, `models::effective_model`.
- **Produces:**
  ```rust
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] pub struct ModeResult { pub backend: String, pub prompt_tps: Option<f64>, pub gen_tps: Option<f64>, pub eval_secs: Option<f64>, pub error: Option<String> }
  #[derive(Serialize, Deserialize, Clone, Debug)] pub struct BenchResult { pub at: u64, pub model: String, pub quant: Option<String>, pub runs: u32, pub cpu: ModeResult, pub gpu: ModeResult, pub speedup_gen: Option<f64> }
  #[derive(Serialize, Deserialize, Clone, Debug)] pub struct BenchProgress { pub phase: String /* loading|warmup|measuring|restoring|done */, pub mode: String /* cpu|gpu */, pub run: u32, pub runs: u32, pub started_at: u64 }
  pub const SAMPLE_OUTPUT: &str;   // ~20 lines of agent output ending in "Approve Bash: ls /tmp | head -5 ? (y/n)"
  pub fn parse_timings(resp: &serde_json::Value) -> (Option<f64>, Option<f64>)   // (timings.prompt_per_second, timings.predicted_per_second)
  pub fn median(values: &[f64]) -> Option<f64>
  pub fn speedup(cpu_gen: Option<f64>, gpu_gen: Option<f64>) -> Option<f64>      // gpu/cpu, rounded to 1 decimal
  pub enum RestoreAction { Recreate { ngl: u32, gpu: GpuBackend }, Stop }
  pub fn restore_plan(prior_state: ContainerState, prior_ngl: Option<u32>, prior_gpu: Option<GpuBackend>) -> RestoreAction
     // Running → Recreate(prior ngl/gpu, default ngl 0 / None); Stopped|Missing → Stop
  pub fn acquire_lock(path: &Path, pid_alive: impl Fn(i32) -> bool) -> anyhow::Result<LockGuard>   // refuses a live pid; overwrites a stale one; removed on drop
  pub fn run_bench(runs: u32, on_progress: impl FnMut(&BenchProgress)) -> anyhow::Result<BenchResult>
  ```
- **`run_bench` flow:**
  1. Container mode check, model resolution, lock.
  2. Read the prior state: `state()`, `container_ngl()`, `container_gpu()`.
  3. **CPU:** start ngl 0 → wait for health (300 s) → 1 warm-up → `runs` measured requests (`POST {host}/v1/chat/completions` with the real request body and `SAMPLE_OUTPUT`; median of the timings and wall time).
  4. **GPU:** same with `gpu_layers` and the resolved backend. Any error → `gpu.error`, and the run continues.
  5. Restore per `restore_plan`. This must run even after a GPU error; use a guard or a `finally`-style block.
  6. Write `bench_last.json`; the progress file ends in phase `done`.
- **CLI:**
  - `--json` prints the `BenchResult`. Otherwise it prints a table plus `GPU ≈ N× faster at generation` / `GPU unavailable: <error>`.
  - `--last` prints the stored result, or exits 1 with "No benchmark yet".
  - `--progress` prints the current progress JSON, or `{}`.
  - Progress lines also go to stderr.

- [ ] **Step 1: Write failing tests:**
  - `parse_timings_reads_values`: fixture `{"timings":{"prompt_per_second":180.5,"predicted_per_second":21.25}}` → `(Some(180.5), Some(21.25))`.
  - `parse_timings_missing_is_none` (Review Focus 4).
  - `median_odd_even_empty`: `[3,1,2]` → 2; `[1,2,3,4]` → 2.5; `[]` → None.
  - `speedup_math`: `(Some(21.4), Some(95.1))` → `Some(4.4)`; any None → None.
  - `restore_plan_cases` (Review Focus 2): Running(ngl 99, Vulkan) → Recreate{99, Vulkan}; Stopped → Stop; Missing → Stop.
  - `lock_refuses_live_pid_and_ignores_stale` (Review Focus 3).
  - `gpu_failure_still_restores_and_reports_cpu` (Review Focus 1): factor the mode loop over a trait or a closure `measure_mode(ngl, gpu) -> Result<ModeResult>` plus `restore(action)`, so a fake can make GPU fail. Assert that the CPU result is present, `gpu.error` is Some, and restore was called once with the planned action.
- [ ] **Step 2:** Run `cargo test --quiet bench`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Run `cargo test --quiet && cargo build --release`. Expected: PASS.
- [ ] **Step 5: Manual check in `ai-box`** (no GPU), using a temp HOME and the SmolLM2 GGUF referenced by `model_id`:
  ```bash
  H=$(mktemp -d); mkdir -p $H/.config/tchelvis $H/models; cp <SmolLM2-135M gguf> $H/models/
  printf '[llm_container]\nname = "tchelvis-llm-bench"\nport = 11441\nmodel_id = "file:%s/models/<file>.gguf"\n' "$H" > $H/.config/tchelvis/supervisor.toml
  HOME=$H TCHELVIS_LLM_CONTAINER=on ./target/release/tchelvis supervisor bench --runs 2
  docker ps -a --filter name=tchelvis-llm-bench --format '{{.Status}}'   # prior state was Missing → expect Exited/stopped
  HOME=$H ./target/release/tchelvis supervisor bench --last --json
  docker rm -f tchelvis-llm-bench
  ```
  Expected: a CPU row with numbers, `GPU unavailable: No GPU detected (gpu_backend=auto)`, the container stopped afterwards, and `--last` returning the same JSON.
  A GGUF may exist at `/tmp/claude-1000/-home-ubuntu--t3-projects-cockpit/8a313547-896e-4869-bcef-4a6162cdb700/scratchpad/m1smoke/home/models/`; otherwise report the check as not run.
- [ ] **Step 6:** Leave uncommitted.

### Task 3: Doctor tab benchmark card (mod)

**Files:** `assets/herdr-app/mods/tchelvis/main.js` (`renderDoctorBody` ~3738 and the doctor fetch ~1502).

- **Card "CPU vs GPU benchmark"** in the Doctor view, below the existing checks:
  - **On Doctor open:** `exec("tchelvis", ["supervisor","bench","--last","--json"])`. On success, show the model, a date (`formatTimestamp(at)`), the table, and the speedup line; on exit 1, show "No benchmark yet".
  - **Run benchmark button:** `exec("tchelvis", ["supervisor","bench","--json"], {timeout: 900000})`. While in flight:
    - disable the button;
    - poll `exec("tchelvis", ["supervisor","bench","--progress","--json"])` every 1000 ms with `cx.timer` and render `Loading CPU…` / `Warm-up GPU (vulkan)…` / `Measuring GPU (vulkan) 2/3…` plus elapsed seconds;
    - stop polling when the exec resolves.
  - **On success:** render the result. **On failure:** show stderr.
- **Table:** columns `Mode | Prompt tok/s | Gen tok/s | Eval (s)`. The GPU label is `GPU (<backend>)`. A row with `error` shows the error text in place of the numbers. Below the table: `GPU ≈ N× faster at generation` when `speedup_gen` is present.
- **Note under the button:** "Takes 1–5 min; the first GPU run may download the GPU image."
- **Supervisor row:** show the snapshot's `gpu_backend` / `gpu_detail` when present.
- No `"transparent"` colors and no `.truncate()` on `v_flex`.

- [ ] **Step 1:** Implement.
- [ ] **Step 2:** Run `node --check assets/herdr-app/mods/tchelvis/main.js`. Expected: exit 0.
- [ ] **Step 3:** Run `grep -n "bench\", \"--last\"\|bench\", \"--progress\"\|CPU vs GPU benchmark" assets/herdr-app/mods/tchelvis/main.js`. Expected: all present.
- [ ] **Step 4:** Leave uncommitted.
