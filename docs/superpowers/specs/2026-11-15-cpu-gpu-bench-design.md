# CPU vs GPU mini benchmark + GPU backend detection (design)

_Date: 2026-11-15 · Status: approved_

## Intent
The user wants a quick speed comparison of the supervisor model on **CPU vs GPU**, to see whether turning on GPU is worth it.
It ships as a CLI command and as a button in the Herdr mod's **Doctor** tab.

The user's host has an **AMD** GPU (`/opt/rocm/bin` on `PATH`). The current GPU mode only supports NVIDIA
(`:server-cuda` + `--gpus all`). GPU support for AMD/Intel is therefore part of this work.

## Goals / non-goals
- **Goal:** `tchelvis supervisor bench` measures prompt tok/s, generation tok/s, and the wall time of one supervisor evaluation, on CPU and on GPU, then restores the previous state.
- **Goal:** a Doctor card with a button, live progress, the results table, and the last result persisted.
- **Goal:** GPU backend detection (`gpu_backend = auto|cuda|vulkan|rocm`). It is used by the benchmark **and** by the normal `num_gpu` toggle.
- **Non-goal:** benchmarking several models or quantizations, `llama-bench`, and the non-container backends (Ollama/LM Studio/API).
  In those modes `bench` exits with "Benchmark requires the managed llama.cpp container".
- **Anti-goals:**
  - the benchmark leaves the container in a different mode or state than before;
  - a GPU failure aborts the whole run instead of showing the CPU result;
  - the mod freezes while it runs.

## Approaches
| Approach | Trade-off | Decision |
| :-- | :-- | :-- |
| **Sequential, same supervisor container** (recreate with `-ngl 0`, measure, recreate with GPU, measure, restore) | Reuses the existing lifecycle (labels, `/health` wait, runtime fallback). Measures the real supervisor request with llama-server's own `timings`. | **Chosen** |
| Two containers at once (CPU + GPU) | Faster, but loads the model twice and doubles RAM/VRAM during the test, which skews it. | Rejected |
| `llama-bench` | A standard benchmark, but only in the multi-GB `:full` image, and it measures synthetic text, not the supervisor's request. | Rejected |

GPU support for non-NVIDIA cards: the user chose option **(b)**: NVIDIA → CUDA; AMD/Intel → Vulkan. ROCm is available by forcing it in config.

## Design

### 1. GPU backend detection (`src/supervisor/container.rs`)
- `[llm_container]` gets new keys, each `#[serde(default)]`:
  - `gpu_backend = "auto"`;
  - `image_vulkan = "ghcr.io/ggml-org/llama.cpp:server-vulkan"`;
  - `image_rocm = "ghcr.io/ggml-org/llama.cpp:server-rocm"`.
  - `image_gpu` stays the CUDA image.
  - Env override: `TCHELVIS_GPU_BACKEND`.
- **`auto` detection** is pure and injectable: `detect_gpu_backend(exists: impl Fn(&Path)->bool, on_path: impl Fn(&str)->bool, dri_render_nodes: &[PathBuf]) -> GpuBackend`.
  1. `/dev/nvidia0` exists or `nvidia-smi` is on PATH → `Cuda`;
  2. otherwise, at least one `/dev/dri/renderD*` → `Vulkan`;
  3. otherwise → `None`.
- **Run args when `ngl > 0`:**

  | Backend | Image | Docker / Kiri | Podman |
  | :-- | :-- | :-- | :-- |
  | `Cuda` | `image_gpu` | `--gpus all` | `--device nvidia.com/gpu=all` |
  | `Vulkan` | `image_vulkan` | `--device /dev/dri --group-add video` | `--device /dev/dri --group-add keep-groups` |
  | `Rocm` | `image_rocm` | `--device /dev/kfd --device /dev/dri --group-add video` (+ `--security-opt seccomp=unconfined`) | same as Docker |
  | `None` | — | `start` with `ngl > 0` fails: "No GPU detected (gpu_backend=auto)" | same |

- **Recreate rule:** a new label `tchelvis.gpu=<backend>`. The container is recreated when ngl, model, **or** backend differ.
- **Doctor / status:** the snapshot gains `gpu_backend` (the effective one) and `gpu_detail` (e.g. `vulkan · /dev/dri/renderD128`), shown in the Doctor's supervisor row.

### 2. `tchelvis supervisor bench [--runs N] [--json] [--last]`
- **Preconditions:**
  - container mode is active (else the error above);
  - a model resolves (same rules as `load`);
  - no other `bench` is running (lock file `~/.local/state/tchelvis/bench.lock`).
- **Remember the prior state:** container `Running`/`Stopped`/`Missing`, plus the current ngl and backend.
- **For each mode (`cpu` = ngl 0, then `gpu` = `gpu_layers` with the effective backend):**
  1. `start` (recreate) and wait for `/health`. Timeout 300 s, because the first GPU run may pull the image.
  2. 1 warm-up request (not counted), then `--runs` (default 3) measured requests.
  3. Each request is the supervisor's real evaluation request (`build_openai_chat_request_body`) with a **fixed sample input**: ~20 lines of agent output ending in `Approve Bash: ls /tmp | head -5 ? (y/n)`, defined once as a const.
  4. Read the response's `timings.prompt_per_second`, `timings.predicted_per_second`, and the client wall time.
  5. Report the **median** of each.
  - If the GPU mode fails at any point (no GPU, start error, health timeout), record `error` for GPU and continue to the restore step.
- **Restore** the prior state:
  - previously `Running` → recreate with the prior ngl/backend;
  - previously `Stopped` or `Missing` → stop the container.
  - Restore happens on every exit path except a hard kill.
- **Progress:** the bench writes `~/.local/state/tchelvis/bench_progress.json` (`{phase, mode, run, runs, started_at}`) atomically at each step, and prints the same steps to stderr.
- **Result:** `~/.local/state/tchelvis/bench_last.json`:
  ```json
  { "at": 1794700000, "model": "lfm3:1.7b", "quant": "Q4_K_M", "runs": 3,
    "cpu": { "prompt_tps": 180.2, "gen_tps": 21.4, "eval_secs": 4.8 },
    "gpu": { "backend": "vulkan", "prompt_tps": 2400.0, "gen_tps": 95.1, "eval_secs": 1.1 } | { "backend": "none", "error": "No GPU detected" },
    "speedup_gen": 4.4 }
  ```
  - `--json` prints this.
  - Without `--json`, it prints a table and `GPU ≈ 4.4× faster at generation` (or `GPU unavailable: <error>`).
  - `--last` prints the stored result without running.

### 3. Herdr mod (Doctor tab)
- A **"CPU vs GPU benchmark"** card in the Doctor view.
  - **Idle:** the last result from `bench --last --json` (model, date, table, speedup line), or "No benchmark yet", plus a **Run benchmark** button.
  - **Running:** `exec("tchelvis", ["supervisor","bench","--json"], {timeout: 900000})` with the button disabled. Poll `bench_progress.json` every 1 s via `exec("cat", …)` or a small `bench --progress --json` subcommand (builder's choice) and show e.g. `Measuring GPU (vulkan) 2/3…` with elapsed seconds.
  - **Done:** render the table, with the GPU column showing the backend name and the speedup line. On error, show stderr.
- Note under the button: "Takes 1–5 min; the first GPU run may download the GPU image."

## Left to the builder
- How the mod reads progress (`cat` vs a subcommand).
- The exact wording of the sample input lines.
- Table layout details.

## Testing
- **Unit tests:**
  - `detect_gpu_backend`: nvidia device → Cuda; only renderD → Vulkan; nothing → None; forced config value wins;
  - run args per backend × runtime (Cuda/Vulkan/Rocm × docker/podman);
  - the recreate-on-`tchelvis.gpu`-label change (FakeCli);
  - median and speedup math;
  - parsing `timings` from a llama-server response fixture;
  - restore planning (prior Running/Stopped/Missing → actions);
  - GPU-failure result shape.
- **Manual in `ai-box`** (no GPU): `bench` with the SmolLM2 model shows the CPU row, `GPU unavailable: No GPU detected`, and the container restored to its prior state.
- **Manual on the host:** the AMD card through Vulkan.
