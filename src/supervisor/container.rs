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
}

impl Default for LlmContainerConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            runtime: "auto".to_string(),
            name: "cockpit-supervisor-llm".to_string(),
            image: "ghcr.io/ggml-org/llama.cpp:server".to_string(),
            image_gpu: "ghcr.io/ggml-org/llama.cpp:server-cuda".to_string(),
            port: 11435,
            ctx_size: 4096,
            model_path: None,
            hf_repo: None,
            extra_args: Vec::new(),
            model_id: None,
            model_filter: "4b".to_string(),
            model_dirs: Vec::new(),
            fallback_model_id: None,
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
    match override_.trim().to_ascii_lowercase().as_str() {
        "kiri" => is_on_path("kiri").then_some(ContainerRuntime::Kiri),
        "podman" => is_on_path("podman").then_some(ContainerRuntime::Podman),
        "docker" => is_on_path("docker").then_some(ContainerRuntime::Docker),
        "auto" | "" => [
            ContainerRuntime::Kiri,
            ContainerRuntime::Podman,
            ContainerRuntime::Docker,
        ]
        .into_iter()
        .find(|rt| is_on_path(rt.bin())),
        _ => None,
    }
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
    let home = env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
    PathBuf::from(home)
        .join(".cache")
        .join("cockpit")
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
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        cfg.name.clone(),
        "--label".to_string(),
        format!("cockpit.ngl={ngl}"),
        "--label".to_string(),
        format!("cockpit.model_id={model_id}"),
        "-p".to_string(),
        format!("127.0.0.1:{}:8080", cfg.port),
    ];

    if ngl > 0 {
        match runtime {
            ContainerRuntime::Docker | ContainerRuntime::Kiri => {
                args.push("--gpus".to_string());
                args.push("all".to_string());
            }
            ContainerRuntime::Podman => {
                args.push("--device".to_string());
                args.push("nvidia.com/gpu=all".to_string());
            }
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
        cfg.image_gpu.clone()
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

    /// Reads the `cockpit.ngl` label from an existing container.
    pub fn container_ngl(&self) -> Option<u32> {
        let args = vec![
            "inspect".to_string(),
            "-f".to_string(),
            "{{index .Config.Labels \"cockpit.ngl\"}}".to_string(),
            self.config.name.clone(),
        ];
        let out = self.cli.run(self.runtime.bin(), &args).ok()?;
        if out.status_code != 0 {
            return None;
        }
        out.stdout.trim().parse::<u32>().ok()
    }

    /// Reads the `cockpit.model_id` label from an existing container.
    pub fn container_model_id(&self) -> Option<String> {
        let args = vec![
            "inspect".to_string(),
            "-f".to_string(),
            "{{index .Config.Labels \"cockpit.model_id\"}}".to_string(),
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

    /// Starts (or recreates when `ngl` or `model_id` changed) the `llama.cpp` container.
    pub fn start(&self, ngl: u32, model: &ModelSource, model_id: &str) -> Result<()> {
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
                );
                self.run_checked(&run_args)?;
            }
            ContainerState::Stopped => {
                let matches = self.container_ngl() == Some(ngl)
                    && self.container_model_id().as_deref() == Some(model_id);
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
                    );
                    self.run_checked(&run_args)?;
                }
            }
            ContainerState::Running => {
                let matches = self.container_ngl() == Some(ngl)
                    && self.container_model_id().as_deref() == Some(model_id);
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
            parse_inspect_running(&err_out(1, "Error: No such object: cockpit-supervisor-llm")),
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
        );
        assert!(!args.contains(&"--gpus".to_string()));
        assert!(!args.contains(&"--device".to_string()));
        assert!(args.contains(&"ghcr.io/ggml-org/llama.cpp:server".to_string()));
        assert!(args.contains(&"/data/models:/models:ro".to_string()));
        assert!(args.contains(&"/models/lfm3.gguf".to_string()));
        assert!(args.contains(&"cockpit.ngl=0".to_string()));
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
        );
        assert!(args.windows(2).any(|w| w == ["--gpus", "all"]));
        assert!(args.contains(&"ghcr.io/ggml-org/llama.cpp:server-cuda".to_string()));
        assert!(args.contains(&"/tmp/hf-cache:/root/.cache/llama.cpp".to_string()));
        assert!(args.contains(&"LLAMA_CACHE=/root/.cache/llama.cpp".to_string()));
        assert!(args
            .windows(2)
            .any(|w| w == ["-hf", "LiquidAI/LFM2-1.2B-GGUF:Q4_K_M"]));
        assert!(args.contains(&"cockpit.ngl=99".to_string()));

        // Podman GPU
        let args = build_run_args(
            &cfg,
            ContainerRuntime::Podman,
            99,
            &file_model,
            &cache,
            "file:/data/models/lfm3.gguf",
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
        mgr.start(0, &model, mid).unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1[0], "run");

        // Stopped with matching ngl and model_id -> start
        let cli = FakeCli::new(vec![
            ok_out("false\n"),              // state() -> Stopped
            ok_out("0\n"),                  // container_ngl() -> 0
            ok_out("hf:org/tiny:Q4_K_M\n"), // container_model_id()
            ok_out(""),                     // start
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, mid).unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls[3].1, vec!["start", "cockpit-supervisor-llm"]);

        // Stopped with different ngl -> rm -f + run
        let cli = FakeCli::new(vec![
            ok_out("false\n"), // state() -> Stopped
            ok_out("0\n"),     // container_ngl() -> 0
            ok_out(""),        // rm -f
            ok_out("cid\n"),   // run
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(99, &model, mid).unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls[2].1, vec!["rm", "-f", "cockpit-supervisor-llm"]);
        assert_eq!(calls[3].1[0], "run");

        // Running with same ngl and model_id -> no-op
        let cli = FakeCli::new(vec![
            ok_out("true\n"),               // state() -> Running
            ok_out("0\n"),                  // container_ngl() -> 0
            ok_out("hf:org/tiny:Q4_K_M\n"), // container_model_id()
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, mid).unwrap();
        assert_eq!(mgr.cli.calls().len(), 3);

        // Stop when Running -> stop -t 2
        let cli = FakeCli::new(vec![ok_out("true\n"), ok_out("")]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.stop().unwrap();
        assert_eq!(
            mgr.cli.calls()[1].1,
            vec!["stop", "-t", "2", "cockpit-supervisor-llm"]
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
        );
        assert!(args.contains(&"cockpit.model_id=ollama:lfm3:1.7b".to_string()));
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
        mgr.start(0, &model, "hf:org/new:Q4_K_M").unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls[3].1, vec!["rm", "-f", "cockpit-supervisor-llm"]);
        assert_eq!(calls[4].1[0], "run");
    }

    #[test]
    fn running_with_same_labels_is_noop() {
        let model = ModelSource::HfRepo("org/tiny:Q4_K_M".into());
        let cli = FakeCli::new(vec![
            ok_out("true\n"),               // state() -> Running
            ok_out("0\n"),                  // container_ngl() -> 0
            ok_out("hf:org/tiny:Q4_K_M\n"), // container_model_id() -> same
        ]);
        let mgr = LlmContainer::new(ContainerRuntime::Docker, LlmContainerConfig::default(), cli);
        mgr.start(0, &model, "hf:org/tiny:Q4_K_M").unwrap();
        let calls = mgr.cli.calls();
        assert_eq!(calls.len(), 3);
        assert!(!calls.iter().any(|c| c.1[0] == "rm" || c.1[0] == "run"));
    }
}
