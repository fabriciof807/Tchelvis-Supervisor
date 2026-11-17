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
}

pub fn provider_env_for_host(host: &str) -> Option<&'static str> {
    let lower = host.to_ascii_lowercase();
    if lower.contains("openai.com") {
        Some("OPENAI_API_KEY")
    } else if lower.contains("openrouter.ai") {
        Some("OPENROUTER_API_KEY")
    } else if lower.contains("deepseek.com") {
        Some("DEEPSEEK_API_KEY")
    } else if lower.contains("opencode.ai") {
        Some("OPENCODE_API_KEY")
    } else {
        None
    }
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
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
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
}
