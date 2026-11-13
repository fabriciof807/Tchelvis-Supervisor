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
        let default_bin = if PathBuf::from("/home/ubuntu/.local/bin/hc").exists() {
            "/home/ubuntu/.local/bin/hc".to_string()
        } else {
            "hc".to_string()
        };
        Self {
            hc_bin: env::var("HC_BIN").unwrap_or(default_bin),
            list_sessions_cmd: "hc ls --json".to_string(),
            capture_output_cmd: "hc read {agent_id} --lines {lines} --json".to_string(),
            send_input_cmd: "hc send {agent_id} {payload}".to_string(),
            sync_hc_labels: true,
        }
    }
}

/// Complete configuration for the Cockpit Durable Supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorConfig {
    /// Primary Ollama HTTP endpoint (default: `http://localhost:11434`).
    pub ollama_host: String,
    /// Fallback Ollama HTTP endpoint (e.g. host `http://10.121.213.1:11434`).
    pub ollama_fallback_host: Option<String>,
    /// Local LLM model name (default: `qwen2.5:1.5b-instruct`, configurable up to `3b`).
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

        let ollama_host =
            env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
        let ollama_fallback_host = env::var("OLLAMA_FALLBACK_HOST")
            .ok()
            .or_else(|| Some("http://10.121.213.1:11434".to_string()));
        let ollama_model =
            env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen2.5:1.5b-instruct".to_string());
        let allow_gpu = resolve_allow_gpu_from_env().unwrap_or(false);

        Self {
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
        cfg
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(host) = env::var("OLLAMA_HOST") {
            self.ollama_host = normalize_host(&host);
        }
        if let Ok(fallback) = env::var("OLLAMA_FALLBACK_HOST") {
            self.ollama_fallback_host = Some(normalize_host(&fallback));
        }
        if let Ok(model) = env::var("OLLAMA_MODEL") {
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
}
