use crate::model::ProviderId;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JiraAuthMode {
    #[default]
    Basic,
    Bearer,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JiraConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub auth_mode: JiraAuthMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitlabConfig {
    #[serde(default = "default_gitlab_url")]
    pub base_url: String,
}

fn default_gitlab_url() -> String {
    "https://gitlab.com".to_string()
}

impl Default for GitlabConfig {
    fn default() -> Self {
        Self {
            base_url: default_gitlab_url(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderCredentialConfig {
    #[serde(default)]
    pub secret_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionConfig {
    #[serde(default = "default_finished_ttl_hours")]
    pub finished_ttl_hours: u64,
}

fn default_finished_ttl_hours() -> u64 {
    72
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            finished_ttl_hours: default_finished_ttl_hours(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CockpitConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub jira: JiraConfig,
    #[serde(default)]
    pub gitlab: GitlabConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCredentialConfig>,
    #[serde(default)]
    pub retention: RetentionConfig,
}

fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

impl Default for CockpitConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            jira: JiraConfig::default(),
            gitlab: GitlabConfig::default(),
            providers: BTreeMap::new(),
            retention: RetentionConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CockpitPaths {
    pub home_dir: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl CockpitPaths {
    pub fn resolve() -> Result<Self> {
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME environment variable is not set")?;
        Self::resolve_from_env(&home_dir)
    }

    pub fn resolve_from_env(home_dir: &Path) -> Result<Self> {
        let config_base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".config"));
        let data_base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".local/share"));
        let state_base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".local/state"));

        let config_dir = config_base.join("tchelvis");
        let data_dir = data_base.join("tchelvis");
        let state_dir = state_base.join("tchelvis");

        let runtime_dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(rt) if !rt.is_empty() => PathBuf::from(rt).join("tchelvis"),
            _ => state_dir.join("run"),
        };

        Ok(Self {
            home_dir: home_dir.to_path_buf(),
            config_dir,
            data_dir,
            state_dir,
            runtime_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        fs::create_dir_all(&self.data_dir)?;
        fs::create_dir_all(&self.state_dir)?;
        fs::create_dir_all(&self.runtime_dir)?;
        fs::set_permissions(&self.runtime_dir, fs::Permissions::from_mode(0o700))?;

        let meta = fs::metadata(&self.runtime_dir)?;
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            bail!(
                "Runtime directory {} is not owned by current UID {}",
                self.runtime_dir.display(),
                euid
            );
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn hooks_dir(&self) -> PathBuf {
        self.config_dir.join("hooks")
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("tchelvis.log")
    }

    pub fn setup_manifest_file(&self) -> PathBuf {
        self.state_dir.join("setup-manifest.json")
    }

    pub fn socket_file(&self) -> PathBuf {
        let candidate = self.runtime_dir.join("tchelvis.sock");
        if candidate.as_os_str().len() <= 100 {
            return candidate;
        }
        let short_candidate = self.home_dir.join("tchelvis.sock");
        if short_candidate.as_os_str().len() <= 100 {
            return short_candidate;
        }
        let euid = unsafe { libc::geteuid() };
        PathBuf::from(format!("/tmp/tchelvis-{euid}-{}.sock", std::process::id()))
    }

    pub fn lock_file(&self) -> PathBuf {
        self.runtime_dir.join("daemon.lock")
    }
}

impl CockpitConfig {
    pub fn load_or_default(paths: &CockpitPaths) -> Result<Self> {
        let cfg_path = paths.config_file();
        if !cfg_path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&cfg_path)
            .with_context(|| format!("failed reading {}", cfg_path.display()))?;
        Self::parse_and_validate(&raw)
    }

    pub fn parse_and_validate(raw: &str) -> Result<Self> {
        let val: toml::Value = toml::from_str(raw).context("invalid TOML in config.toml")?;
        Self::reject_plaintext_secrets(&val, "")?;
        let cfg: CockpitConfig =
            toml::from_str(raw).context("failed deserializing CockpitConfig")?;
        if cfg.schema_version > CONFIG_SCHEMA_VERSION {
            tracing::warn!(
                found = cfg.schema_version,
                supported = CONFIG_SCHEMA_VERSION,
                "config.toml has a newer schema_version; proceeding with compatible fields"
            );
        }
        Ok(cfg)
    }

    fn reject_plaintext_secrets(val: &toml::Value, path: &str) -> Result<()> {
        if let Some(table) = val.as_table() {
            for (k, v) in table {
                let lower = k.to_ascii_lowercase();
                let key_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if matches!(
                    lower.as_str(),
                    "token" | "api_key" | "apikey" | "password" | "secret" | "client_secret"
                ) {
                    bail!(
                        "Plaintext credential key '{}' is forbidden in config.toml; use environment variables or secret_command instead",
                        key_path
                    );
                }
                Self::reject_plaintext_secrets(v, &key_path)?;
            }
        }
        Ok(())
    }

    pub async fn resolve_token(&self, provider: ProviderId) -> Option<String> {
        let env_val = match provider {
            ProviderId::Github => std::env::var("GITHUB_TOKEN").ok(),
            ProviderId::Gitlab => std::env::var("GITLAB_TOKEN").ok(),
            ProviderId::Jira => crate::envvars::var("JIRA_TOKEN"),
            ProviderId::Linear => crate::envvars::var("LINEAR_TOKEN"),
            ProviderId::Slack => crate::envvars::var("SLACK_TOKEN"),
            ProviderId::Runbooks => return None,
        };

        if let Some(val) = env_val {
            let trimmed = val.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }

        if let Some(p_cfg) = self.providers.get(provider.as_str()) {
            if let Some(cmd_str) = &p_cfg.secret_command {
                if let Some(tok) = run_secret_command(cmd_str).await {
                    return Some(tok);
                }
            }
        }

        match provider {
            ProviderId::Github => {
                run_cli_token("gh", &["auth", "token"], &[("GH_PROMPT_DISABLED", "1")]).await
            }
            ProviderId::Gitlab => {
                run_cli_token("glab", &["auth", "token"], &[("GLAB_NO_PROMPT", "1")]).await
            }
            _ => None,
        }
    }
}

async fn run_secret_command(cmd_str: &str) -> Option<String> {
    let fut = Command::new("sh")
        .arg("-c")
        .arg(cmd_str)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(out)) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        _ => None,
    }
}

async fn run_cli_token(bin: &str, args: &[&str], envs: &[(&str, &str)]) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rejects_plaintext_token_in_config_toml() {
        let bad = r#"
            schema_version = 1
            [jira]
            base_url = "https://acme.atlassian.net"
            token = "secret-value"
        "#;
        let err = CockpitConfig::parse_and_validate(bad).unwrap_err();
        assert!(err.to_string().contains("Plaintext credential key"));
    }

    #[test]
    fn test_accepts_valid_config_and_unknown_fields() {
        let ok = r#"
            schema_version = 2
            future_flag = true
            [jira]
            base_url = "https://acme.atlassian.net"
            email = "dev@acme.com"
            auth_mode = "basic"
            [providers.jira]
            secret_command = "echo tok"
        "#;
        let cfg = CockpitConfig::parse_and_validate(ok).unwrap();
        assert_eq!(
            cfg.jira.base_url.as_deref(),
            Some("https://acme.atlassian.net")
        );
        assert_eq!(cfg.jira.auth_mode, JiraAuthMode::Basic);
    }

    #[test]
    fn test_runtime_dir_0700_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = CockpitPaths {
            home_dir: tmp.path().to_path_buf(),
            config_dir: tmp.path().join("config"),
            data_dir: tmp.path().join("data"),
            state_dir: tmp.path().join("state"),
            runtime_dir: tmp.path().join("state/run"),
        };
        paths.ensure_dirs().unwrap();
        let mode = fs::metadata(&paths.runtime_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
}
