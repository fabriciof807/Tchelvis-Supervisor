use super::config::{LlmBackendKind, SupervisorConfig};
use super::container::{
    probe_llama_server_health, ContainerState, LlmContainer, SystemContainerCli,
};
use super::ollama::{CognitiveBackend, CognitiveEvaluation, OllamaClient};
use super::state::AgentDurableState;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FallbackConfig {
    pub host: String,
    pub model: String,
}

impl FallbackConfig {
    pub fn is_set(&self) -> bool {
        !self.host.trim().is_empty() && !self.model.trim().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    LoadFailed,
    Health,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FallbackState {
    pub primary_intended: bool,
    pub health_failures: u32,
    pub active: bool,
    pub since: Option<u64>,
    pub reason: Option<FallbackReason>,
    pub detail: Option<String>,
    /// Set while an intentional primary (re)start is in progress (load, model/GPU switch, bench).
    /// Health probes are not counted until it settles or `START_GRACE_SECS` pass.
    pub starting_since: Option<u64>,
    /// When the last counted health failure happened (any process); closer probes do not count.
    pub last_failure_at: Option<u64>,
}

/// How long an intentional primary restart may keep `/health` down before probes count again.
pub const START_GRACE_SECS: u64 = 300;

/// Extracts the lowercase hostname from a base URL (scheme, userinfo, port and path removed).
fn hostname_of(host: &str) -> String {
    let lower = host.trim().to_ascii_lowercase();
    let rest = lower.split_once("://").map(|(_, r)| r).unwrap_or(&lower);
    // `\\` too: URL parsers treat it as a path separator for http(s).
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or("");
    let without_user = authority.rsplit('@').next().unwrap_or("");
    without_user.split(':').next().unwrap_or("").to_string()
}

/// Provider key env var for a host. Matches the hostname exactly or as a subdomain, so a
/// provider key is never sent to a look-alike host (`openai.com.example.io`).
pub fn provider_env_for_host(host: &str) -> Option<&'static str> {
    const PROVIDERS: [(&str, &str); 4] = [
        ("openai.com", "OPENAI_API_KEY"),
        ("openrouter.ai", "OPENROUTER_API_KEY"),
        ("deepseek.com", "DEEPSEEK_API_KEY"),
        ("opencode.ai", "OPENCODE_API_KEY"),
    ];
    let name = hostname_of(host);
    PROVIDERS
        .iter()
        .find(|(domain, _)| name == *domain || name.ends_with(&format!(".{domain}")))
        .map(|(_, env)| *env)
}

fn non_blank(val: Option<String>) -> Option<String> {
    val.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn resolve_fallback_key(
    host: &str,
    key_file: Option<String>,
    get: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    non_blank(get("TCHELVIS_FALLBACK_API_KEY"))
        .or_else(|| non_blank(key_file))
        .or_else(|| non_blank(get("TCHELVIS_LLM_API_KEY")))
        .or_else(|| provider_env_for_host(host).and_then(|env_key| non_blank(get(env_key))))
}

pub fn resolve_primary_key(
    host: &str,
    toml_key: Option<String>,
    get: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    non_blank(get("TCHELVIS_LLM_API_KEY"))
        .or_else(|| non_blank(toml_key))
        .or_else(|| provider_env_for_host(host).and_then(|env_key| non_blank(get(env_key))))
}

pub fn fallback_key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("tchelvis")
        .join("fallback_api_key")
}

pub fn read_key_file(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    non_blank(Some(raw))
}

pub fn write_key_file(path: &Path, key: &str) -> Result<()> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        bail!("Fallback API key cannot be empty");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let tmp = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
    // Never follow or reuse a leftover file: remove it, then require a fresh 0600 file.
    let _ = fs::remove_file(&tmp);
    {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("Failed to create temp key file {}", tmp.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        f.write_all(trimmed.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to install key file {}", path.display()))?;
    Ok(())
}

pub fn redact_key(text: &str, key: &str) -> String {
    if key.is_empty() {
        text.to_string()
    } else {
        text.replace(key, "***")
    }
}

pub fn fallback_state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("tchelvis")
        .join("supervisor_fallback.json")
}

pub fn load_state(path: &Path) -> FallbackState {
    if !path.exists() {
        return FallbackState::default();
    }
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<FallbackState>(&raw) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "Corrupt supervisor fallback state; using default"
                );
                FallbackState::default()
            }
        },
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to read supervisor fallback state; using default"
            );
            FallbackState::default()
        }
    }
}

struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire_exclusive(lock_path: &Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).with_context(|| format!("flock failed on {}", lock_path.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn update_state(path: &Path, f: impl FnOnce(&mut FallbackState)) -> Result<FallbackState> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let _lock = FileLock::acquire_exclusive(&lock_path)?;

    let mut state = load_state(path);
    f(&mut state);

    let payload = serde_json::to_vec_pretty(&state)?;
    let tmp = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
    fs::write(&tmp, payload)
        .with_context(|| format!("Failed to write temp state {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(state)
}

#[derive(Debug, Clone)]
pub struct ProbeCtx {
    pub healthy: bool,
    pub paused: bool,
    pub sessions: usize,
    pub configured: bool,
    pub now: u64,
    pub detail: String,
    /// Minimum seconds between two counted failures, so ticks from several processes
    /// (watch daemon + mod) do not add up to "2 consecutive" within seconds.
    pub min_gap_secs: u64,
}

pub fn activate(state: &mut FallbackState, reason: FallbackReason, detail: &str, now: u64) {
    if !state.active || state.since.is_none() {
        state.since = Some(now);
    }
    state.active = true;
    state.starting_since = None;
    state.last_failure_at = None;
    state.reason = Some(reason);
    state.detail = Some(detail.to_string());
    state.health_failures = 0;
}

pub fn apply_health_probe(state: &mut FallbackState, ctx: &ProbeCtx) -> bool {
    if state.active || !state.primary_intended || ctx.paused || ctx.sessions == 0 {
        return false;
    }
    if let Some(started) = state.starting_since {
        if ctx.now.saturating_sub(started) < START_GRACE_SECS {
            state.health_failures = 0;
            return false;
        }
    }
    if ctx.healthy {
        state.health_failures = 0;
        state.last_failure_at = None;
        return false;
    }
    if let Some(last) = state.last_failure_at {
        if ctx.now.saturating_sub(last) < ctx.min_gap_secs {
            return false;
        }
    }
    state.last_failure_at = Some(ctx.now);
    state.health_failures = state.health_failures.saturating_add(1);
    if state.health_failures >= 2 && ctx.configured {
        activate(state, FallbackReason::Health, &ctx.detail, ctx.now);
        return true;
    }
    false
}

/// Marks an intentional primary (re)start; probes are ignored until it settles or the grace ends.
pub fn primary_starting(state: &mut FallbackState, now: u64) {
    state.starting_since = Some(now);
    state.health_failures = 0;
    state.last_failure_at = None;
}

/// Ends the start grace without changing whether the primary is intended or the fallback active.
pub fn primary_start_settled(state: &mut FallbackState) {
    state.starting_since = None;
    state.health_failures = 0;
    state.last_failure_at = None;
}

/// Best-effort `primary_starting` on the shared state file (errors are logged, never fatal).
pub fn mark_primary_starting(path: &Path, now: u64) {
    if let Err(err) = update_state(path, |s| primary_starting(s, now)) {
        tracing::warn!(error = %format!("{err:#}"), "Failed to mark primary start");
    }
}

pub fn primary_started(state: &mut FallbackState) {
    state.starting_since = None;
    state.last_failure_at = None;
    state.primary_intended = true;
    state.active = false;
    state.reason = None;
    state.detail = None;
    state.since = None;
    state.health_failures = 0;
}

pub fn primary_stopped(state: &mut FallbackState) {
    state.starting_since = None;
    state.last_failure_at = None;
    state.primary_intended = false;
    state.active = false;
    state.reason = None;
    state.detail = None;
    state.since = None;
    state.health_failures = 0;
}

pub trait FallbackProvider: Send + Sync {
    fn load_state(&self) -> FallbackState;
    fn update_state(&self, f: &mut dyn FnMut(&mut FallbackState)) -> Result<FallbackState>;
    /// (healthy, detail) for the primary: container Running AND GET {primary}/health == 200.
    fn probe_primary(&self) -> (bool, String);
    /// Fresh each call: re-reads `[fallback]` from supervisor.toml and resolves the key.
    /// None when not configured. Errors returned by the backend are redacted of the key.
    fn backend(&self) -> Option<Box<dyn CognitiveBackend>>;
}

pub struct NoFallback;

impl FallbackProvider for NoFallback {
    fn load_state(&self) -> FallbackState {
        FallbackState::default()
    }

    fn update_state(&self, _f: &mut dyn FnMut(&mut FallbackState)) -> Result<FallbackState> {
        Ok(FallbackState::default())
    }

    fn probe_primary(&self) -> (bool, String) {
        (true, String::new())
    }

    fn backend(&self) -> Option<Box<dyn CognitiveBackend>> {
        None
    }
}

pub struct RedactingBackend {
    pub inner: Box<dyn CognitiveBackend>,
    pub key: String,
}

impl CognitiveBackend for RedactingBackend {
    fn evaluate(
        &self,
        state: &AgentDurableState,
        recent_output: &str,
    ) -> Result<CognitiveEvaluation> {
        self.inner
            .evaluate(state, recent_output)
            .map_err(|err| anyhow::anyhow!("{}", redact_key(&format!("{err:#}"), &self.key)))
    }

    fn unload_model(&self) -> Result<()> {
        self.inner
            .unload_model()
            .map_err(|err| anyhow::anyhow!("{}", redact_key(&format!("{err:#}"), &self.key)))
    }

    fn is_available(&self) -> bool {
        self.inner.is_available()
    }

    fn model_label(&self) -> String {
        self.inner.model_label()
    }
}

pub fn fallback_client_config(
    primary: &SupervisorConfig,
    fb: &FallbackConfig,
    key: String,
) -> SupervisorConfig {
    let mut cfg = primary.clone();
    cfg.llm_container.mode = "off".to_string();
    cfg.llm_backend = LlmBackendKind::OpenAiCompatible;
    let host = normalize_fallback_host(&fb.host);
    // Explicit override: `effective_primary_host` would otherwise rewrite the Ollama default
    // port (`localhost:11434`) to api.openai.com for the openai-compatible backend.
    cfg.host_override = Some(host.clone());
    cfg.ollama_host = host;
    cfg.ollama_fallback_host = None;
    cfg.ollama_model = fb.model.trim().to_string();
    cfg.api_key = Some(key);
    cfg
}

pub struct LiveFallback {
    pub state_path: PathBuf,
    pub primary: SupervisorConfig,
}

impl FallbackProvider for LiveFallback {
    fn load_state(&self) -> FallbackState {
        load_state(&self.state_path)
    }

    fn update_state(&self, f: &mut dyn FnMut(&mut FallbackState)) -> Result<FallbackState> {
        update_state(&self.state_path, f)
    }

    fn probe_primary(&self) -> (bool, String) {
        let Some(rt) = self.primary.detected_container_runtime() else {
            return (false, "container parado".to_string());
        };
        let mgr = LlmContainer::new(rt, self.primary.llm_container.clone(), SystemContainerCli);
        if !matches!(mgr.state(), Ok(ContainerState::Running)) {
            return (false, "container parado".to_string());
        }
        if probe_llama_server_health(&self.primary.effective_primary_host()) {
            (true, String::new())
        } else {
            (false, "/health sem resposta".to_string())
        }
    }

    fn backend(&self) -> Option<Box<dyn CognitiveBackend>> {
        let fb = SupervisorConfig::load().fallback;
        if !fb.is_set() {
            return None;
        }
        let host = normalize_fallback_host(&fb.host);
        let key = resolve_fallback_key(&host, read_key_file(&fallback_key_path()), |k| {
            std::env::var(k).ok()
        })?;
        let client_cfg = fallback_client_config(&self.primary, &fb, key.clone());
        Some(Box::new(RedactingBackend {
            inner: Box::new(OllamaClient::new(client_cfg)),
            key,
        }))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackInfo {
    pub configured: bool,
    pub key_present: bool,
    pub active: bool,
    pub host: String,
    pub model: String,
    pub reason: Option<FallbackReason>,
    pub detail: Option<String>,
    pub since: Option<u64>,
}

pub fn fallback_snapshot_info(
    cfg: &FallbackConfig,
    key_present: bool,
    state: &FallbackState,
) -> FallbackInfo {
    FallbackInfo {
        configured: cfg.is_set() && key_present,
        key_present,
        active: state.active,
        host: cfg.host.trim().to_string(),
        model: cfg.model.trim().to_string(),
        reason: state.reason,
        detail: state.detail.clone(),
        since: state.since,
    }
}

pub fn resolve_fallback_key_with_source(
    host: &str,
    key_file: Option<String>,
    get: impl Fn(&str) -> Option<String>,
) -> Option<(String, &'static str)> {
    if let Some(k) = non_blank(get("TCHELVIS_FALLBACK_API_KEY")) {
        return Some((k, "TCHELVIS_FALLBACK_API_KEY"));
    }
    if let Some(k) = non_blank(key_file) {
        return Some((k, "~/.config/tchelvis/fallback_api_key"));
    }
    if let Some(k) = non_blank(get("TCHELVIS_LLM_API_KEY")) {
        return Some((k, "TCHELVIS_LLM_API_KEY"));
    }
    if let Some(env_name) = provider_env_for_host(host) {
        if let Some(k) = non_blank(get(env_name)) {
            return Some((k, env_name));
        }
    }
    None
}

pub fn write_key_from_reader(path: &Path, mut r: impl std::io::Read) -> Result<()> {
    let mut raw = String::new();
    r.read_to_string(&mut raw)
        .context("Failed to read fallback API key from stdin")?;
    write_key_file(path, &raw)
}

pub fn finish_primary_start(p: &dyn FallbackProvider, res: Result<()>, now: u64) -> Result<()> {
    match res {
        Ok(()) => {
            let _ = p.update_state(&mut primary_started);
            Ok(())
        }
        Err(e) => {
            if let Some(backend) = p.backend() {
                let model = backend.model_label();
                let short_err: String = format!("{e:#}").chars().take(300).collect();
                let _ = p.update_state(&mut |s| {
                    s.primary_intended = true;
                    activate(s, FallbackReason::LoadFailed, &short_err, now);
                });
                Err(e.context(format!("fallback ativo: {model}")))
            } else {
                let _ = p.update_state(&mut |s| {
                    s.primary_intended = true;
                    s.starting_since = None;
                });
                Err(e)
            }
        }
    }
}

/// Normalizes a fallback base URL: trims, drops trailing `/`, and defaults to `https://` so a
/// typed `openrouter.ai/api` never sends the key in cleartext. An explicit `http://` is kept.
pub fn normalize_fallback_host(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

pub fn models_url(host: &str) -> String {
    let base = normalize_fallback_host(host);
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

pub fn parse_models_list(v: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = v
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    ids
}

pub fn filter_models(ids: &[String], filter: Option<&str>) -> Vec<String> {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        None => ids.to_vec(),
        Some(needle) => {
            let lower = needle.to_ascii_lowercase();
            ids.iter()
                .filter(|id| id.to_ascii_lowercase().contains(&lower))
                .cloned()
                .collect()
        }
    }
}

pub fn models_http_error(status: u16) -> String {
    match status {
        401 | 403 => format!("chave recusada pelo endpoint (HTTP {status})"),
        404 => "endpoint sem /models (HTTP 404) — confira o host".to_string(),
        _ => format!("HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn provider_env_matches_host() {
        assert_eq!(
            provider_env_for_host("https://openrouter.ai/api"),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            provider_env_for_host("https://api.deepseek.com"),
            Some("DEEPSEEK_API_KEY")
        );
        assert_eq!(
            provider_env_for_host("https://opencode.ai/inference/go/openai/v1"),
            Some("OPENCODE_API_KEY")
        );
        assert_eq!(
            provider_env_for_host("https://API.OPENAI.COM"),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(provider_env_for_host("http://127.0.0.1:8080"), None);
    }

    #[test]
    fn fallback_key_order() {
        let all = [
            ("TCHELVIS_FALLBACK_API_KEY", "f"),
            ("TCHELVIS_LLM_API_KEY", "l"),
            ("OPENROUTER_API_KEY", "p"),
        ];
        let h = "https://openrouter.ai/api";
        assert_eq!(
            resolve_fallback_key(h, Some("file".into()), env(&all)).as_deref(),
            Some("f")
        );
        assert_eq!(
            resolve_fallback_key(h, Some("file".into()), env(&all[1..])).as_deref(),
            Some("file")
        );
        assert_eq!(
            resolve_fallback_key(h, None, env(&all[1..])).as_deref(),
            Some("l")
        );
        assert_eq!(
            resolve_fallback_key(h, None, env(&all[2..])).as_deref(),
            Some("p")
        );
    }

    #[test]
    fn provider_key_never_crosses_hosts() {
        let e = [("OPENAI_API_KEY", "sk-openai")];
        assert_eq!(
            resolve_fallback_key("https://api.deepseek.com", None, env(&e)),
            None
        );
        assert_eq!(
            resolve_primary_key("https://api.deepseek.com", None, env(&e)),
            None
        );
        assert_eq!(
            resolve_primary_key("https://api.openai.com", None, env(&e)).as_deref(),
            Some("sk-openai")
        );
    }

    #[test]
    fn primary_key_order_env_then_toml_then_provider() {
        let e = [("TCHELVIS_LLM_API_KEY", "l"), ("OPENAI_API_KEY", "p")];
        assert_eq!(
            resolve_primary_key("https://api.openai.com", Some("t".into()), env(&e)).as_deref(),
            Some("l")
        );
        assert_eq!(
            resolve_primary_key("https://api.openai.com", Some("t".into()), env(&e[1..]))
                .as_deref(),
            Some("t")
        );
    }

    #[test]
    fn key_file_is_0600_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/fallback_api_key");
        write_key_file(&p, "  sk-x \n").unwrap();
        assert_eq!(read_key_file(&p).as_deref(), Some("sk-x"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_key_file(&p, "  ").is_err());
    }

    #[test]
    fn redact_key_hides_every_occurrence() {
        assert_eq!(redact_key("Bearer sk-1 / sk-1", "sk-1"), "Bearer *** / ***");
        assert_eq!(redact_key("x", ""), "x");
    }

    #[test]
    fn state_roundtrip_and_update_under_lock() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("supervisor_fallback.json");
        assert_eq!(load_state(&p), FallbackState::default());
        let s = update_state(&p, |s| {
            s.active = true;
            s.reason = Some(FallbackReason::Health);
        })
        .unwrap();
        assert!(s.active);
        assert_eq!(load_state(&p), s);
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load_state(&p), FallbackState::default());
    }

    #[test]
    fn fallback_config_is_set_needs_both() {
        assert!(!FallbackConfig {
            host: "h".into(),
            model: " ".into()
        }
        .is_set());
        assert!(FallbackConfig {
            host: "h".into(),
            model: "m".into()
        }
        .is_set());
    }

    fn ctx(healthy: bool) -> ProbeCtx {
        ProbeCtx {
            healthy,
            paused: false,
            sessions: 1,
            configured: true,
            now: 100,
            detail: "/health sem resposta".into(),
            min_gap_secs: 9,
        }
    }

    #[test]
    fn failures_closer_than_the_min_gap_count_once() {
        // Two processes (watch daemon + mod tick) probing seconds apart must not add up.
        let mut s = intended();
        let mut c = ctx(false);
        apply_health_probe(&mut s, &c);
        c.now = 103;
        assert!(!apply_health_probe(&mut s, &c));
        assert_eq!(s.health_failures, 1);
        c.now = 112;
        assert!(apply_health_probe(&mut s, &c));
    }

    #[test]
    fn localhost_ollama_port_fallback_is_not_rewritten_to_openai() {
        let primary = SupervisorConfig::default();
        let fb = FallbackConfig {
            host: "localhost:11434".into(),
            model: "qwen3:4b".into(),
        };
        let c = fallback_client_config(&primary, &fb, "k".into());
        assert_eq!(c.effective_primary_host(), "https://localhost:11434");
        let fb = FallbackConfig {
            host: "http://localhost:11434".into(),
            model: "qwen3:4b".into(),
        };
        let c = fallback_client_config(&primary, &fb, "k".into());
        assert_eq!(c.effective_primary_host(), "http://localhost:11434");
    }

    #[test]
    fn fallback_host_without_scheme_defaults_to_https() {
        assert_eq!(
            normalize_fallback_host(" openrouter.ai/api/ "),
            "https://openrouter.ai/api"
        );
        assert_eq!(
            normalize_fallback_host("http://10.0.0.5:4000"),
            "http://10.0.0.5:4000"
        );
        assert_eq!(
            models_url("openrouter.ai/api"),
            "https://openrouter.ai/api/v1/models"
        );
    }

    #[test]
    fn backslash_in_host_does_not_fake_a_provider_domain() {
        assert_eq!(
            provider_env_for_host("https://evil.example\\.openai.com/v1"),
            None
        );
    }

    #[test]
    fn failed_start_without_fallback_ends_the_start_grace() {
        let p = Task5FakeFallback::unconfigured();
        let _ = p.update_state(&mut |s| primary_starting(s, 1));
        let _ = finish_primary_start(&p, Err(anyhow::anyhow!("boom")), 7);
        assert_eq!(p.load_state().starting_since, None);
    }

    fn intended() -> FallbackState {
        FallbackState {
            primary_intended: true,
            ..Default::default()
        }
    }

    #[test]
    fn provider_env_matches_hostname_not_substring() {
        assert_eq!(provider_env_for_host("https://openai.com.evil.io/v1"), None);
        assert_eq!(provider_env_for_host("https://evil.io/?x=openai.com"), None);
        assert_eq!(provider_env_for_host("https://notopenai.com"), None);
        assert_eq!(
            provider_env_for_host("https://user@api.openai.com:443/v1"),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            provider_env_for_host("openrouter.ai/api"),
            Some("OPENROUTER_API_KEY")
        );
    }

    #[test]
    fn intentional_restart_does_not_trigger_within_grace() {
        let mut s = intended();
        primary_starting(&mut s, 100);
        let mut c = ctx(false);
        c.now = 112;
        apply_health_probe(&mut s, &c);
        c.now = 124;
        apply_health_probe(&mut s, &c);
        c.now = 100 + START_GRACE_SECS - 1;
        apply_health_probe(&mut s, &c);
        assert!(!s.active);
        assert_eq!(s.health_failures, 0);
    }

    #[test]
    fn restart_that_never_settles_triggers_after_grace() {
        let mut s = intended();
        primary_starting(&mut s, 100);
        let mut c = ctx(false);
        c.now = 100 + START_GRACE_SECS;
        apply_health_probe(&mut s, &c);
        c.now += 12;
        assert!(apply_health_probe(&mut s, &c));
    }

    #[test]
    fn settle_started_stopped_and_activate_clear_starting() {
        let mut s = intended();
        primary_starting(&mut s, 1);
        primary_start_settled(&mut s);
        assert_eq!(s.starting_since, None);
        assert!(s.primary_intended);
        primary_starting(&mut s, 2);
        primary_started(&mut s);
        assert_eq!(s.starting_since, None);
        primary_starting(&mut s, 3);
        primary_stopped(&mut s);
        assert_eq!(s.starting_since, None);
        primary_starting(&mut s, 4);
        activate(&mut s, FallbackReason::LoadFailed, "x", 5);
        assert_eq!(s.starting_since, None);
    }

    #[test]
    fn two_consecutive_failures_activate() {
        let mut s = intended();
        assert!(!apply_health_probe(&mut s, &ctx(false)));
        let mut c = ctx(false);
        c.now = 112; // next poll, 12 s later
        assert!(apply_health_probe(&mut s, &c));
        assert!(s.active);
        assert_eq!(s.reason, Some(FallbackReason::Health));
        assert_eq!(s.since, Some(112));
    }

    #[test]
    fn single_failure_then_success_resets() {
        let mut s = intended();
        apply_health_probe(&mut s, &ctx(false));
        apply_health_probe(&mut s, &ctx(true));
        assert!(!apply_health_probe(&mut s, &ctx(false)));
        assert!(!s.active);
    }

    #[test]
    fn intentional_stop_never_triggers() {
        let mut s = FallbackState::default();
        apply_health_probe(&mut s, &ctx(false));
        apply_health_probe(&mut s, &ctx(false));
        assert!(!s.active);
        assert_eq!(s.health_failures, 0);
    }

    #[test]
    fn paused_does_not_count() {
        let mut s = intended();
        let mut c = ctx(false);
        c.paused = true;
        apply_health_probe(&mut s, &c);
        apply_health_probe(&mut s, &c);
        assert!(!s.active);
        assert_eq!(s.health_failures, 0);
    }

    #[test]
    fn no_counting_without_sessions() {
        let mut s = intended();
        let mut c = ctx(false);
        c.sessions = 0;
        apply_health_probe(&mut s, &c);
        apply_health_probe(&mut s, &c);
        assert_eq!(s.health_failures, 0);
    }

    #[test]
    fn unconfigured_counts_but_never_activates() {
        let mut s = intended();
        let mut c = ctx(false);
        c.configured = false;
        apply_health_probe(&mut s, &c);
        apply_health_probe(&mut s, &c);
        apply_health_probe(&mut s, &c);
        assert!(!s.active);
    }

    #[test]
    fn started_and_stopped_reset_state() {
        let mut s = intended();
        activate(&mut s, FallbackReason::LoadFailed, "x", 5);
        primary_started(&mut s);
        assert!(s.primary_intended && !s.active && s.reason.is_none());
        activate(&mut s, FallbackReason::Health, "y", 6);
        primary_stopped(&mut s);
        assert!(!s.primary_intended && !s.active);
    }

    #[test]
    fn fallback_client_config_talks_openai_to_the_fallback_host() {
        let mut primary = SupervisorConfig::default();
        primary.llm_container.mode = "on".into();
        let fb = FallbackConfig {
            host: "https://openrouter.ai/api".into(),
            model: "openai/gpt-4.1-mini".into(),
        };
        let c = fallback_client_config(&primary, &fb, "sk".into());
        assert!(!c.container_mode_active());
        assert_eq!(
            c.effective_backend(),
            crate::supervisor::config::LlmBackendKind::OpenAiCompatible
        );
        assert_eq!(c.effective_primary_host(), "https://openrouter.ai/api");
        assert_eq!(c.ollama_model, "openai/gpt-4.1-mini");
        assert_eq!(c.ollama_fallback_host, None);
    }

    #[test]
    fn redacting_backend_strips_key_from_errors() {
        struct FailWithSecret;
        impl crate::supervisor::ollama::CognitiveBackend for FailWithSecret {
            fn evaluate(
                &self,
                _state: &crate::supervisor::state::AgentDurableState,
                _recent_output: &str,
            ) -> Result<crate::supervisor::ollama::CognitiveEvaluation> {
                anyhow::bail!("401 for Bearer sk-secret");
            }
            fn unload_model(&self) -> Result<()> {
                Ok(())
            }
        }
        let b = RedactingBackend {
            inner: Box::new(FailWithSecret),
            key: "sk-secret".into(),
        };
        let st = crate::supervisor::state::AgentDurableState::new("a", "n", 1, 0);
        let err = b.evaluate(&st, "out").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("***"));
        assert!(!msg.contains("sk-secret"));
    }

    #[test]
    fn snapshot_info_has_no_key_and_requires_key_for_configured() {
        let cfg = FallbackConfig {
            host: "h".into(),
            model: "m".into(),
        };
        let info = fallback_snapshot_info(&cfg, false, &FallbackState::default());
        assert!(!info.configured);
        let json = serde_json::to_string(&fallback_snapshot_info(
            &cfg,
            true,
            &FallbackState::default(),
        ))
        .unwrap();
        assert!(!json.contains("key\":\""));
    }

    struct Task5FakeFallback {
        state: std::sync::Mutex<FallbackState>,
        model: Option<String>,
    }

    impl Task5FakeFallback {
        fn configured(model: &str) -> Self {
            Self {
                state: std::sync::Mutex::new(FallbackState::default()),
                model: Some(model.to_string()),
            }
        }
        fn unconfigured() -> Self {
            Self {
                state: std::sync::Mutex::new(FallbackState::default()),
                model: None,
            }
        }
        fn set_active(&self) {
            let mut g = self.state.lock().unwrap();
            g.primary_intended = true;
            g.active = true;
            g.reason = Some(FallbackReason::Health);
            g.since = Some(1);
        }
    }

    impl FallbackProvider for Task5FakeFallback {
        fn load_state(&self) -> FallbackState {
            self.state.lock().unwrap().clone()
        }
        fn update_state(&self, f: &mut dyn FnMut(&mut FallbackState)) -> Result<FallbackState> {
            let mut g = self.state.lock().unwrap();
            f(&mut g);
            Ok(g.clone())
        }
        fn probe_primary(&self) -> (bool, String) {
            (true, String::new())
        }
        fn backend(&self) -> Option<Box<dyn CognitiveBackend>> {
            let m = self.model.clone()?;
            struct DummyBackend(String);
            impl CognitiveBackend for DummyBackend {
                fn evaluate(
                    &self,
                    _state: &AgentDurableState,
                    _recent_output: &str,
                ) -> Result<CognitiveEvaluation> {
                    unreachable!()
                }
                fn unload_model(&self) -> Result<()> {
                    Ok(())
                }
                fn model_label(&self) -> String {
                    self.0.clone()
                }
            }
            Some(Box::new(DummyBackend(m)))
        }
    }

    #[test]
    fn failed_start_activates_when_configured() {
        let p = Task5FakeFallback::configured("openai/gpt-4.1-mini");
        let err = finish_primary_start(
            &p,
            Err(anyhow::anyhow!("Container llama.cpp não respondeu")),
            7,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("fallback ativo"));
        let s = p.load_state();
        assert!(s.active && s.primary_intended);
        assert_eq!(s.reason, Some(FallbackReason::LoadFailed));
    }

    #[test]
    fn failed_start_unconfigured_returns_original_error() {
        let p = Task5FakeFallback::unconfigured();
        let err = finish_primary_start(&p, Err(anyhow::anyhow!("boom")), 7).unwrap_err();
        assert_eq!(format!("{err:#}"), "boom");
        assert!(!p.load_state().active);
    }

    #[test]
    fn successful_start_clears_active() {
        let p = Task5FakeFallback::configured("m");
        p.set_active();
        finish_primary_start(&p, Ok(()), 8).unwrap();
        assert!(!p.load_state().active);
    }

    #[test]
    fn failed_return_keeps_fallback_and_updates_detail() {
        let p = Task5FakeFallback::configured("m");
        p.set_active();
        let _ = finish_primary_start(&p, Err(anyhow::anyhow!("ainda quebrado")), 9);
        let s = p.load_state();
        assert!(s.active);
        assert!(s.detail.unwrap().contains("ainda quebrado"));
    }

    #[test]
    fn models_url_and_parsing() {
        assert_eq!(
            models_url("https://openrouter.ai/api"),
            "https://openrouter.ai/api/v1/models"
        );
        assert_eq!(
            models_url("https://opencode.ai/inference/go/openai/v1"),
            "https://opencode.ai/inference/go/openai/v1/models"
        );
        let v = serde_json::json!({"data":[{"id":"b"},{"id":"a"},{"id":"a"}]});
        assert_eq!(parse_models_list(&v), vec!["a", "b"]);
        assert_eq!(
            filter_models(&["GPT-4.1-mini".into(), "glm".into()], Some("gpt")),
            vec!["GPT-4.1-mini"]
        );
        assert!(models_http_error(401).contains("401"));
        assert!(models_http_error(404).contains("404"));
    }

    #[test]
    fn key_from_reader_trims_and_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fallback_api_key");
        write_key_from_reader(&p, std::io::Cursor::new("  sk-reader \n")).unwrap();
        assert_eq!(read_key_file(&p).as_deref(), Some("sk-reader"));
        assert!(write_key_from_reader(&p, std::io::Cursor::new("   \n")).is_err());
    }
}
