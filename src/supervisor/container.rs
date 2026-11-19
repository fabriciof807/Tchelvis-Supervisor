use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Supported container runtimes in auto-detection preference order (`kiri` -> `podman` -> `docker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerRuntime {
    Kiri,
    Podman,
    Docker,
}

impl ContainerRuntime {
    /// Executable name for this runtime.
    pub fn bin(&self) -> &'static str {
        match self {
            Self::Kiri => "kiri",
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }

    /// Canonical lowercase identifier for snapshots and logs.
    pub fn as_str(&self) -> &'static str {
        self.bin()
    }
}

/// GPU backend for the managed `llama.cpp` container.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuBackend {
    Cuda,
    Vulkan,
    Rocm,
    None,
}

impl GpuBackend {
    /// Parses a GPU backend name (`"cuda"`, `"vulkan"`, `"rocm"`, `"none"`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cuda" => Some(Self::Cuda),
            "vulkan" => Some(Self::Vulkan),
            "rocm" => Some(Self::Rocm),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Canonical lowercase identifier for labels, snapshots, and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
            Self::Rocm => "rocm",
            Self::None => "none",
        }
    }
}

/// Configuration for the managed `llama.cpp` (`llama-server`) container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmContainerConfig {
    /// Container lifecycle mode: `"auto"` | `"on"` | `"off"`.
    pub mode: String,
    /// Runtime override: `"auto"` | `"kiri"` | `"podman"` | `"docker"`.
    pub runtime: String,
    /// Container name.
    pub name: String,
    /// CPU container image.
    pub image: String,
    /// GPU (CUDA) container image.
    pub image_gpu: String,
    /// GPU backend override: `"auto"` | `"cuda"` | `"vulkan"` | `"rocm"` | `"none"`.
    pub gpu_backend: String,
    /// GPU (Vulkan) container image.
    pub image_vulkan: String,
    /// GPU (ROCm) container image.
    pub image_rocm: String,
    /// Host loopback port mapped to container port `8080`.
    pub port: u16,
    /// Context window size (`-c`).
    pub ctx_size: u32,
    /// Optional explicit host GGUF file path.
    pub model_path: Option<PathBuf>,
    /// Optional HuggingFace repo (`-hf`) to download/run.
    pub hf_repo: Option<String>,
    /// Extra CLI arguments appended to `llama-server`.
    pub extra_args: Vec<String>,
    /// Selected model identifier (`ollama:<name>`, `file:<path>`, or `hf:<repo>`).
    pub model_id: Option<String>,
    /// Maximum parameter filter for model listing (`2b` | `4b` | `8b` | `all`).
    pub model_filter: String,
    /// Additional directories to scan for local `.gguf` files.
    pub model_dirs: Vec<PathBuf>,
    /// Reserved for M1b; not read.
    pub fallback_model_id: Option<String>,
    /// User-hidden model ids omitted from `model list` unless `--all` is passed.
    pub hidden_models: Vec<String>,
}

impl Default for LlmContainerConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            runtime: "auto".to_string(),
            name: "tchelvis-supervisor-llm".to_string(),
            image: "ghcr.io/ggml-org/llama.cpp:server".to_string(),
            image_gpu: "ghcr.io/ggml-org/llama.cpp:server-cuda".to_string(),
            gpu_backend: "auto".to_string(),
            image_vulkan: "ghcr.io/ggml-org/llama.cpp:server-vulkan".to_string(),
            image_rocm: "ghcr.io/ggml-org/llama.cpp:server-rocm".to_string(),
            port: 11435,
            ctx_size: 4096,
            model_path: None,
            hf_repo: None,
            extra_args: Vec::new(),
            model_id: None,
            model_filter: "4b".to_string(),
            model_dirs: Vec::new(),
            fallback_model_id: None,
            hidden_models: Vec::new(),
        }
    }
}

/// Checks whether `bin` is present as an executable file on `PATH`.
pub fn binary_on_path(bin: &str) -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return true;
        }
    }
    false
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Detects the container runtime according to `override_` (`"auto"`, `"kiri"`, `"podman"`, `"docker"`).
pub fn detect_runtime(
    override_: &str,
    is_on_path: impl Fn(&str) -> bool,
) -> Option<ContainerRuntime> {
    detect_runtime_with_pref(override_, None, is_on_path)
}

/// Detects the preferred container runtime, prioritizing `remembered` in `auto` mode.
pub fn detect_runtime_with_pref(
    override_: &str,
    remembered: Option<ContainerRuntime>,
    is_on_path: impl Fn(&str) -> bool,
) -> Option<ContainerRuntime> {
    detect_runtime_candidates(override_, remembered, is_on_path)
        .into_iter()
        .next()
}

/// Returns ordered candidate container runtimes for `override_`.
/// Explicit overrides (`kiri`/`podman`/`docker`) never fall back to other runtimes.
pub fn detect_runtime_candidates(
    override_: &str,
    remembered: Option<ContainerRuntime>,
    is_on_path: impl Fn(&str) -> bool,
) -> Vec<ContainerRuntime> {
    match override_.trim().to_ascii_lowercase().as_str() {
        "kiri" => is_on_path("kiri")
            .then_some(ContainerRuntime::Kiri)
            .into_iter()
            .collect(),
        "podman" => is_on_path("podman")
            .then_some(ContainerRuntime::Podman)
            .into_iter()
            .collect(),
        "docker" => is_on_path("docker")
            .then_some(ContainerRuntime::Docker)
            .into_iter()
            .collect(),
        "auto" | "" => {
            let mut order = Vec::with_capacity(4);
            if let Some(rt) = remembered {
                order.push(rt);
            }
            for rt in [
                ContainerRuntime::Kiri,
                ContainerRuntime::Podman,
                ContainerRuntime::Docker,
            ] {
                if !order.contains(&rt) {
                    order.push(rt);
                }
            }
            order
                .into_iter()
                .filter(|rt| is_on_path(rt.bin()))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Reads the remembered container runtime preference (`kiri` / `podman` / `docker`).
pub fn read_runtime_pref(path: &Path) -> Option<ContainerRuntime> {
    match fs::read_to_string(path)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "kiri" => Some(ContainerRuntime::Kiri),
        "podman" => Some(ContainerRuntime::Podman),
        "docker" => Some(ContainerRuntime::Docker),
        _ => None,
    }
}

/// Persists the remembered container runtime atomically.
pub fn write_runtime_pref(path: &Path, rt: ContainerRuntime) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{}\n", rt.bin()))?;
    fs::rename(&tmp, path)
}

/// Lists `/dev/dri/renderD*` device nodes sorted lexicographically.
pub fn list_dri_render_nodes() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("renderD"))
                .unwrap_or(false)
        })
        .collect();
    nodes.sort();
    nodes
}

/// Pure GPU backend auto-detection:
/// 1. `/dev/nvidia0` exists or `nvidia-smi` is on PATH -> `Cuda`
/// 2. Any `/dev/dri/renderD*` node -> `Vulkan`
/// 3. Otherwise -> `None`
pub fn detect_gpu_backend(
    exists: impl Fn(&Path) -> bool,
    on_path: impl Fn(&str) -> bool,
    render_nodes: &[PathBuf],
) -> GpuBackend {
    if exists(Path::new("/dev/nvidia0")) || on_path("nvidia-smi") {
        GpuBackend::Cuda
    } else if !render_nodes.is_empty() {
        GpuBackend::Vulkan
    } else {
        GpuBackend::None
    }
}

/// Resolves the effective GPU backend and a human-readable detail string from `cfg`.
/// Forced values (anything other than `"auto"` / empty) are honored as-is.
pub fn resolve_gpu_backend(cfg: &LlmContainerConfig) -> (GpuBackend, String) {
    let raw = cfg.gpu_backend.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
        let nodes = list_dri_render_nodes();
        let nvidia_dev = Path::new("/dev/nvidia0");
        let backend = detect_gpu_backend(|p| p.exists(), binary_on_path, &nodes);
        let detail = match backend {
            GpuBackend::Cuda => {
                if nvidia_dev.exists() {
                    "cuda · /dev/nvidia0".to_string()
                } else {
                    "cuda · nvidia-smi".to_string()
                }
            }
            GpuBackend::Vulkan => {
                let first = nodes
                    .first()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "/dev/dri".to_string());
                format!("vulkan · {first}")
            }
            GpuBackend::Rocm => "rocm".to_string(),
            GpuBackend::None => "none".to_string(),
        };
        (backend, detail)
    } else if let Some(forced) = GpuBackend::parse(raw) {
        (forced, format!("{} · forced", forced.as_str()))
    } else {
        (GpuBackend::None, format!("none · invalid ({raw})"))
    }
}

/// Attempts `start` + `wait_health` across `candidates` in order, cleaning up failed containers
/// and persisting the first runtime that succeeds.
#[allow(clippy::too_many_arguments)]
pub fn start_with_fallback<C, F, H>(
    candidates: &[ContainerRuntime],
    ngl: u32,
    model: &ModelSource,
    model_id: &str,
    gpu: GpuBackend,
    pref_path: Option<&Path>,
    mut make_mgr: F,
    mut wait_health: H,
) -> Result<ContainerRuntime>
where
    C: ContainerCli,
    F: FnMut(ContainerRuntime) -> LlmContainer<C>,
    H: FnMut() -> Result<()>,
{
    if candidates.is_empty() {
        anyhow::bail!("Nenhum runtime de container encontrado (kiri/podman/docker)");
    }
    let mut errors = Vec::new();
    for &rt in candidates {
        let mgr = make_mgr(rt);
        match mgr
            .start(ngl, model, model_id, gpu)
            .and_then(|()| wait_health())
        {
            Ok(()) => {
                if let Some(p) = pref_path {
                    let _ = write_runtime_pref(p, rt);
                }
                return Ok(rt);
            }
            Err(e) => {
                let _ = mgr.cli.run(
                    rt.bin(),
                    &["rm".to_string(), "-f".to_string(), mgr.config.name.clone()],
                );
                errors.push(format!("{}: {e:#}", rt.bin()));
            }
        }
    }
    anyhow::bail!(
        "Falha ao iniciar o container LLM em todos os runtimes ({})",
        errors.join("; ")
    )
}

/// Output captured from a container CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub status_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Boundary trait abstracting container CLI invocations for deterministic testing.
pub trait ContainerCli: Send + Sync {
    fn run(&self, bin: &str, args: &[String]) -> Result<CmdOutput>;
}

/// Real `std::process::Command` implementation with bounded timeout and stderr capture.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemContainerCli;

impl ContainerCli for SystemContainerCli {
    fn run(&self, bin: &str, args: &[String]) -> Result<CmdOutput> {
        let timeout = if args.first().map(|s| s.as_str()) == Some("run") {
            Duration::from_secs(300)
        } else {
            Duration::from_secs(30)
        };

        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Falha ao executar `{bin}`"))?;

        // Drain both pipes concurrently so a chatty `run` (image pull progress) cannot fill the
        // pipe buffer and block the child until the timeout.
        fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
            std::thread::spawn(move || {
                let mut buf = String::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_string(&mut buf);
                }
                buf
            })
        }
        let stdout_reader = drain(child.stdout.take());
        let stderr_reader = drain(child.stderr.take());

        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Timeout ao executar `{bin} {}`", args.join(" "));
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();

        Ok(CmdOutput {
            status_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }
}

/// Lifecycle state of the managed LLM container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Missing,
    Stopped,
    Running,
}

impl ContainerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Stopped => "stopped",
            Self::Running => "running",
        }
    }
}

/// Parses `<rt> inspect -f '{{.State.Running}}' <name>` output into `ContainerState`.
pub fn parse_inspect_running(out: &CmdOutput) -> ContainerState {
    let lower_err = out.stderr.to_ascii_lowercase();
    let lower_out = out.stdout.to_ascii_lowercase();
    if out.status_code != 0 || lower_err.contains("no such") || lower_out.contains("no such") {
        return ContainerState::Missing;
    }
    match out.stdout.trim() {
        "true" => ContainerState::Running,
        "false" => ContainerState::Stopped,
        _ => ContainerState::Missing,
    }
}

/// Model source mounted or passed to `llama-server`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    File(PathBuf),
    HfRepo(String),
}

/// Default Ollama model store roots searched when auto-resolving a GGUF blob.
pub fn default_ollama_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(custom) = env::var("OLLAMA_MODELS") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            roots.push(PathBuf::from(trimmed));
        }
    }
    if let Ok(home) = env::var("HOME") {
        roots.push(PathBuf::from(home).join(".ollama").join("models"));
    }
    roots.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
    roots
}

/// Default cache directory mounted into `/root/.cache/llama.cpp` for `-hf` downloads.
pub fn default_hf_cache_dir() -> PathBuf {
    super::config::home_or_fallback_dir()
        .join(".cache")
        .join("tchelvis")
        .join("llama.cpp")
}

/// Parsed model selection identifier (`ollama:<name>`, `file:<path>`, `hf:<repo>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelId {
    Ollama(String),
    File(PathBuf),
    Hf(String),
}

impl ModelId {
    /// Parses a model id by splitting on the FIRST `:` only.
    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        let (scheme, rest) = trimmed
            .split_once(':')
            .ok_or_else(|| anyhow!("ID de modelo inválido (esperado prefixo:valor): {s}"))?;
        if rest.is_empty() {
            bail!("ID de modelo inválido (valor vazio): {s}");
        }
        match scheme {
            "ollama" => Ok(Self::Ollama(rest.to_string())),
            "file" => Ok(Self::File(PathBuf::from(rest))),
            "hf" => Ok(Self::Hf(rest.to_string())),
            other => bail!("Esquema de modelo desconhecido '{other}': {s}"),
        }
    }

    /// Formats back into the canonical `<scheme>:<value>` id string.
    pub fn as_id(&self) -> String {
        match self {
            Self::Ollama(n) => format!("ollama:{n}"),
            Self::File(p) => format!("file:{}", p.display()),
            Self::Hf(r) => format!("hf:{r}"),
        }
    }
}

/// Resolves the model source in precedence order:
/// 1. `llm_container.model_id`
/// 2. `llm_container.model_path`
/// 3. `llm_container.hf_repo`
/// 4. Ollama store manifest/blob matching `ollama_model` across `search_roots`
/// 5. Clear pt-BR error.
pub fn resolve_model_source(
    cfg: &LlmContainerConfig,
    ollama_model: &str,
    search_roots: &[PathBuf],
) -> Result<ModelSource> {
    if let Some(ref raw_id) = cfg.model_id {
        let trimmed = raw_id.trim();
        if !trimmed.is_empty() {
            return match ModelId::parse(trimmed)? {
                ModelId::Ollama(name) => resolve_ollama_gguf_blob(&name, search_roots)
                    .map(ModelSource::File)
                    .ok_or_else(|| anyhow!("Modelo selecionado não encontrado: {name}")),
                ModelId::File(path) => {
                    if path.exists() {
                        Ok(ModelSource::File(path))
                    } else {
                        bail!("Modelo selecionado não encontrado: {}", path.display());
                    }
                }
                ModelId::Hf(repo) => Ok(ModelSource::HfRepo(repo)),
            };
        }
    }

    if let Some(ref path) = cfg.model_path {
        if !path.as_os_str().is_empty() {
            return Ok(ModelSource::File(path.clone()));
        }
    }

    if let Some(ref repo) = cfg.hf_repo {
        let trimmed = repo.trim();
        if !trimmed.is_empty() {
            return Ok(ModelSource::HfRepo(trimmed.to_string()));
        }
    }

    if let Some(blob) = resolve_ollama_gguf_blob(ollama_model, search_roots) {
        return Ok(ModelSource::File(blob));
    }

    Err(anyhow!(
        "Modelo GGUF não encontrado: defina llm_container.model_path ou hf_repo"
    ))
}

pub(crate) fn resolve_ollama_gguf_blob(
    ollama_model: &str,
    search_roots: &[PathBuf],
) -> Option<PathBuf> {
    let trimmed = ollama_model.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (name_part, tag) = match trimmed.rsplit_once(':') {
        Some((n, t)) if !n.is_empty() && !t.is_empty() => (n, t),
        _ => (trimmed, "latest"),
    };

    let slash_count = name_part.matches('/').count();
    let rel_manifest = if slash_count >= 2 {
        PathBuf::from("manifests").join(name_part).join(tag)
    } else if slash_count == 1 {
        PathBuf::from("manifests")
            .join("registry.ollama.ai")
            .join(name_part)
            .join(tag)
    } else {
        PathBuf::from("manifests")
            .join("registry.ollama.ai")
            .join("library")
            .join(name_part)
            .join(tag)
    };

    for root in search_roots {
        let manifest_path = root.join(&rel_manifest);
        let Ok(raw) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(layers) = val.get("layers").and_then(|l| l.as_array()) else {
            continue;
        };
        for layer in layers {
            let media_type = layer
                .get("mediaType")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            if media_type == "application/vnd.ollama.image.model" {
                if let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) {
                    let blob_name = digest.replace(':', "-");
                    let blob_path = root.join("blobs").join(blob_name);
                    if blob_path.is_file() {
                        return Some(blob_path);
                    }
                }
            }
        }
    }
    None
}

/// Builds the `<runtime> run ...` argument vector for `llama-server`.
pub fn build_run_args(
    cfg: &LlmContainerConfig,
    runtime: ContainerRuntime,
    ngl: u32,
    model: &ModelSource,
    hf_cache_dir: &Path,
    model_id: &str,
    gpu: GpuBackend,
) -> Vec<String> {
    let effective_gpu = if ngl > 0 { gpu } else { GpuBackend::None };
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        cfg.name.clone(),
        "--label".to_string(),
        format!("tchelvis.ngl={ngl}"),
        "--label".to_string(),
        format!("tchelvis.model_id={model_id}"),
        "--label".to_string(),
        format!("tchelvis.gpu={}", effective_gpu.as_str()),
        "-p".to_string(),
        format!("127.0.0.1:{}:8080", cfg.port),
    ];

    if ngl > 0 {
        match effective_gpu {
            GpuBackend::Cuda => match runtime {
                ContainerRuntime::Docker | ContainerRuntime::Kiri => {
                    args.push("--gpus".to_string());
                    args.push("all".to_string());
                }
                ContainerRuntime::Podman => {
                    args.push("--device".to_string());
                    args.push("nvidia.com/gpu=all".to_string());
                }
            },
            GpuBackend::Vulkan => {
                args.push("--device".to_string());
                args.push("/dev/dri".to_string());
                args.push("--group-add".to_string());
                match runtime {
                    ContainerRuntime::Docker | ContainerRuntime::Kiri => {
                        args.push("video".to_string());
                    }
                    ContainerRuntime::Podman => {
                        args.push("keep-groups".to_string());
                    }
                }
            }
            GpuBackend::Rocm => {
                args.extend([
                    "--device".to_string(),
                    "/dev/kfd".to_string(),
                    "--device".to_string(),
                    "/dev/dri".to_string(),
                    "--group-add".to_string(),
                    "video".to_string(),
                    "--security-opt".to_string(),
                    "seccomp=unconfined".to_string(),
                ]);
            }
            GpuBackend::None => {}
        }
    }

    let mut model_args = Vec::new();
    match model {
        ModelSource::File(path) => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let file_name = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            args.push("-v".to_string());
            args.push(format!("{}:/models:ro", parent.display()));
            model_args.push("-m".to_string());
            model_args.push(format!("/models/{file_name}"));
        }
        ModelSource::HfRepo(repo) => {
            args.push("-v".to_string());
            args.push(format!("{}:/root/.cache/llama.cpp", hf_cache_dir.display()));
            // Pin the download cache to the mounted dir so recreating the container reuses it.
            args.push("-e".to_string());
            args.push("LLAMA_CACHE=/root/.cache/llama.cpp".to_string());
            model_args.push("-hf".to_string());
            model_args.push(repo.clone());
        }
    }

    let image = if ngl > 0 {
        match effective_gpu {
            GpuBackend::Cuda => cfg.image_gpu.clone(),
            GpuBackend::Vulkan => cfg.image_vulkan.clone(),
            GpuBackend::Rocm => cfg.image_rocm.clone(),
            GpuBackend::None => cfg.image.clone(),
        }
    } else {
        cfg.image.clone()
    };
    args.push(image);
    args.extend(model_args);
    args.extend([
        "--host".to_string(),
        "0.0.0.0".to_string(),
        "--port".to_string(),
        "8080".to_string(),
        "-ngl".to_string(),
        ngl.to_string(),
        "-c".to_string(),
        cfg.ctx_size.to_string(),
    ]);
    let has_parallel = cfg
        .extra_args
        .iter()
        .any(|a| a == "-np" || a == "--parallel" || a.starts_with("--parallel="));
    if !has_parallel {
        args.push("-np".to_string());
        args.push("1".to_string());
    }
    args.extend(cfg.extra_args.iter().cloned());
    args
}

/// Parses the left side of `<rt> stats --no-stream --format '{{.MemUsage}}'` into bytes.
pub fn parse_mem_usage(s: &str) -> Option<u64> {
    let left = s.split('/').next()?.trim();
    if left.is_empty() {
        return None;
    }

    let units: [(&str, f64); 8] = [
        ("gib", 1024.0 * 1024.0 * 1024.0),
        ("mib", 1024.0 * 1024.0),
        ("kib", 1024.0),
        ("gb", 1_000_000_000.0),
        ("mb", 1_000_000.0),
        ("kb", 1_000.0),
        ("b", 1.0),
        ("i", 1.0), // fallback unused
    ];

    let lower = left.to_ascii_lowercase();
    for (suffix, mult) in units {
        if suffix == "i" {
            continue;
        }
        if let Some(num_str) = lower.strip_suffix(suffix) {
            let num: f64 = num_str.trim().parse().ok()?;
            if num.is_finite() && num >= 0.0 {
                return Some((num * mult).round() as u64);
            }
            return None;
        }
    }
    None
}

/// Polls `probe(host)` until it returns `true` or `timeout` elapses, using injected sleep & clock.
pub fn wait_healthy_with<F, S, N>(
    host: &str,
    timeout: Duration,
    interval: Duration,
    mut probe: F,
    mut sleep_fn: S,
    mut now_fn: N,
) -> Result<()>
where
    F: FnMut(&str) -> bool,
    S: FnMut(Duration),
    N: FnMut() -> Duration,
{
    let start = now_fn();
    loop {
        if probe(host) {
            return Ok(());
        }
        if now_fn().saturating_sub(start) >= timeout {
            bail!(
                "Container llama.cpp não respondeu em {host}/health após {}s",
                timeout.as_secs()
            );
        }
        sleep_fn(interval);
    }
}

/// Probes `GET {host}/health` expecting HTTP 200.
pub fn probe_llama_server_health(host: &str) -> bool {
    let url = format!("{}/health", host.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(1200))
        .build();
    matches!(agent.get(&url).call(), Ok(resp) if resp.status() == 200)
}

/// Waits for the real `llama-server` HTTP `/health` endpoint to return 200.
pub fn wait_healthy(host: &str, timeout: Duration) -> Result<()> {
    let epoch = Instant::now();
    wait_healthy_with(
        host,
        timeout,
        Duration::from_millis(500),
        probe_llama_server_health,
        std::thread::sleep,
        || epoch.elapsed(),
    )
}

/// Stateful manager around the named `llama.cpp` container.
pub struct LlmContainer<C: ContainerCli> {
    pub runtime: ContainerRuntime,
    pub config: LlmContainerConfig,
    pub cli: C,
    pub hf_cache_dir: PathBuf,
}

impl<C: ContainerCli> LlmContainer<C> {
    pub fn new(runtime: ContainerRuntime, config: LlmContainerConfig, cli: C) -> Self {
        Self {
            runtime,
            config,
            cli,
            hf_cache_dir: default_hf_cache_dir(),
        }
    }

    pub fn with_hf_cache_dir(mut self, dir: PathBuf) -> Self {
        self.hf_cache_dir = dir;
        self
    }

    fn run_checked(&self, args: &[String]) -> Result<CmdOutput> {
        let out = self.cli.run(self.runtime.bin(), args)?;
        if out.status_code != 0 {
            let err_msg = out.stderr.trim();
            let out_msg = out.stdout.trim();
            let detail = if !err_msg.is_empty() {
                err_msg
            } else {
                out_msg
            };
            bail!(
                "Comando `{} {}` falhou (exit {}): {}",
                self.runtime.bin(),
                args.join(" "),
                out.status_code,
                detail
            );
        }
        Ok(out)
    }

    /// Queries the container's current lifecycle state.
    pub fn state(&self) -> Result<ContainerState> {
        let args = vec![
            "inspect".to_string(),
            "-f".to_string(),
            "{{.State.Running}}".to_string(),
            self.config.name.clone(),
        ];
        let out = self.cli.run(self.runtime.bin(), &args)?;
        Ok(parse_inspect_running(&out))
    }

    /// Reads the `tchelvis.ngl` label from an existing container.
    pub fn container_ngl(&self) -> Option<u32> {
        let args = vec![
            "inspect".to_string(),
            "-f".to_string(),
            "{{index .Config.Labels \"tchelvis.ngl\"}}".to_string(),
            self.config.name.clone(),
        ];
        let out = self.cli.run(self.runtime.bin(), &args).ok()?;
        if out.status_code != 0 {
            return None;
        }
        out.stdout.trim().parse::<u32>().ok()
    }

    /// Reads the `tchelvis.model_id` label from an existing container.
    pub fn container_model_id(&self) -> Option<String> {
        let args = vec![
            "inspect".to_string(),
            "-f".to_string(),
            "{{index .Config.Labels \"tchelvis.model_id\"}}".to_string(),
            self.config.name.clone(),
        ];
        let out = self.cli.run(self.runtime.bin(), &args).ok()?;
        if out.status_code != 0 {
            return None;
        }
        let val = out.stdout.trim();
        if val.is_empty() || val == "<no value>" {
            None
        } else {
            Some(val.to_string())
        }
    }

    /// Reads the `tchelvis.gpu` label from an existing container.
    pub fn container_gpu(&self) -> Option<String> {
        let args = vec![
            "inspect".to_string(),
            "-f".to_string(),
            "{{index .Config.Labels \"tchelvis.gpu\"}}".to_string(),
            self.config.name.clone(),
        ];
        let out = self.cli.run(self.runtime.bin(), &args).ok()?;
        if out.status_code != 0 {
            return None;
        }
        let val = out.stdout.trim();
        if val.is_empty() || val == "<no value>" {
            None
        } else {
            Some(val.to_string())
        }
    }

    /// Starts (or recreates when `ngl`, `model_id`, or `gpu` changed) the `llama.cpp` container.
    pub fn start(
        &self,
        ngl: u32,
        model: &ModelSource,
        model_id: &str,
        gpu: GpuBackend,
    ) -> Result<()> {
        if ngl > 0 && gpu == GpuBackend::None {
            bail!("No GPU detected (gpu_backend=auto)");
        }
        let effective_gpu = if ngl > 0 { gpu } else { GpuBackend::None };

        if matches!(model, ModelSource::HfRepo(_)) {
            // Pre-create the cache so the runtime does not create the bind mount as root.
            fs::create_dir_all(&self.hf_cache_dir)
                .with_context(|| format!("Falha ao criar cache {}", self.hf_cache_dir.display()))?;
        }
        match self.state()? {
            ContainerState::Missing => {
                let run_args = build_run_args(
                    &self.config,
                    self.runtime,
                    ngl,
                    model,
                    &self.hf_cache_dir,
                    model_id,
                    effective_gpu,
                );
                self.run_checked(&run_args)?;
            }
            ContainerState::Stopped => {
                let matches = self.container_ngl() == Some(ngl)
                    && self.container_model_id().as_deref() == Some(model_id)
                    && self.container_gpu().as_deref() == Some(effective_gpu.as_str());
                if matches {
                    self.run_checked(&["start".to_string(), self.config.name.clone()])?;
                } else {
                    let _ = self.cli.run(
                        self.runtime.bin(),
                        &["rm".to_string(), "-f".to_string(), self.config.name.clone()],
                    );
                    let run_args = build_run_args(
                        &self.config,
                        self.runtime,
                        ngl,
                        model,
                        &self.hf_cache_dir,
                        model_id,
                        effective_gpu,
                    );
                    self.run_checked(&run_args)?;
                }
            }
            ContainerState::Running => {
                let matches = self.container_ngl() == Some(ngl)
                    && self.container_model_id().as_deref() == Some(model_id)
                    && self.container_gpu().as_deref() == Some(effective_gpu.as_str());
                if !matches {
                    self.run_checked(&[
                        "rm".to_string(),
                        "-f".to_string(),
                        self.config.name.clone(),
                    ])?;
                    let run_args = build_run_args(
                        &self.config,
                        self.runtime,
                        ngl,
                        model,
                        &self.hf_cache_dir,
                        model_id,
                        effective_gpu,
                    );
                    self.run_checked(&run_args)?;
                }
            }
        }
        Ok(())
    }

    /// Stops the container (`stop -t 2 <name>`). No-op when already stopped or missing.
    pub fn stop(&self) -> Result<()> {
        match self.state()? {
            ContainerState::Missing | ContainerState::Stopped => Ok(()),
            ContainerState::Running => {
                self.run_checked(&[
                    "stop".to_string(),
                    "-t".to_string(),
                    "2".to_string(),
                    self.config.name.clone(),
                ])?;
                Ok(())
            }
        }
    }

    /// Queries resident memory usage in bytes via `stats --no-stream`.
    pub fn ram_bytes(&self) -> Option<u64> {
        let args = vec![
            "stats".to_string(),
            "--no-stream".to_string(),
            "--format".to_string(),
            "{{.MemUsage}}".to_string(),
            self.config.name.clone(),
        ];
        let out = self.cli.run(self.runtime.bin(), &args).ok()?;
        if out.status_code != 0 {
            return None;
        }
        parse_mem_usage(&out.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeCli {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        responses: Mutex<VecDeque<CmdOutput>>,
    }

    impl FakeCli {
        fn new(responses: Vec<CmdOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ContainerCli for FakeCli {
        fn run(&self, bin: &str, args: &[String]) -> Result<CmdOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((bin.to_string(), args.to_vec()));
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(CmdOutput {
                    status_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }
    }

    fn ok_out(stdout: &str) -> CmdOutput {
        CmdOutput {
            status_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn err_out(code: i32, stderr: &str) -> CmdOutput {
        CmdOutput {
            status_code: code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn test_detect_runtime_order_and_override() {
        // auto: kiri -> podman -> docker
        assert_eq!(
            detect_runtime("auto", |b| matches!(b, "kiri" | "podman" | "docker")),
            Some(ContainerRuntime::Kiri)
        );
        assert_eq!(
            detect_runtime("auto", |b| matches!(b, "podman" | "docker")),
            Some(ContainerRuntime::Podman)
        );
        assert_eq!(
            detect_runtime("auto", |b| b == "docker"),
            Some(ContainerRuntime::Docker)
        );
        assert_eq!(detect_runtime("auto", |_| false), None);

        // override wins if on PATH
        assert_eq!(
            detect_runtime("docker", |b| matches!(b, "kiri" | "docker")),
            Some(ContainerRuntime::Docker)
        );
        assert_eq!(detect_runtime("podman", |b| b == "docker"), None);
        assert_eq!(detect_runtime("unknown", |_| true), None);
    }

    #[test]
    fn test_parse_inspect_running() {
        assert_eq!(
            parse_inspect_running(&ok_out("true\n")),
            ContainerState::Running
        );
        assert_eq!(
            parse_inspect_running(&ok_out("false\n")),
            ContainerState::Stopped
        );
        assert_eq!(
            parse_inspect_running(&err_out(
                1,
                "Error: No such object: tchelvis-supervisor-llm"
            )),
            ContainerState::Missing
        );
        assert_eq!(parse_inspect_running(&ok_out("")), ContainerState::Missing);
    }

    #[test]
    fn test_build_run_args_cpu_gpu_docker_podman_file_hf() {
        let cfg = LlmContainerConfig {
            extra_args: vec!["--threads".into(), "4".into()],
            ..Default::default()
        };
        let cache = PathBuf::from("/tmp/hf-cache");
        let file_model = ModelSource::File(PathBuf::from("/data/models/lfm3.gguf"));

        // Docker CPU
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            0,
            &file_model,
            &cache,
            "file:/data/models/lfm3.gguf",
            GpuBackend::None,
        );
        assert!(!args.contains(&"--gpus".to_string()));
        assert!(!args.contains(&"--device".to_string()));
        assert!(args.contains(&"ghcr.io/ggml-org/llama.cpp:server".to_string()));
        assert!(args.contains(&"/data/models:/models:ro".to_string()));
        assert!(args.contains(&"/models/lfm3.gguf".to_string()));
        assert!(args.contains(&"tchelvis.ngl=0".to_string()));
        assert!(args.contains(&"--threads".to_string()));

        // Docker GPU + HfRepo
        let hf_model = ModelSource::HfRepo("LiquidAI/LFM2-1.2B-GGUF:Q4_K_M".into());
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            99,
            &hf_model,
            &cache,
            "hf:LiquidAI/LFM2-1.2B-GGUF:Q4_K_M",
            GpuBackend::Cuda,
        );
        assert!(args.windows(2).any(|w| w == ["--gpus", "all"]));
        assert!(args.contains(&"ghcr.io/ggml-org/llama.cpp:server-cuda".to_string()));
        assert!(args.contains(&"/tmp/hf-cache:/root/.cache/llama.cpp".to_string()));
        assert!(args.contains(&"LLAMA_CACHE=/root/.cache/llama.cpp".to_string()));
        assert!(args
            .windows(2)
            .any(|w| w == ["-hf", "LiquidAI/LFM2-1.2B-GGUF:Q4_K_M"]));
        assert!(args.contains(&"tchelvis.ngl=99".to_string()));

        // Podman GPU
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Podman,
            99,
            &file_model,
            &cache,
            "file:/data/models/lfm3.gguf",
            GpuBackend::Cuda,
        );
        assert!(args
            .windows(2)
            .any(|w| w == ["--device", "nvidia.com/gpu=all"]));
    }

    #[test]
    fn test_resolve_model_source_precedence_and_ollama_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create fake Ollama manifest & blob for lfm3:1.7b
        let manifest_dir = root.join("manifests/registry.ollama.ai/library/lfm3");
        fs::create_dir_all(&manifest_dir).unwrap();
        let blobs_dir = root.join("blobs");
        fs::create_dir_all(&blobs_dir).unwrap();
        let blob_path = blobs_dir.join("sha256-deadbeef");
        fs::write(&blob_path, b"gguf").unwrap();
        fs::write(
            manifest_dir.join("1.7b"),
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:deadbeef"}]}"#,
        )
        .unwrap();

        // 1. model_path wins over hf_repo and ollama
        let mut cfg = LlmContainerConfig {
            model_path: Some(PathBuf::from("/custom/model.gguf")),
            hf_repo: Some("org/repo".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_model_source(&cfg, "lfm3:1.7b", std::slice::from_ref(&root)).unwrap(),
            ModelSource::File(PathBuf::from("/custom/model.gguf"))
        );

        // 2. hf_repo wins over ollama
        cfg.model_path = None;
        assert_eq!(
            resolve_model_source(&cfg, "lfm3:1.7b", std::slice::from_ref(&root)).unwrap(),
            ModelSource::HfRepo("org/repo".into())
        );

        // 3. ollama manifest resolution
        cfg.hf_repo = None;
        assert_eq!(
            resolve_model_source(&cfg, "lfm3:1.7b", std::slice::from_ref(&root)).unwrap(),
            ModelSource::File(blob_path)
        );

        // 4. missing model -> pt-BR error
        let err = resolve_model_source(&cfg, "nonexistent:latest", &[root]).unwrap_err();
        assert!(err
            .to_string()
            .contains("Modelo GGUF não encontrado: defina llm_container.model_path ou hf_repo"));
    }

    #[test]
    fn test_parse_mem_usage() {
        assert_eq!(parse_mem_usage("1.2GiB / 31GiB"), Some(1_288_490_189));
        assert_eq!(parse_mem_usage("512MiB / 31.2GiB"), Some(512 * 1024 * 1024));
        assert_eq!(parse_mem_usage("1.234GB / 33.5GB"), Some(1_234_000_000));
        assert_eq!(parse_mem_usage("800kB / 16GB"), Some(800_000));
        assert_eq!(parse_mem_usage(""), None);
        assert_eq!(parse_mem_usage("-- / --"), None);
    }

    #[test]
    fn test_container_start_stop_state_machine() {
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let mid = "hf:org/tiny:Q4_K_M";

        // Missing -> run
        let cli = FakeCli::new(vec![
            err_out(1, "No such object"), // state() -> Missing
            ok_out("container_id\n"),     // run
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, mid, GpuBackend::None).unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1[0], "run");

        // Stopped with matching ngl, model_id, and gpu -> start
        let cli = FakeCli::new(vec![
            ok_out("false\n"),              // state() -> Stopped
            ok_out("0\n"),                  // container_ngl() -> 0
            ok_out("hf:org/tiny:Q4_K_M\n"), // container_model_id()
            ok_out("none\n"),               // container_gpu()
            ok_out(""),                     // start
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, mid, GpuBackend::None).unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls[4].1, vec!["start", "tchelvis-supervisor-llm"]);

        // Stopped with different ngl -> rm -f + run
        let cli = FakeCli::new(vec![
            ok_out("false\n"), // state() -> Stopped
            ok_out("0\n"),     // container_ngl() -> 0
            ok_out(""),        // rm -f
            ok_out("cid\n"),   // run
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(99, &model, mid, GpuBackend::Cuda).unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls[2].1, vec!["rm", "-f", "tchelvis-supervisor-llm"]);
        assert_eq!(calls[3].1[0], "run");

        // Running with same ngl, model_id, and gpu -> no-op
        let cli = FakeCli::new(vec![
            ok_out("true\n"),               // state() -> Running
            ok_out("0\n"),                  // container_ngl() -> 0
            ok_out("hf:org/tiny:Q4_K_M\n"), // container_model_id()
            ok_out("none\n"),               // container_gpu()
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, mid, GpuBackend::None).unwrap();
        assert_eq!(mgr.cli.calls().len(), 4);

        // Stop when Running -> stop -t 2
        let cli = FakeCli::new(vec![ok_out("true\n"), ok_out("")]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.stop().unwrap();
        assert_eq!(
            mgr.cli.calls()[1].1,
            vec!["stop", "-t", "2", "tchelvis-supervisor-llm"]
        );

        // Stop when Stopped -> no-op
        let cli = FakeCli::new(vec![ok_out("false\n")]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.stop().unwrap();
        assert_eq!(mgr.cli.calls().len(), 1);
    }

    #[test]
    fn test_wait_healthy_with_injected_clock() {
        let elapsed = Cell::new(Duration::ZERO);
        let attempts = Cell::new(0);
        let res = wait_healthy_with(
            "http://127.0.0.1:11435",
            Duration::from_secs(10),
            Duration::from_secs(2),
            |_| {
                attempts.set(attempts.get() + 1);
                attempts.get() >= 3
            },
            |d| elapsed.set(elapsed.get() + d),
            || elapsed.get(),
        );
        assert!(res.is_ok());
        assert_eq!(attempts.get(), 3);

        // Timeout case
        let elapsed = Cell::new(Duration::ZERO);
        let err = wait_healthy_with(
            "http://127.0.0.1:11435",
            Duration::from_secs(4),
            Duration::from_secs(2),
            |_| false,
            |d| elapsed.set(elapsed.get() + d),
            || elapsed.get(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("não respondeu"));
    }

    #[test]
    fn model_id_roundtrip_with_spaces_and_colons() {
        let parsed = ModelId::parse("file:/m/my model:v2.gguf").unwrap();
        assert_eq!(parsed, ModelId::File(PathBuf::from("/m/my model:v2.gguf")));
        assert_eq!(parsed.as_id(), "file:/m/my model:v2.gguf");

        let ol = ModelId::parse("ollama:hf.co/org/repo:Q8_0").unwrap();
        assert_eq!(ol, ModelId::Ollama("hf.co/org/repo:Q8_0".to_string()));
        assert_eq!(ol.as_id(), "ollama:hf.co/org/repo:Q8_0");

        assert!(ModelId::parse("bogus").is_err());
    }

    #[test]
    fn resolve_prefers_model_id_over_model_path_and_hf_repo() {
        let dir = tempfile::tempdir().unwrap();
        let selected_file = dir.path().join("selected.gguf");
        let other_file = dir.path().join("other.gguf");
        fs::write(&selected_file, b"gguf").unwrap();
        fs::write(&other_file, b"gguf").unwrap();

        let cfg = LlmContainerConfig {
            model_id: Some(format!("file:{}", selected_file.display())),
            model_path: Some(other_file),
            hf_repo: Some("org/repo".to_string()),
            ..Default::default()
        };
        let resolved = resolve_model_source(&cfg, "lfm3:1.7b", &[]).unwrap();
        assert_eq!(resolved, ModelSource::File(selected_file));
    }

    #[test]
    fn resolve_missing_ollama_selection_errors() {
        let cfg = LlmContainerConfig {
            model_id: Some("ollama:missing:1b".to_string()),
            ..Default::default()
        };
        let err = resolve_model_source(&cfg, "lfm3:1.7b", &[]).unwrap_err();
        assert!(err.to_string().contains("missing:1b"));
    }

    #[test]
    fn resolve_missing_file_selection_errors() {
        let cfg = LlmContainerConfig {
            model_id: Some("file:/nonexistent/path/model.gguf".to_string()),
            ..Default::default()
        };
        let err = resolve_model_source(&cfg, "lfm3:1.7b", &[]).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/path/model.gguf"));
    }

    #[test]
    fn run_args_include_model_id_label() {
        let cfg = LlmContainerConfig::default();
        let cache = PathBuf::from("/tmp/hf-cache");
        let file_model = ModelSource::File(PathBuf::from("/models/lfm3.gguf"));
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            0,
            &file_model,
            &cache,
            "ollama:lfm3:1.7b",
            GpuBackend::None,
        );
        assert!(args.contains(&"tchelvis.model_id=ollama:lfm3:1.7b".to_string()));
    }

    #[test]
    fn stopped_with_same_ngl_but_other_model_recreates() {
        let model = ModelSource::HfRepo("org/new:Q4_K_M".into());
        let cli = FakeCli::new(vec![
            ok_out("false\n"),            // state() -> Stopped
            ok_out("0\n"),                // container_ngl() -> 0
            ok_out("ollama:lfm3:1.7b\n"), // container_model_id() -> old model
            ok_out(""),                   // rm -f
            ok_out("cid\n"),              // run
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, "hf:org/new:Q4_K_M", GpuBackend::None)
            .unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls[3].1, vec!["rm", "-f", "tchelvis-supervisor-llm"]);
        assert_eq!(calls[4].1[0], "run");
    }

    #[test]
    fn running_with_same_labels_is_noop() {
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let cli = FakeCli::new(vec![
            ok_out("true\n"),               // state() -> Running
            ok_out("0\n"),                  // container_ngl() -> 0
            ok_out("hf:org/tiny:Q4_K_M\n"), // container_model_id() -> same
            ok_out("none\n"),               // container_gpu() -> same
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, "hf:org/tiny:Q4_K_M", GpuBackend::None)
            .unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls.len(), 4);
        assert!(!calls.iter().any(|c| c.1[0] == "rm" || c.1[0] == "run"));
    }

    #[test]
    fn run_args_default_single_slot() {
        let cfg = LlmContainerConfig::default();
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            0,
            &model,
            Path::new("/tmp"),
            "hf:org/tiny:Q4_K_M",
            GpuBackend::None,
        );
        assert!(args.windows(2).any(|w| w == ["-np", "1"]));
    }

    #[test]
    fn run_args_respect_user_parallel() {
        let cfg = LlmContainerConfig {
            extra_args: vec!["--parallel".to_string(), "2".to_string()],
            ..Default::default()
        };
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            0,
            &model,
            Path::new("/tmp"),
            "hf:org/tiny:Q4_K_M",
            GpuBackend::None,
        );
        assert!(!args.windows(2).any(|w| w == ["-np", "1"]));
        assert!(args.windows(2).any(|w| w == ["--parallel", "2"]));
    }

    #[test]
    fn auto_prefers_remembered_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let pref_path = dir.path().join("container_runtime");
        write_runtime_pref(&pref_path, ContainerRuntime::Docker).unwrap();
        let remembered = read_runtime_pref(&pref_path);
        assert_eq!(remembered, Some(ContainerRuntime::Docker));

        let candidates =
            detect_runtime_candidates("auto", remembered, |b| matches!(b, "podman" | "docker"));
        assert_eq!(
            candidates,
            vec![ContainerRuntime::Docker, ContainerRuntime::Podman]
        );
        assert_eq!(
            detect_runtime_with_pref("auto", remembered, |b| matches!(b, "podman" | "docker")),
            Some(ContainerRuntime::Docker)
        );
    }

    #[test]
    fn explicit_runtime_ignores_pref() {
        let candidates = detect_runtime_candidates("podman", Some(ContainerRuntime::Docker), |b| {
            matches!(b, "podman" | "docker")
        });
        assert_eq!(candidates, vec![ContainerRuntime::Podman]);
    }

    #[test]
    fn fallback_tries_next_runtime_on_run_failure() {
        let dir = tempfile::tempdir().unwrap();
        let pref_path = dir.path().join("container_runtime");
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let candidates = vec![ContainerRuntime::Podman, ContainerRuntime::Docker];

        let chosen = start_with_fallback(
            &candidates,
            0,
            &model,
            "hf:org/tiny:Q4_K_M",
            GpuBackend::None,
            Some(&pref_path),
            |rt| {
                let cli = match rt {
                    ContainerRuntime::Podman => FakeCli::new(vec![
                        err_out(1, "no such container"),  // inspect -> Missing
                        err_out(125, "newuidmap failed"), // run fails 125
                        ok_out(""),                       // rm -f cleanup
                    ]),
                    ContainerRuntime::Docker => FakeCli::new(vec![
                        err_out(1, "no such container"), // inspect -> Missing
                        ok_out("cid\n"),                 // run succeeds
                    ]),
                    ContainerRuntime::Kiri => unreachable!(),
                };
                LlmContainer::new(rt, LlmContainerConfig::default(), cli)
            },
            || Ok(()),
        )
        .unwrap();

        assert_eq!(chosen, ContainerRuntime::Docker);
        assert_eq!(
            read_runtime_pref(&pref_path),
            Some(ContainerRuntime::Docker)
        );
    }

    #[test]
    fn fallback_reports_all_errors() {
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let candidates = vec![ContainerRuntime::Podman, ContainerRuntime::Docker];

        let err = start_with_fallback(
            &candidates,
            0,
            &model,
            "hf:org/tiny:Q4_K_M",
            GpuBackend::None,
            None,
            |rt| {
                let msg = match rt {
                    ContainerRuntime::Podman => "podman broke",
                    ContainerRuntime::Docker => "docker daemon down",
                    ContainerRuntime::Kiri => unreachable!(),
                };
                let cli = FakeCli::new(vec![
                    err_out(1, "no such container"),
                    err_out(125, msg),
                    ok_out(""),
                ]);
                LlmContainer::new(rt, LlmContainerConfig::default(), cli)
            },
            || Ok(()),
        )
        .unwrap_err();

        let text = err.to_string();
        assert!(text.contains("podman:"));
        assert!(text.contains("podman broke"));
        assert!(text.contains("docker:"));
        assert!(text.contains("docker daemon down"));
    }

    #[test]
    fn detect_nvidia_device_or_smi() {
        assert_eq!(
            detect_gpu_backend(
                |p| p == Path::new("/dev/nvidia0"),
                |_| false,
                &[PathBuf::from("/dev/dri/renderD128")],
            ),
            GpuBackend::Cuda
        );
        assert_eq!(
            detect_gpu_backend(|_| false, |b| b == "nvidia-smi", &[]),
            GpuBackend::Cuda
        );
    }

    #[test]
    fn detect_render_node_is_vulkan() {
        assert_eq!(
            detect_gpu_backend(
                |_| false,
                |_| false,
                &[PathBuf::from("/dev/dri/renderD128")],
            ),
            GpuBackend::Vulkan
        );
    }

    #[test]
    fn detect_nothing_is_none() {
        assert_eq!(
            detect_gpu_backend(|_| false, |_| false, &[]),
            GpuBackend::None
        );
    }

    #[test]
    fn forced_backend_wins() {
        let cfg = LlmContainerConfig {
            gpu_backend: "rocm".to_string(),
            ..Default::default()
        };
        let (backend, _detail) = resolve_gpu_backend(&cfg);
        assert_eq!(backend, GpuBackend::Rocm);
    }

    #[test]
    fn run_args_per_backend() {
        let cfg = LlmContainerConfig::default();
        let cache = PathBuf::from("/tmp/hf-cache");
        let model = ModelSource::File(PathBuf::from("/data/models/lfm3.gguf"));
        let mid = "file:/data/models/lfm3.gguf";

        // Cuda docker
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            99,
            &model,
            &cache,
            mid,
            GpuBackend::Cuda,
        );
        assert!(args.windows(2).any(|w| w == ["--gpus", "all"]));
        assert!(args.contains(&cfg.image_gpu));
        assert!(args.contains(&"tchelvis.gpu=cuda".to_string()));

        // Cuda podman
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Podman,
            99,
            &model,
            &cache,
            mid,
            GpuBackend::Cuda,
        );
        assert!(args
            .windows(2)
            .any(|w| w == ["--device", "nvidia.com/gpu=all"]));
        assert!(args.contains(&cfg.image_gpu));

        // Vulkan docker
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            99,
            &model,
            &cache,
            mid,
            GpuBackend::Vulkan,
        );
        assert!(args.windows(2).any(|w| w == ["--device", "/dev/dri"]));
        assert!(args.windows(2).any(|w| w == ["--group-add", "video"]));
        assert!(args.contains(&cfg.image_vulkan));
        assert!(args.contains(&"tchelvis.gpu=vulkan".to_string()));

        // Vulkan podman
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Podman,
            99,
            &model,
            &cache,
            mid,
            GpuBackend::Vulkan,
        );
        assert!(args.windows(2).any(|w| w == ["--device", "/dev/dri"]));
        assert!(args.windows(2).any(|w| w == ["--group-add", "keep-groups"]));

        // Rocm
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            99,
            &model,
            &cache,
            mid,
            GpuBackend::Rocm,
        );
        assert!(args.windows(2).any(|w| w == ["--device", "/dev/kfd"]));
        assert!(args.windows(2).any(|w| w == ["--device", "/dev/dri"]));
        assert!(args.windows(2).any(|w| w == ["--group-add", "video"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--security-opt", "seccomp=unconfined"]));
        assert!(args.contains(&cfg.image_rocm));
        assert!(args.contains(&"tchelvis.gpu=rocm".to_string()));

        // ngl 0 contains no GPU flags and uses cfg.image
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Docker,
            0,
            &model,
            &cache,
            mid,
            GpuBackend::Cuda,
        );
        assert!(!args.contains(&"--gpus".to_string()));
        assert!(!args.contains(&"--device".to_string()));
        assert!(args.contains(&cfg.image));
        assert!(args.contains(&"tchelvis.gpu=none".to_string()));
    }

    #[test]
    fn gpu_label_change_recreates() {
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let mid = "hf:org/tiny:Q4_K_M";
        let cli = FakeCli::new(vec![
            ok_out("false\n"),              // state() -> Stopped
            ok_out("99\n"),                 // container_ngl() -> 99
            ok_out("hf:org/tiny:Q4_K_M\n"), // container_model_id() -> same
            ok_out("vulkan\n"),             // container_gpu() -> vulkan
            ok_out(""),                     // rm -f
            ok_out("cid\n"),                // run
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(99, &model, mid, GpuBackend::Cuda).unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls[4].1, vec!["rm", "-f", "tchelvis-supervisor-llm"]);
        assert_eq!(calls[5].1[0], "run");
    }

    #[test]
    fn start_gpu_none_errors() {
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let cli = FakeCli::new(vec![]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        let err = mgr
            .start(99, &model, "hf:org/tiny:Q4_K_M", GpuBackend::None)
            .unwrap_err();
        assert_eq!(err.to_string(), "No GPU detected (gpu_backend=auto)");
    }
}
