use crate::home::unix_now_secs;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UsageBarSnapshot {
    pub installed: bool,
    pub binary_path: Option<String>,
    pub tui_binary_path: Option<String>,
    pub fetched_at_secs: u64,
    pub error: Option<String>,
    pub primary: Option<String>,
    pub overview_vendors: Option<Vec<String>>,
    pub entries: Vec<serde_json::Value>,
    pub vendors: Vec<serde_json::Value>,
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn candidate_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path_var) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path_var));
    }
    if let Ok(home) = env::var("HOME") {
        let home_path = PathBuf::from(home);
        dirs.push(home_path.join(".cargo/bin"));
        dirs.push(home_path.join(".local/bin"));
        dirs.push(home_path.join(".nix-profile/bin"));
    }
    if let Ok(user) = env::var("USER") {
        if !user.trim().is_empty() {
            dirs.push(PathBuf::from(format!("/etc/profiles/per-user/{user}/bin")));
        }
    }
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/usr/bin"));
    dirs
}

fn find_binary(env_suffix: &str, bin_name: &str) -> Option<PathBuf> {
    if let Some(raw) = crate::envvars::var(env_suffix) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if is_executable_file(&p) {
                return Some(p);
            }
        }
    }
    for dir in candidate_bin_dirs() {
        let candidate = dir.join(bin_name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub fn find_ai_usagebar_bin() -> Option<PathBuf> {
    find_binary("AI_USAGEBAR_BIN", "ai-usagebar")
}

pub fn find_ai_usagebar_tui_bin() -> Option<PathBuf> {
    find_binary("AI_USAGEBAR_TUI_BIN", "ai-usagebar-tui")
}

struct CmdOutput {
    success: bool,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn run_cmd_with_timeout(bin: &Path, args: &[&str], timeout: Duration) -> Result<CmdOutput, String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed spawning {}: {e}", bin.display()))?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut out) = stdout_pipe {
            let _ = out.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut err) = stderr_pipe {
            let _ = err.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break None,
        }
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    Ok(CmdOutput {
        success: status.is_some_and(|s| s.success()),
        stdout,
        stderr,
        timed_out,
    })
}

/// Parsed `usage.json` shape: `(primary vendor, overview vendors, raw usage entries)`.
pub type UsageDocument = (Option<String>, Option<Vec<String>>, Vec<serde_json::Value>);

pub fn parse_usage_document(raw: &str) -> Result<UsageDocument, String> {
    let val: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|e| format!("invalid usage JSON: {e}"))?;
    let primary = val
        .get("primary")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let overview_vendors = val
        .get("overview_vendors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        });
    let entries = val
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok((primary, overview_vendors, entries))
}

pub fn parse_vendors_document(raw: &str) -> Result<Vec<serde_json::Value>, String> {
    let val: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|e| format!("invalid vendors JSON: {e}"))?;
    if let Some(arr) = val.as_array() {
        return Ok(arr.clone());
    }
    if let Some(arr) = val.get("vendors").and_then(|v| v.as_array()) {
        return Ok(arr.clone());
    }
    Err("vendors JSON is not an array".to_string())
}

pub fn collect_usage_snapshot(detect_first: bool) -> UsageBarSnapshot {
    let fetched_at_secs = unix_now_secs();
    let tui_binary_path = find_ai_usagebar_tui_bin().map(|p| p.to_string_lossy().into_owned());
    let Some(bin) = find_ai_usagebar_bin() else {
        return UsageBarSnapshot {
            installed: false,
            binary_path: None,
            tui_binary_path,
            fetched_at_secs,
            error: Some(
                "ai-usagebar não encontrado no sistema ($PATH, ~/.cargo/bin, ~/.local/bin). Instale com `cargo binstall ai-usagebar` ou `yay -S ai-usagebar-bin`."
                    .to_string(),
            ),
            primary: None,
            overview_vendors: None,
            entries: Vec::new(),
            vendors: Vec::new(),
        };
    };

    let binary_path = Some(bin.to_string_lossy().into_owned());

    if detect_first {
        let try_json = run_cmd_with_timeout(&bin, &["detect", "--json"], Duration::from_secs(10));
        if !matches!(try_json, Ok(ref o) if o.success) {
            let _ = run_cmd_with_timeout(&bin, &["detect"], Duration::from_secs(10));
        }
    }

    let vendors = match run_cmd_with_timeout(&bin, &["vendors", "--json"], Duration::from_secs(5)) {
        Ok(out) if !out.stdout.trim().is_empty() => {
            parse_vendors_document(&out.stdout).unwrap_or_default()
        }
        _ => Vec::new(),
    };

    let mut snap = UsageBarSnapshot {
        installed: true,
        binary_path,
        tui_binary_path,
        fetched_at_secs,
        error: None,
        primary: None,
        overview_vendors: None,
        entries: Vec::new(),
        vendors,
    };

    match run_cmd_with_timeout(&bin, &["usage", "--json"], Duration::from_secs(15)) {
        Ok(out) => {
            if out.timed_out {
                snap.error = Some("ai-usagebar usage --json timed out after 15s".to_string());
                return snap;
            }
            if !out.stdout.trim().is_empty() {
                match parse_usage_document(&out.stdout) {
                    Ok((primary, overview, entries)) => {
                        snap.primary = primary;
                        snap.overview_vendors = overview;
                        snap.entries = entries;
                        if !out.success && snap.entries.is_empty() && !out.stderr.trim().is_empty()
                        {
                            snap.error = Some(out.stderr.trim().to_string());
                        }
                    }
                    Err(e) => {
                        let stderr_clean = out.stderr.trim();
                        snap.error = Some(if !stderr_clean.is_empty() {
                            stderr_clean.to_string()
                        } else {
                            e
                        });
                    }
                }
            } else {
                let stderr_clean = out.stderr.trim();
                snap.error = Some(if !stderr_clean.is_empty() {
                    stderr_clean.to_string()
                } else {
                    "ai-usagebar usage --json returned empty output".to_string()
                });
            }
        }
        Err(e) => {
            snap.error = Some(e);
        }
    }

    snap
}

pub fn format_human_usage(snap: &UsageBarSnapshot) -> String {
    let mut lines = Vec::new();
    if !snap.installed {
        lines.push(format!(
            "ai-usagebar: not installed ({})",
            snap.error.as_deref().unwrap_or("binary not found")
        ));
        return lines.join("\n");
    }

    lines.push(format!(
        "ai-usagebar ({}) — {} entries, {} catalog vendors{}",
        snap.binary_path.as_deref().unwrap_or("ai-usagebar"),
        snap.entries.len(),
        snap.vendors.len(),
        snap.primary
            .as_deref()
            .map(|p| format!(" | primary: {p}"))
            .unwrap_or_default()
    ));

    if let Some(err) = &snap.error {
        lines.push(format!("Warning/Error: {err}"));
    }

    for entry in &snap.entries {
        let name = entry
            .get("display_name")
            .or_else(|| entry.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let plan = entry
            .get("plan")
            .and_then(|v| v.as_str())
            .map(|p| format!(" [{p}]"))
            .unwrap_or_default();
        let status = entry
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        lines.push(format!("\n• {name}{plan} ({status})"));
        if let Some(err) = entry.get("error").and_then(|v| v.as_str()) {
            lines.push(format!("  error: {err}"));
        }
        if let Some(metrics) = entry.get("metrics").and_then(|v| v.as_array()) {
            for m in metrics {
                let label = m.get("label").and_then(|v| v.as_str()).unwrap_or("metric");
                let value = m.get("value").and_then(|v| v.as_str()).unwrap_or("");
                let pct = m.get("percent").and_then(|v| v.as_u64()).unwrap_or(0);
                let detail = m.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                lines.push(format!("  - {label}: {pct}% ({value}) — {detail}"));
            }
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_usage_json_payload() {
        let raw = r#"{
          "schema_version": 1,
          "primary": "anthropic@work",
          "overview_vendors": ["anthropic", "openai"],
          "entries": [
            {
              "id": "anthropic@work",
              "name": "anthropic · work",
              "display_name": "Claude · work",
              "short_name": "CLD",
              "icon": "🤖",
              "brand": "anthropic",
              "plan": "Max 20x",
              "status": "ready",
              "error": null,
              "stale": false,
              "fetched_at": "2026-11-14T15:50:00Z",
              "activity": { "working": 2, "waiting": 1 },
              "metrics": [
                {
                  "label": "5-Hour Window",
                  "percent": 42,
                  "value": "42%",
                  "detail": "Resets in 3h 15m · 34% elapsed · 8pts over",
                  "headline": "percent",
                  "severity": "low",
                  "reset_at": "2026-11-14T19:05:00Z",
                  "window_secs": 18000
                }
              ],
              "sections": [
                {
                  "type": "metric",
                  "label": "5-Hour Window",
                  "percent": 42,
                  "value": "42%",
                  "detail": "Resets in 3h 15m · 34% elapsed · 8pts over",
                  "headline": "percent",
                  "severity": "low",
                  "reset_at": "2026-11-14T19:05:00Z",
                  "window_secs": 18000
                },
                {
                  "type": "text",
                  "label": "Activity",
                  "value": "2 working · 1 waiting"
                },
                {
                  "type": "block",
                  "label": "Notes",
                  "body": ["Session quota healthy"]
                },
                {
                  "type": "spacer"
                }
              ]
            }
          ]
        }"#;

        let (primary, overview, entries) = parse_usage_document(raw).expect("valid usage json");
        assert_eq!(primary.as_deref(), Some("anthropic@work"));
        assert_eq!(
            overview,
            Some(vec!["anthropic".to_string(), "openai".to_string()])
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].get("short_name").and_then(|v| v.as_str()),
            Some("CLD")
        );
        assert_eq!(
            entries[0]
                .get("sections")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(4)
        );
    }

    #[test]
    fn parses_vendors_json_array() {
        let raw = r#"[
          {
            "id": "anthropic",
            "name": "Claude",
            "short_name": "CLD",
            "kind": "oauth",
            "enabled": true,
            "configured": true,
            "needs_credential": true,
            "env": "",
            "login": "claude"
          },
          {
            "id": "antigravity",
            "name": "Antigravity",
            "short_name": "AGY",
            "kind": "local",
            "enabled": true,
            "configured": true,
            "needs_credential": false,
            "env": "",
            "login": ""
          }
        ]"#;

        let vendors = parse_vendors_document(raw).expect("valid vendors json");
        assert_eq!(vendors.len(), 2);
        assert_eq!(
            vendors[0].get("id").and_then(|v| v.as_str()),
            Some("anthropic")
        );
        assert_eq!(
            vendors[1].get("needs_credential").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn graceful_fallback_when_binary_missing() {
        let snap = UsageBarSnapshot {
            installed: false,
            binary_path: None,
            tui_binary_path: None,
            fetched_at_secs: 100,
            error: Some("not installed".to_string()),
            primary: None,
            overview_vendors: None,
            entries: Vec::new(),
            vendors: Vec::new(),
        };
        let text = format_human_usage(&snap);
        assert!(text.contains("not installed"));
    }
}
