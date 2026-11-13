use anyhow::Result;
use regex::Regex;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

pub const MAX_LOG_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)(Authorization\s*:\s*(?:Bearer|Basic|token)\s+)[^\s,\x22\x27]+")
            .expect("valid auth regex"),
        Regex::new(r"\bghp_[A-Za-z0-9_]{16,}\b").expect("valid ghp regex"),
        Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{16,}\b").expect("valid github_pat regex"),
        Regex::new(r"\bglpat-[A-Za-z0-9_\-]{10,}\b").expect("valid glpat regex"),
        Regex::new(r"\blin_api_[A-Za-z0-9]{12,}\b").expect("valid lin_api regex"),
        Regex::new(r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b").expect("valid slack regex"),
        Regex::new(r"\bATATT3x[A-Za-z0-9_\-=]{12,}\b").expect("valid jira pat regex"),
    ]
});

/// Scrubs Authorization headers and known API token formats from any log or diagnostic string.
pub fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    for (idx, re) in SECRET_PATTERNS.iter().enumerate() {
        if idx == 0 {
            out = re.replace_all(&out, "${1}[REDACTED]").into_owned();
        } else {
            out = re.replace_all(&out, "[REDACTED]").into_owned();
        }
    }
    out
}

#[derive(Clone)]
pub struct RotatingRedactingWriter {
    path: PathBuf,
    lock: std::sync::Arc<Mutex<()>>,
}

impl RotatingRedactingWriter {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: std::sync::Arc::new(Mutex::new(())),
        }
    }
}

pub struct RedactingGuard {
    path: PathBuf,
    lock: std::sync::Arc<Mutex<()>>,
    buf: Vec<u8>,
}

impl Write for RedactingGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for RedactingGuard {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let _guard = self.lock.lock().ok();
        let text = String::from_utf8_lossy(&self.buf);
        let redacted = redact_secrets(&text);

        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(meta) = fs::metadata(&self.path) {
            if meta.len() >= MAX_LOG_FILE_BYTES {
                let rotated = self.path.with_extension("log.1");
                let _ = fs::rename(&self.path, rotated);
            }
        }

        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(redacted.as_bytes());
        }
    }
}

impl<'a> MakeWriter<'a> for RotatingRedactingWriter {
    type Writer = RedactingGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingGuard {
            path: self.path.clone(),
            lock: self.lock.clone(),
            buf: Vec::with_capacity(256),
        }
    }
}

pub fn init_logging(log_path: &Path) -> Result<()> {
    let writer = RotatingRedactingWriter::new(log_path.to_path_buf());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .try_init();
    Ok(())
}

pub fn read_filtered_logs(
    log_path: &Path,
    agent_filter: Option<&str>,
    provider_filter: Option<&str>,
    max_lines: usize,
) -> Result<Vec<String>> {
    if !log_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(log_path)?;
    let mut matched: Vec<String> = content
        .lines()
        .filter(|line| {
            if let Some(agent) = agent_filter {
                if !line.contains(agent) {
                    return false;
                }
            }
            if let Some(prov) = provider_filter {
                if !line.contains(prov) {
                    return false;
                }
            }
            true
        })
        .map(redact_secrets)
        .collect();

    if matched.len() > max_lines {
        matched = matched.split_off(matched.len() - max_lines);
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_secrets_scrubs_all_provider_tokens() {
        let raw = "Authorization: Bearer ghp_1234567890abcdef1234 and glpat-abcdef123456 and lin_api_1234567890abcd and xoxb-12345-67890-abcdef and ATATT3xFfGF0123456789";
        let scrubbed = redact_secrets(raw);
        assert!(!scrubbed.contains("ghp_"));
        assert!(!scrubbed.contains("glpat-"));
        assert!(!scrubbed.contains("lin_api_"));
        assert!(!scrubbed.contains("xoxb-"));
        assert!(!scrubbed.contains("ATATT3x"));
        assert!(scrubbed.contains("Authorization: Bearer [REDACTED]"));
    }
}
