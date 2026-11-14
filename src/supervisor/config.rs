use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

/// Configurable `hc` CLI command templates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HcAdapterConfig {
    /// Path or binary name for `hc` (default: `/home/ubuntu/.local/bin/hc` or `hc`).
    pub hc_bin: String,
    /// Template for listing active sessions (e.g., `hc ls --json`).
    pub list_sessions_cmd: String,
    /// Template for capturing the last N lines of output (e.g., `hc read {agent_id} --lines {lines} --json`).
    pub capture_output_cmd: String,
    /// Template for sending a command/response to a specific agent (e.g., `hc send {agent_id} {payload}`).
    pub send_input_cmd: String,
    /// Optional label sync back onto `hc` terminals (`hc label <id> cockpit.supervisor_state=...`).
    pub sync_hc_labels: bool,
}

impl Default for HcAdapterConfig {
    fn default() -> Self {
        let default_bin = resolve_default_hc_bin();
        Self {
            hc_bin: env::var("HC_BIN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(default_bin),
            list_sessions_cmd: "hc ls --json".to_string(),
            capture_output_cmd: "hc read {agent_id} --lines {lines} --json".to_string(),
            send_input_cmd: "hc send {agent_id} {payload}".to_string(),
            sync_hc_labels: true,
        }
    }
}

fn resolve_default_hc_bin() -> String {
    if let Ok(home) = env::var("HOME") {
        let home_path = PathBuf::from(home);
        for candidate in [
            home_path.join(".local/bin/hc"),
            home_path.join(".cargo/bin/hc"),
        ] {
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    let ubuntu_hc = PathBuf::from("/home/ubuntu/.local/bin/hc");
    if ubuntu_hc.exists() {
        return "/home/ubuntu/.local/bin/hc".to_string();
    }
    "hc".to_string()
}

/// Supported LLM inference backends for the Cockpit Durable Supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LlmBackendKind {
    #[default]
    #[serde(alias = "ollama")]
    Ollama,
    #[serde(alias = "llamacpp", alias = "llama_cpp", alias = "llama.cpp")]
    LlamaCpp,
    #[serde(alias = "lmstudio", alias = "lm_studio")]
    LmStudio,
    #[serde(
        alias = "openai",
        alias = "api",
        alias = "cloud",
        alias = "openai_compatible"
    )]
    OpenAiCompatible,
}

impl LlmBackendKind {
    /// Parses a backend identifier from CLI flags, env vars, or config strings.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" => Some(Self::Ollama),
            "llama-cpp" | "llamacpp" | "llama_cpp" | "llama.cpp" => Some(Self::LlamaCpp),
            "lm-studio" | "lmstudio" | "lm_studio" => Some(Self::LmStudio),
            "openai-compatible" | "openai" | "api" | "cloud" | "openai_compatible" => {
                Some(Self::OpenAiCompatible)
            }
            _ => None,
        }
    }

    /// Returns the canonical kebab-case identifier for this backend.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::LlamaCpp => "llama-cpp",
            Self::LmStudio => "lm-studio",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }
}

/// Complete configuration for the Cockpit Durable Supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorConfig {
    /// Selected LLM backend (`ollama`, `llama-cpp`, `lm-studio`, `openai-compatible`).
    #[serde(default)]
    pub llm_backend: LlmBackendKind,
    /// Optional Bearer API key for OpenAI-compatible / cloud LLM endpoints.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Primary LLM HTTP endpoint (default: `http://localhost:11434`).
    pub ollama_host: String,
    /// Fallback Ollama HTTP endpoint (e.g. host `http://10.121.213.1:11434`).
    pub ollama_fallback_host: Option<String>,
    /// LLM model name (default: `lfm3:1.7b`).
    pub ollama_model: String,
    /// Sampling temperature (default: `0.1`).
    pub temperature: f32,
    /// Allow GPU offloading (`false` => `num_gpu: 0` CPU inference to preserve VRAM).
    pub allow_gpu: bool,
    /// Number of GPU layers when `allow_gpu` is true (ignored when `allow_gpu == false`).
    pub gpu_layers: u32,
    /// Request timeout for Ollama HTTP requests in seconds.
    pub ollama_timeout_secs: u64,
    /// Deterministic watcher poll interval in seconds (default: `12`).
    pub poll_interval_secs: u64,
    /// Seconds with 0 active sessions before unloading Ollama model (`keep_alive: 0`, default: `180`).
    pub idle_unload_secs: u64,
    /// Seconds of unchanged output before triggering inactivity LLM evaluation (default: `20`).
    pub inactivity_trigger_secs: u64,
    /// Number of terminal lines to capture per agent (default: `25`).
    pub capture_lines: usize,
    /// Default maximum consecutive auto-replies before loop-breaker triggers (default: `2`).
    pub max_auto_replies: u32,
    /// Path to persistent episodic FSM state file (`~/.local/state/cockpit/supervisor_state.json`).
    pub state_file_path: PathBuf,
    /// Whether to trigger desktop notifications via `notify-send` on Normal/Critical alerts.
    pub enable_desktop_notifications: bool,
    /// Configurable `hc` adapter settings.
    pub hc: HcAdapterConfig,
}

fn parse_env_bool(val: &str) -> bool {
    val == "1" || val.eq_ignore_ascii_case("true")
}

fn resolve_allow_gpu_from_env() -> Option<bool> {
    if let Ok(v) = env::var("COCKPIT_ALLOW_GPU") {
        return Some(parse_env_bool(&v));
    }
    if let Ok(v) = env::var("HERDR_ALLOW_GPU") {
        return Some(parse_env_bool(&v));
    }
    None
}

/// Path of the persisted GPU-offload toggle written by `cockpit supervisor gpu <on|off>`.
pub fn gpu_override_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("cockpit")
        .join("supervisor_gpu")
}

/// Reads the persisted GPU toggle (`on` / `off`); `None` when absent or unreadable.
pub fn read_gpu_override(path: &std::path::Path) -> Option<bool> {
    match fs::read_to_string(path)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// Persists the GPU toggle atomically.
pub fn write_gpu_override(path: &std::path::Path, allow_gpu: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, if allow_gpu { "on\n" } else { "off\n" })?;
    fs::rename(&tmp, path)
}

fn resolve_llm_backend_from_env() -> Option<LlmBackendKind> {
    env::var("COCKPIT_LLM_BACKEND")
        .ok()
        .and_then(|v| LlmBackendKind::from_str_loose(&v))
}

fn resolve_llm_host_from_env() -> Option<String> {
    env::var("COCKPIT_LLM_HOST")
        .ok()
        .or_else(|| env::var("OLLAMA_HOST").ok())
        .map(|h| normalize_host(&h))
}

fn resolve_llm_model_from_env() -> Option<String> {
    env::var("COCKPIT_LLM_MODEL")
        .ok()
        .or_else(|| env::var("OLLAMA_MODEL").ok())
}

fn resolve_api_key_from_env() -> Option<String> {
    env::var("COCKPIT_LLM_API_KEY")
        .ok()
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .or_else(|| env::var("OPENROUTER_API_KEY").ok())
        .filter(|k| !k.trim().is_empty())
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        let home = env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
        let default_state_path = env::var("HERDR_SUPERVISOR_STATE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(&home)
                    .join(".local")
                    .join("state")
                    .join("cockpit")
                    .join("supervisor_state.json")
            });

        let llm_backend = resolve_llm_backend_from_env().unwrap_or_default();
        let api_key = resolve_api_key_from_env();
        let ollama_host =
            resolve_llm_host_from_env().unwrap_or_else(|| "http://localhost:11434".to_string());
        let ollama_fallback_host = env::var("OLLAMA_FALLBACK_HOST")
            .ok()
            .or_else(|| Some("http://10.121.213.1:11434".to_string()));
        let ollama_model = resolve_llm_model_from_env().unwrap_or_else(|| "lfm3:1.7b".to_string());
        let allow_gpu = resolve_allow_gpu_from_env().unwrap_or(false);

        Self {
            llm_backend,
            api_key,
            ollama_host: normalize_host(&ollama_host),
            ollama_fallback_host: ollama_fallback_host.map(|h| normalize_host(&h)),
            ollama_model,
            temperature: 0.1,
            allow_gpu,
            gpu_layers: 99,
            ollama_timeout_secs: 45,
            poll_interval_secs: 12,
            idle_unload_secs: 180,
            inactivity_trigger_secs: 20,
            capture_lines: 25,
            max_auto_replies: 2,
            state_file_path: default_state_path,
            enable_desktop_notifications: true,
            hc: HcAdapterConfig::default(),
        }
    }
}

impl SupervisorConfig {
    /// Returns the legacy Herdr state file path (`~/.local/share/herdr/supervisor_state.json`).
    pub fn legacy_state_file_path() -> PathBuf {
        let home = env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
        PathBuf::from(&home)
            .join(".local")
            .join("share")
            .join("herdr")
            .join("supervisor_state.json")
    }

    /// Load configuration from `~/.config/cockpit/supervisor.toml` (or legacy `~/.config/herdr/supervisor.toml`),
    /// merged with environment variable overrides.
    pub fn load() -> Self {
        let home = env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
        let cockpit_cfg = PathBuf::from(&home)
            .join(".config")
            .join("cockpit")
            .join("supervisor.toml");
        let legacy_cfg = PathBuf::from(&home)
            .join(".config")
            .join("herdr")
            .join("supervisor.toml");

        let chosen_path = if cockpit_cfg.exists() {
            Some(cockpit_cfg)
        } else if legacy_cfg.exists() {
            Some(legacy_cfg)
        } else {
            None
        };

        let mut cfg = Self::default();
        if let Some(config_path) = chosen_path {
            if let Ok(raw) = fs::read_to_string(&config_path) {
                if let Ok(parsed) = toml::from_str::<SupervisorConfig>(&raw) {
                    cfg = parsed;
                    cfg.apply_env_overrides();
                }
            }
        }
        // Persisted `cockpit supervisor gpu on|off` wins over the TOML file, but env vars win over it.
        if resolve_allow_gpu_from_env().is_none() {
            if let Some(allow) = read_gpu_override(&gpu_override_path()) {
                cfg.allow_gpu = allow;
            }
        }
        cfg
    }

    fn apply_env_overrides(&mut self) {
        if let Some(backend) = resolve_llm_backend_from_env() {
            self.llm_backend = backend;
        }
        if let Some(api_key) = resolve_api_key_from_env() {
            self.api_key = Some(api_key);
        }
        if let Some(host) = resolve_llm_host_from_env() {
            self.ollama_host = host;
        }
        if let Ok(fallback) = env::var("OLLAMA_FALLBACK_HOST") {
            self.ollama_fallback_host = Some(normalize_host(&fallback));
        }
        if let Some(model) = resolve_llm_model_from_env() {
            self.ollama_model = model;
        }
        if let Some(allow_gpu) = resolve_allow_gpu_from_env() {
            self.allow_gpu = allow_gpu;
        }
        if let Ok(state_path) = env::var("HERDR_SUPERVISOR_STATE_PATH") {
            self.state_file_path = PathBuf::from(state_path);
        }
        if let Ok(hc_bin) = env::var("HC_BIN") {
            self.hc.hc_bin = hc_bin;
        }
    }

    /// Returns the effective primary host for the configured `llm_backend`.
    /// When `ollama_host` is still the Ollama default (`http://localhost:11434`),
    /// switches to each backend's standard default endpoint.
    pub fn effective_primary_host(&self) -> String {
        if self.ollama_host == "http://localhost:11434" {
            match self.llm_backend {
                LlmBackendKind::Ollama => self.ollama_host.clone(),
                LlmBackendKind::LlamaCpp => "http://localhost:8080".to_string(),
                LlmBackendKind::LmStudio => "http://localhost:1234".to_string(),
                LlmBackendKind::OpenAiCompatible => "https://api.openai.com".to_string(),
            }
        } else {
            self.ollama_host.clone()
        }
    }

    /// Returns the effective `num_gpu` option sent to Ollama (`0` when `allow_gpu` is false).
    pub fn effective_num_gpu(&self) -> u32 {
        if self.allow_gpu {
            self.gpu_layers
        } else {
            0
        }
    }
}

pub fn normalize_host(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_host() {
        assert_eq!(normalize_host("localhost:11434/"), "http://localhost:11434");
        assert_eq!(
            normalize_host("https://ollama.example.com/"),
            "https://ollama.example.com"
        );
    }

    #[test]
    fn test_default_gpu_and_idle_unload() {
        let mut cfg = SupervisorConfig::default();
        cfg.allow_gpu = false;
        assert_eq!(cfg.effective_num_gpu(), 0);
        assert_eq!(cfg.idle_unload_secs, 180);
    }

    #[test]
    fn test_default_model_is_lfm3() {
        // Unless overridden by env, default model should be lfm3:1.7b
        if env::var("COCKPIT_LLM_MODEL").is_err() && env::var("OLLAMA_MODEL").is_err() {
            let cfg = SupervisorConfig::default();
            assert_eq!(cfg.ollama_model, "lfm3:1.7b");
            assert_eq!(cfg.llm_backend, LlmBackendKind::Ollama);
        }
    }

    #[test]
    fn test_llm_backend_kind_from_str_loose() {
        assert_eq!(
            LlmBackendKind::from_str_loose("ollama"),
            Some(LlmBackendKind::Ollama)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("llama-cpp"),
            Some(LlmBackendKind::LlamaCpp)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("llamacpp"),
            Some(LlmBackendKind::LlamaCpp)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("llama.cpp"),
            Some(LlmBackendKind::LlamaCpp)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("lm-studio"),
            Some(LlmBackendKind::LmStudio)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("lmstudio"),
            Some(LlmBackendKind::LmStudio)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("openai-compatible"),
            Some(LlmBackendKind::OpenAiCompatible)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("openai"),
            Some(LlmBackendKind::OpenAiCompatible)
        );
        assert_eq!(
            LlmBackendKind::from_str_loose("api"),
            Some(LlmBackendKind::OpenAiCompatible)
        );
        assert_eq!(LlmBackendKind::from_str_loose("unknown"), None);
    }

    #[test]
    fn test_effective_primary_host() {
        let mut cfg = SupervisorConfig::default();
        cfg.ollama_host = "http://localhost:11434".to_string();

        cfg.llm_backend = LlmBackendKind::Ollama;
        assert_eq!(cfg.effective_primary_host(), "http://localhost:11434");

        cfg.llm_backend = LlmBackendKind::LlamaCpp;
        assert_eq!(cfg.effective_primary_host(), "http://localhost:8080");

        cfg.llm_backend = LlmBackendKind::LmStudio;
        assert_eq!(cfg.effective_primary_host(), "http://localhost:1234");

        cfg.llm_backend = LlmBackendKind::OpenAiCompatible;
        assert_eq!(cfg.effective_primary_host(), "https://api.openai.com");

        // Explicit custom host is preserved for any backend
        cfg.ollama_host = "https://openrouter.ai/api/v1".to_string();
        assert_eq!(cfg.effective_primary_host(), "https://openrouter.ai/api/v1");
    }

    #[test]
    fn test_gpu_override_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("supervisor_gpu");
        assert_eq!(read_gpu_override(&path), None);
        write_gpu_override(&path, true).unwrap();
        assert_eq!(read_gpu_override(&path), Some(true));
        write_gpu_override(&path, false).unwrap();
        assert_eq!(read_gpu_override(&path), Some(false));
    }
}
