use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::config::SupervisorConfig;
use super::container::{self, ContainerState, GpuBackend};
use super::models;
use super::ollama::OllamaClient;
use super::state::{unix_now_secs, AgentDurableState};

/// Fixed ~20-line sample agent output used for reproducible supervisor benchmarks.
pub const SAMPLE_OUTPUT: &str = "\
$ cargo test --quiet
   Compiling tchelvis v0.1.0 (/workspace/cockpit)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 4.12s
     Running unittests src/lib.rs (target/debug/deps/tchelvis-123456)
running 42 tests
..........................................
test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s

Checking workspace status...
Modified: src/supervisor/container.rs
Modified: src/supervisor/mod.rs
Inspecting temporary artifacts in /tmp before cleanup.
Listing recent files:
- /tmp/build-check.log
- /tmp/bench-sample.txt
- /tmp/test-output.json
Ready for final verification command.
Waiting for operator confirmation.
Approve Bash: ls /tmp | head -5 ? (y/n)";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModeResult {
    pub backend: String,
    pub prompt_tps: Option<f64>,
    pub gen_tps: Option<f64>,
    pub eval_secs: Option<f64>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BenchResult {
    pub at: u64,
    pub model: String,
    pub quant: Option<String>,
    pub runs: u32,
    pub cpu: ModeResult,
    pub gpu: ModeResult,
    pub speedup_gen: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BenchProgress {
    pub phase: String,
    pub mode: String,
    pub run: u32,
    pub runs: u32,
    pub started_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAction {
    Recreate { ngl: u32, gpu: GpuBackend },
    Stop,
}

pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn bench_state_dir() -> PathBuf {
    super::config::home_or_fallback_dir()
        .join(".local")
        .join("state")
        .join("tchelvis")
}

pub fn bench_lock_path() -> PathBuf {
    bench_state_dir().join("bench.lock")
}

pub fn bench_progress_path() -> PathBuf {
    bench_state_dir().join("bench_progress.json")
}

pub fn bench_last_path() -> PathBuf {
    bench_state_dir().join("bench_last.json")
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, data + "\n")?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Parses `(prompt_per_second, predicted_per_second)` from a `llama-server` response `timings` object.
pub fn parse_timings(resp: &Value) -> (Option<f64>, Option<f64>) {
    let Some(timings) = resp.get("timings") else {
        return (None, None);
    };
    let prompt = timings
        .get("prompt_per_second")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite());
    let gen = timings
        .get("predicted_per_second")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite());
    (prompt, gen)
}

/// Computes the median of `values`, returning `None` when empty.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[mid])
    } else {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    }
}

/// Computes `gpu_gen / cpu_gen` rounded to 1 decimal place.
pub fn speedup(cpu_gen: Option<f64>, gpu_gen: Option<f64>) -> Option<f64> {
    let (c, g) = (cpu_gen?, gpu_gen?);
    if !c.is_finite() || !g.is_finite() || c <= 0.0 {
        return None;
    }
    Some(((g / c) * 10.0).round() / 10.0)
}

/// Plans how to restore the container after the benchmark finishes or fails.
pub fn restore_plan(
    prior_state: ContainerState,
    prior_ngl: Option<u32>,
    prior_gpu: Option<GpuBackend>,
) -> RestoreAction {
    match prior_state {
        ContainerState::Running => RestoreAction::Recreate {
            ngl: prior_ngl.unwrap_or(0),
            gpu: prior_gpu.unwrap_or(GpuBackend::None),
        },
        ContainerState::Stopped | ContainerState::Missing => RestoreAction::Stop,
    }
}

fn system_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Acquires an exclusive benchmark lock file; refuses if a live PID owns it, overwrites stale locks.
pub fn acquire_lock(path: &Path, pid_alive: impl Fn(i32) -> bool) -> Result<LockGuard> {
    if let Ok(contents) = fs::read_to_string(path) {
        if let Ok(pid) = contents.trim().parse::<i32>() {
            if pid > 0 && pid_alive(pid) {
                anyhow::bail!("A benchmark is already running");
            }
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("lock.tmp");
    fs::write(&tmp, format!("{}\n", std::process::id()))?;
    fs::rename(&tmp, path)?;
    Ok(LockGuard {
        path: path.to_path_buf(),
    })
}

struct RestoreGuard<F: FnMut()> {
    action: Option<F>,
}

impl<F: FnMut()> RestoreGuard<F> {
    fn run_now(&mut self) {
        if let Some(mut f) = self.action.take() {
            f();
        }
    }
}

impl<F: FnMut()> Drop for RestoreGuard<F> {
    fn drop(&mut self) {
        self.run_now();
    }
}

/// Core benchmark state machine, abstracted over `measure_mode` and `restore` for testing.
#[allow(clippy::too_many_arguments)]
pub fn run_bench_core<M, R, P>(
    model: String,
    quant: Option<String>,
    runs: u32,
    prior_state: ContainerState,
    prior_ngl: Option<u32>,
    prior_gpu: Option<GpuBackend>,
    gpu_layers: u32,
    gpu_backend: GpuBackend,
    started_at: u64,
    mut measure_mode: M,
    restore: R,
    on_progress: P,
) -> Result<BenchResult>
where
    M: FnMut(&str, u32, GpuBackend, &mut dyn FnMut(BenchProgress)) -> Result<ModeResult>,
    R: FnMut(RestoreAction),
    P: FnMut(&BenchProgress),
{
    let plan = restore_plan(prior_state, prior_ngl, prior_gpu);
    let restore_cell = RefCell::new(restore);
    let progress_cell = RefCell::new(on_progress);

    let mut emit = |p: BenchProgress| {
        (progress_cell.borrow_mut())(&p);
    };

    let mut guard = RestoreGuard {
        action: Some(|| {
            (restore_cell.borrow_mut())(plan);
        }),
    };

    let cpu = measure_mode("cpu", 0, GpuBackend::None, &mut emit)?;

    let gpu = match measure_mode("gpu", gpu_layers, gpu_backend, &mut emit) {
        Ok(res) => res,
        Err(e) => ModeResult {
            backend: gpu_backend.as_str().to_string(),
            prompt_tps: None,
            gen_tps: None,
            eval_secs: None,
            error: Some(format!("{e:#}")),
        },
    };

    emit(BenchProgress {
        phase: "restoring".to_string(),
        mode: "gpu".to_string(),
        run: runs,
        runs,
        started_at,
    });
    guard.run_now();

    emit(BenchProgress {
        phase: "done".to_string(),
        mode: "gpu".to_string(),
        run: runs,
        runs,
        started_at,
    });

    let speedup_gen = speedup(cpu.gen_tps, gpu.gen_tps);
    Ok(BenchResult {
        at: unix_now_secs(),
        model,
        quant,
        runs,
        cpu,
        gpu,
        speedup_gen,
    })
}

/// Runs the CPU vs GPU supervisor benchmark on the managed `llama.cpp` container.
pub fn run_bench(runs: u32, mut on_progress: impl FnMut(&BenchProgress)) -> Result<BenchResult> {
    let config = SupervisorConfig::load();
    if !config.container_mode_active() {
        anyhow::bail!("Benchmark requires the managed llama.cpp container");
    }
    let Some(rt) = config.detected_container_runtime() else {
        anyhow::bail!("Benchmark requires the managed llama.cpp container");
    };

    let ollama_roots = container::default_ollama_roots();
    let model_src = container::resolve_model_source(
        &config.llm_container,
        &config.ollama_model,
        &ollama_roots,
    )?;
    let eff = models::effective_model(&config, &ollama_roots);
    let model_id = eff.as_ref().map(|m| m.id.clone()).unwrap_or_default();
    let model_name = eff
        .as_ref()
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| config.ollama_model.clone());
    let quant = eff
        .as_ref()
        .and_then(|m| m.path.as_deref())
        .and_then(|p| super::gguf::read_gguf_info_path(p).ok())
        .and_then(|info| info.file_type.and_then(super::gguf::file_type_label))
        .map(str::to_string);

    let _lock = acquire_lock(&bench_lock_path(), system_pid_alive)?;

    let mgr = container::LlmContainer::new(
        rt,
        config.llm_container.clone(),
        container::SystemContainerCli,
    );
    let prior_state = mgr.state()?;
    let prior_ngl = mgr.container_ngl();
    let prior_gpu = mgr.container_gpu().and_then(|s| GpuBackend::parse(&s));
    let (gpu_backend, _) = container::resolve_gpu_backend(&config.llm_container);
    let host = config.effective_primary_host();
    let gpu_layers = config.gpu_layers.max(1);
    let started_at = unix_now_secs();
    let progress_path = bench_progress_path();

    let client = OllamaClient::new(config.clone());
    let sample_state = AgentDurableState::new("bench-agent", "bench", config.max_auto_replies, 0);
    let request_body = client.build_openai_chat_request_body(&sample_state, SAMPLE_OUTPUT);
    let http = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();
    let completions_url = format!("{}/v1/chat/completions", host.trim_end_matches('/'));

    let result = run_bench_core(
        model_name,
        quant,
        runs,
        prior_state,
        prior_ngl,
        prior_gpu,
        gpu_layers,
        gpu_backend,
        started_at,
        |mode, ngl, gpu, emit| {
            emit(BenchProgress {
                phase: "loading".to_string(),
                mode: mode.to_string(),
                run: 0,
                runs,
                started_at,
            });
            // The bench recreates the container on purpose: keep the fallback health trigger quiet.
            super::fallback::mark_primary_starting(
                &super::fallback::fallback_state_path(),
                super::state::unix_now_secs(),
            );
            mgr.start(ngl, &model_src, &model_id, gpu)?;
            container::wait_healthy(&host, Duration::from_secs(300))?;

            emit(BenchProgress {
                phase: "warmup".to_string(),
                mode: mode.to_string(),
                run: 0,
                runs,
                started_at,
            });
            http.post(&completions_url)
                .set("Content-Type", "application/json")
                .send_json(request_body.clone())
                .with_context(|| format!("Warm-up request failed ({mode})"))?;

            let mut prompts = Vec::with_capacity(runs as usize);
            let mut gens = Vec::with_capacity(runs as usize);
            let mut evals = Vec::with_capacity(runs as usize);

            for r in 1..=runs {
                emit(BenchProgress {
                    phase: "measuring".to_string(),
                    mode: mode.to_string(),
                    run: r,
                    runs,
                    started_at,
                });
                let t0 = Instant::now();
                let resp = http
                    .post(&completions_url)
                    .set("Content-Type", "application/json")
                    .send_json(request_body.clone())
                    .with_context(|| format!("Bench request {r}/{runs} failed ({mode})"))?;
                let val: Value = resp.into_json()?;
                let wall = t0.elapsed().as_secs_f64();
                let (p_tps, g_tps) = parse_timings(&val);
                if let Some(p) = p_tps {
                    prompts.push(p);
                }
                if let Some(g) = g_tps {
                    gens.push(g);
                }
                evals.push(wall);
            }

            let backend_label = if mode == "cpu" {
                "cpu".to_string()
            } else {
                gpu.as_str().to_string()
            };

            Ok(ModeResult {
                backend: backend_label,
                prompt_tps: median(&prompts),
                gen_tps: median(&gens),
                eval_secs: median(&evals),
                error: None,
            })
        },
        |action| match action {
            RestoreAction::Recreate { ngl, gpu } => {
                let fb_path = super::fallback::fallback_state_path();
                super::fallback::mark_primary_starting(&fb_path, super::state::unix_now_secs());
                let _ = mgr.start(ngl, &model_src, &model_id, gpu);
                let _ = container::wait_healthy(&host, Duration::from_secs(300));
                // Back to the user's prior running state: health probes count again from here.
                let _ =
                    super::fallback::update_state(&fb_path, super::fallback::primary_start_settled);
            }
            RestoreAction::Stop => {
                // The container was stopped before the bench. Only end the start grace: an
                // intended-but-crashed primary keeps its active fallback (return is the user's
                // call), and a user-stopped primary already has primary_intended = false.
                let _ = mgr.stop();
                let _ = super::fallback::update_state(
                    &super::fallback::fallback_state_path(),
                    super::fallback::primary_start_settled,
                );
            }
        },
        |p| {
            let _ = write_json_atomic(&progress_path, p);
            on_progress(p);
        },
    )?;

    write_json_atomic(&bench_last_path(), &result)?;
    Ok(result)
}

/// Loads the last saved benchmark result from `bench_last.json`.
pub fn load_last_bench() -> Result<BenchResult> {
    let path = bench_last_path();
    if !path.exists() {
        anyhow::bail!("No benchmark yet");
    }
    let raw = fs::read_to_string(&path)?;
    let parsed: BenchResult = serde_json::from_str(&raw)?;
    Ok(parsed)
}

/// Reads the current progress JSON from `bench_progress.json`, or `"{}"` if absent.
pub fn load_bench_progress() -> String {
    let path = bench_progress_path();
    fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string())
}

fn fmt_opt(val: Option<f64>, decimals: usize) -> String {
    match val {
        Some(v) => format!("{v:.decimals$}"),
        None => "—".to_string(),
    }
}

/// Formats a `BenchResult` as a human-readable table and summary line.
pub fn format_bench_table(res: &BenchResult) -> String {
    let mut lines = Vec::new();
    let quant_str = res
        .quant
        .as_deref()
        .map(|q| format!(" ({q})"))
        .unwrap_or_default();
    lines.push(format!(
        "Model: {}{} · {} runs",
        res.model, quant_str, res.runs
    ));
    lines.push(format!(
        "{:<18} {:>12} {:>12} {:>10}",
        "Mode", "Prompt tok/s", "Gen tok/s", "Eval (s)"
    ));
    lines.push("-".repeat(56));

    let format_row = |label: &str, m: &ModeResult| -> String {
        if let Some(ref err) = m.error {
            format!("{label:<18} {err}")
        } else {
            format!(
                "{:<18} {:>12} {:>12} {:>10}",
                label,
                fmt_opt(m.prompt_tps, 1),
                fmt_opt(m.gen_tps, 1),
                fmt_opt(m.eval_secs, 2),
            )
        }
    };

    lines.push(format_row("CPU", &res.cpu));
    let gpu_label = format!("GPU ({})", res.gpu.backend);
    lines.push(format_row(&gpu_label, &res.gpu));

    if let Some(ref err) = res.gpu.error {
        lines.push(format!("GPU unavailable: {err}"));
    } else if let Some(sp) = res.speedup_gen {
        lines.push(format!("GPU ≈ {sp:.1}× faster at generation"));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_timings_reads_values() {
        let resp = json!({"timings":{"prompt_per_second":180.5,"predicted_per_second":21.25}});
        assert_eq!(parse_timings(&resp), (Some(180.5), Some(21.25)));
    }

    #[test]
    fn parse_timings_missing_is_none() {
        let resp = json!({"choices":[{"message":{"content":"{}"}}]});
        assert_eq!(parse_timings(&resp), (None, None));
    }

    #[test]
    fn median_odd_even_empty() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn speedup_math() {
        assert_eq!(speedup(Some(21.4), Some(95.1)), Some(4.4));
        assert_eq!(speedup(None, Some(95.1)), None);
        assert_eq!(speedup(Some(21.4), None), None);
        assert_eq!(speedup(Some(0.0), Some(95.1)), None);
    }

    #[test]
    fn restore_plan_cases() {
        assert_eq!(
            restore_plan(ContainerState::Running, Some(99), Some(GpuBackend::Vulkan)),
            RestoreAction::Recreate {
                ngl: 99,
                gpu: GpuBackend::Vulkan,
            }
        );
        assert_eq!(
            restore_plan(ContainerState::Stopped, Some(99), Some(GpuBackend::Vulkan)),
            RestoreAction::Stop
        );
        assert_eq!(
            restore_plan(ContainerState::Missing, None, None),
            RestoreAction::Stop
        );
    }

    #[test]
    fn lock_refuses_live_pid_and_ignores_stale() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("bench.lock");

        fs::write(&lock_path, "4242\n").unwrap();
        let err = match acquire_lock(&lock_path, |pid| pid == 4242) {
            Ok(_) => panic!("expected live pid lock to fail"),
            Err(e) => e,
        };
        assert_eq!(err.to_string(), "A benchmark is already running");

        // Stale pid -> overwritten and removed on drop
        {
            let _guard = acquire_lock(&lock_path, |_| false).unwrap();
            assert!(lock_path.exists());
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn gpu_failure_still_restores_and_reports_cpu() {
        let mut restored: Vec<RestoreAction> = Vec::new();
        let mut phases: Vec<(String, String)> = Vec::new();

        let res = run_bench_core(
            "lfm3:1.7b".to_string(),
            Some("Q4_K_M".to_string()),
            3,
            ContainerState::Stopped,
            None,
            None,
            99,
            GpuBackend::Vulkan,
            1000,
            |mode, _ngl, _gpu, _emit| {
                if mode == "cpu" {
                    Ok(ModeResult {
                        backend: "cpu".to_string(),
                        prompt_tps: Some(180.0),
                        gen_tps: Some(21.4),
                        eval_secs: Some(4.8),
                        error: None,
                    })
                } else {
                    anyhow::bail!("GPU image pull failed")
                }
            },
            |action| restored.push(action),
            |p| phases.push((p.phase.clone(), p.mode.clone())),
        )
        .unwrap();

        assert_eq!(res.cpu.gen_tps, Some(21.4));
        assert!(res
            .gpu
            .error
            .as_deref()
            .unwrap()
            .contains("GPU image pull failed"));
        assert_eq!(res.speedup_gen, None);
        assert_eq!(restored, vec![RestoreAction::Stop]);
        assert_eq!(phases.last().unwrap().0, "done");
    }
}
