use super::config::HcAdapterConfig;
use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

/// Normalized representation of an active or recently transitioned `hc` session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HcSession {
    /// Address identifier in `hc`, e.g. `"t7"` or `"vps/t3"`.
    pub agent_id: String,
    /// Human-readable session name (`name`, `where`, or `agent`).
    pub session_name: String,
    /// Detected agent/program type reported by `hc` (e.g., `"pi"`, `"claude"`, `"zsh"`).
    pub agent_program: Option<String>,
    /// Working directory of the terminal.
    pub cwd: Option<String>,
    /// Terminal lifecycle state in `hc` (`"live"` or `"finished"`).
    pub state: String,
    /// Optional OSC 7501 status reported by `hc` (`"idle"`, `"working"`, `"done"`, `"blocked"`, `"error"`).
    pub status: Option<String>,
    /// Turn counter reported by `hc`.
    pub turn: Option<u64>,
    /// Kind of a blocked prompt (`"permission"`, `"question"`, …) from the OSC 7501 status or the
    /// `cockpit.kind` label.
    #[serde(default)]
    pub kind: Option<String>,
    /// The pending request as the agent reports it in the OSC 7501 status `msg`
    /// (Claude Code: `approve Bash: <cmd>`). Absent when the agent sends none.
    #[serde(default)]
    pub msg: Option<String>,
}

impl HcSession {
    /// Returns true if this terminal session is currently live.
    pub fn is_active(&self) -> bool {
        self.state == "live"
    }

    /// Returns true if the agent is genuinely blocked on a prompt awaiting input.
    pub fn is_blocked(&self) -> bool {
        matches!(self.status.as_deref(), Some("blocked") | Some("waiting"))
    }

    /// Returns true if `hc` reported a status transition to waiting/blocked, failed/error, or finished/done.
    pub fn has_notable_hc_status(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("blocked") | Some("error") | Some("done") | Some("waiting") | Some("failed")
        ) || self.state == "finished"
    }
}

/// Raw JSON output structure of `hc ls --json`.
#[derive(Debug, Clone, Deserialize)]
struct RawHcListOutput {
    #[serde(default)]
    terminals: Vec<RawHcTerminal>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawHcForeground {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawHcTerminal {
    address: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default = "default_live_state")]
    state: String,
    #[serde(default)]
    reported: bool,
    #[serde(default)]
    status: Option<serde_json::Value>,
    #[serde(default)]
    foreground: Option<RawHcForeground>,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    turn: Option<u64>,
    #[serde(rename = "where", default)]
    where_label: Option<String>,
}

fn default_live_state() -> String {
    "live".to_string()
}

/// Raw JSON output structure of `hc read <t> --lines N --json`.
#[derive(Debug, Clone, Deserialize)]
struct RawHcReadOutput {
    text: String,
}

/// Trait abstracting `hc` CLI operations so the Watcher can be tested deterministically
/// and run against the live `hc` binary in production.
pub trait HcBackend: Send + Sync {
    /// Lists active sessions via `hc ls --json`.
    fn list_sessions(&self) -> Result<Vec<HcSession>>;
    /// Captures the last `lines` of terminal output for `agent_id` via `hc read`.
    fn capture_output(&self, agent_id: &str, lines: usize) -> Result<String>;
    /// Sends input text + Enter to `agent_id` via `hc send`.
    fn send_input(&self, agent_id: &str, payload: &str, force: bool) -> Result<()>;
    /// Sends a named key (e.g. `"enter"`, `"esc"`) to `agent_id` via `hc keys`.
    fn send_keys(&self, agent_id: &str, key: &str) -> Result<()>;
    /// Returns `(is_blocked, pending_msg)` for `agent_id` from `hc`.
    fn pending_prompt_info(&self, agent_id: &str) -> Result<(bool, String)> {
        let sessions = self.list_sessions()?;
        let Some(s) = sessions.into_iter().find(|s| s.agent_id == agent_id) else {
            return Ok((false, String::new()));
        };
        Ok((s.is_blocked(), String::new()))
    }
    /// Sets metadata labels on an `hc` terminal (`hc label <agent_id> k=v...`).
    fn set_labels(&self, agent_id: &str, labels: &[(&str, &str)]) -> Result<()>;
}

/// Production `hc` CLI adapter bound to the resolved `hc_bin` (via `default_hc_bin`).
#[derive(Debug, Clone)]
pub struct HcCliAdapter {
    pub config: HcAdapterConfig,
    /// Terminal ID of the supervisor itself (from `HC_TERMINAL_ID`), excluded from self-monitoring.
    pub self_terminal_id: Option<String>,
}

impl HcCliAdapter {
    pub fn new(config: HcAdapterConfig) -> Self {
        let self_terminal_id = env::var("HC_TERMINAL_ID").ok();
        Self {
            config,
            self_terminal_id,
        }
    }

    /// Marks the supervisor's own terminal in `hc` if running inside one.
    pub fn register_self_terminal(&self) {
        if let Some(ref term_id) = self.self_terminal_id {
            let _ = Command::new(&self.config.hc_bin)
                .args(["label", term_id, "supervisor.role=self"])
                .output();
        }
    }
}

impl HcCliAdapter {
    fn list_sessions_via_connect(&self) -> Result<Vec<HcSession>> {
        let mut child = Command::new(&self.config.hc_bin)
            .arg("connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("Failed to spawn `{} connect`", self.config.hc_bin))?;

        if let Some(mut stdin) = child.stdin.take() {
            let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"terminal.list\",\"params\":{\"all\":false}}\n";
            stdin.write_all(req)?;
            let _ = stdin.flush();
        }

        let mut line = String::new();
        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout);
            reader.read_line(&mut line)?;
        }
        let _ = child.kill();
        let _ = child.wait();

        if line.trim().is_empty() {
            return Err(anyhow!("`hc connect` returned empty response"));
        }
        parse_hc_ls_json(&line, self.self_terminal_id.as_deref())
    }
}

impl HcBackend for HcCliAdapter {
    fn list_sessions(&self) -> Result<Vec<HcSession>> {
        if let Ok(sessions) = self.list_sessions_via_connect() {
            return Ok(sessions);
        }

        let output = Command::new(&self.config.hc_bin)
            .args(["ls", "--json"])
            .output()
            .with_context(|| format!("Failed to execute `{} ls --json`", self.config.hc_bin))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`hc ls --json` exited with {}: {}",
                output.status,
                stderr
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_hc_ls_json(&stdout, self.self_terminal_id.as_deref())
    }

    fn capture_output(&self, agent_id: &str, lines: usize) -> Result<String> {
        let output = Command::new(&self.config.hc_bin)
            .args(["read", agent_id, "--lines", &lines.to_string(), "--json"])
            .output()
            .with_context(|| {
                format!("Failed to execute `{} read {agent_id}`", self.config.hc_bin)
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`hc read {agent_id}` exited with {}: {}",
                output.status,
                stderr
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        extract_tail_lines(&stdout, lines)
    }

    fn send_input(&self, agent_id: &str, payload: &str, force: bool) -> Result<()> {
        let mut cmd = Command::new(&self.config.hc_bin);
        cmd.arg("send");
        if force {
            cmd.arg("--force");
        }
        cmd.arg(agent_id).arg(payload).arg("--json");

        let output = cmd.output().with_context(|| {
            format!("Failed to execute `{} send {agent_id}`", self.config.hc_bin)
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`hc send {agent_id}` exited with {}: {}",
                output.status,
                stderr
            ));
        }
        Ok(())
    }

    fn send_keys(&self, agent_id: &str, key: &str) -> Result<()> {
        let output = Command::new(&self.config.hc_bin)
            .args(["keys", agent_id, key])
            .output()
            .with_context(|| {
                format!(
                    "Failed to execute `{} keys {agent_id} {key}`",
                    self.config.hc_bin
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`hc keys {agent_id} {key}` exited with {}: {}",
                output.status,
                stderr
            ));
        }
        Ok(())
    }

    fn pending_prompt_info(&self, agent_id: &str) -> Result<(bool, String)> {
        let output = Command::new(&self.config.hc_bin)
            .args(["ls", "--json"])
            .output()
            .with_context(|| format!("Failed to execute `{} ls --json`", self.config.hc_bin))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`hc ls --json` exited with {}: {}",
                output.status,
                stderr
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: RawHcListOutput = serde_json::from_str(&stdout)
            .with_context(|| "Failed to parse `hc ls --json` output")?;
        let Some(term) = parsed.terminals.into_iter().find(|t| t.address == agent_id) else {
            return Ok((false, String::new()));
        };
        if term.state == "finished" {
            return Ok((false, String::new()));
        }
        let (status_state, _) = extract_status_fields(term.status.as_ref());
        let label_state = term
            .labels
            .get("cockpit.state")
            .map(|s| s.trim().to_ascii_lowercase());
        let is_blocked = matches!(status_state.as_deref(), Some("blocked" | "waiting"))
            || matches!(label_state.as_deref(), Some("blocked" | "waiting"));
        let msg = term
            .status
            .as_ref()
            .and_then(|v| v.get("msg").or_else(|| v.get("message")))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                term.labels
                    .get("cockpit.msg")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
        Ok((is_blocked, msg))
    }

    fn set_labels(&self, agent_id: &str, labels: &[(&str, &str)]) -> Result<()> {
        if !self.config.sync_hc_labels || labels.is_empty() {
            return Ok(());
        }
        let mut cmd = Command::new(&self.config.hc_bin);
        cmd.arg("label").arg(agent_id);
        for (k, v) in labels {
            cmd.arg(format!("{k}={v}"));
        }
        let _ = cmd.output();
        Ok(())
    }
}

fn extract_status_fields(status: Option<&serde_json::Value>) -> (Option<String>, Option<String>) {
    let Some(val) = status else {
        return (None, None);
    };
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        return ((!trimmed.is_empty()).then(|| trimmed.to_string()), None);
    }
    if let Some(obj) = val.as_object() {
        let state = obj
            .get("state")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let app = obj
            .get("app")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        return (state, app);
    }
    (None, None)
}

/// Returns `true` if the `hc` terminal represents an active coding agent session rather than
/// an idle plain shell or a supervisor/tchelvis UI process.
#[allow(clippy::too_many_arguments)]
pub fn is_coding_agent_terminal(
    agent: Option<&str>,
    name: Option<&str>,
    foreground: Option<&RawHcForeground>,
    argv: &[String],
    reported: bool,
    status_state: Option<&str>,
    status_app: Option<&str>,
    labels: &BTreeMap<String, String>,
) -> bool {
    let fg_name = foreground.and_then(|f| f.name.as_deref());
    if agent.is_some_and(crate::agents::is_supervisor_program)
        || name.is_some_and(crate::agents::is_supervisor_program)
        || fg_name.is_some_and(crate::agents::is_supervisor_program)
    {
        return false;
    }

    let fg_argv = foreground.map(|f| f.argv.as_slice()).unwrap_or(&[]);
    if crate::agents::is_attach_client_invocation(fg_name, fg_argv)
        || crate::agents::is_attach_client_invocation(None, argv)
    {
        return false;
    }

    let mut candidates: Vec<&str> = Vec::new();
    if let Some(lbl) = labels.get("cockpit.agent").map(String::as_str) {
        candidates.push(lbl);
    }
    if let Some(fg) = fg_name {
        candidates.push(fg);
    }
    if let Some(fg) = foreground {
        for arg in &fg.argv {
            candidates.push(arg.as_str());
        }
    }
    if let Some(ag) = agent {
        candidates.push(ag);
    }
    for arg in argv {
        candidates.push(arg.as_str());
    }
    if let Some(app) = status_app {
        candidates.push(app);
    }

    if candidates.into_iter().any(crate::agents::is_known_agent) {
        return true;
    }

    let has_active_status = reported
        || matches!(
            status_state,
            Some("working") | Some("blocked") | Some("done") | Some("error") | Some("waiting")
        );
    if has_active_status {
        let primary = fg_name.or(agent);
        if !primary.is_some_and(crate::agents::is_plain_shell) {
            return true;
        }
    }

    if labels.keys().any(|k| {
        matches!(
            k.as_str(),
            "cockpit.issue" | "cockpit.pr" | "cockpit.slack" | "cockpit.runbook"
        )
    }) {
        return true;
    }
    if let Some(agent_lbl) = labels.get("cockpit.agent") {
        let norm = agent_lbl.trim().to_ascii_lowercase();
        if !norm.is_empty() && norm != "shell" {
            return true;
        }
    }

    false
}

/// Parses `hc ls --json` output into normalized `HcSession` items, filtering out finished
/// terminals, plain shells without coding agents, and the supervisor's own terminal.
pub fn parse_hc_ls_json(raw_json: &str, self_terminal_id: Option<&str>) -> Result<Vec<HcSession>> {
    let val: serde_json::Value =
        serde_json::from_str(raw_json.trim()).context("Failed to parse `hc ls --json` output")?;
    let parsed: RawHcListOutput = if val.get("result").and_then(|r| r.get("terminals")).is_some() {
        serde_json::from_value(val["result"].clone())
            .context("Failed to parse `hc connect` terminal.list result")?
    } else {
        serde_json::from_value(val).context("Failed to parse `hc ls --json` terminals")?
    };

    let mut sessions = Vec::new();
    for term in parsed.terminals {
        if let Some(self_id) = self_terminal_id {
            if term.address == self_id {
                continue;
            }
        }
        if term.state != "live" {
            continue;
        }

        let (raw_status_state, status_app) = extract_status_fields(term.status.as_ref());
        let (fg_name, fg_argv) = match &term.foreground {
            Some(fg) => (fg.name.as_deref(), fg.argv.as_slice()),
            None => (None, &[][..]),
        };
        if term.foreground.is_some()
            && crate::agents::is_plain_shell_foreground_parts(
                &term.argv,
                fg_name,
                fg_argv,
                raw_status_state.as_deref(),
                status_app.as_deref(),
            )
        {
            continue;
        }
        let status_state = match raw_status_state.as_deref() {
            Some(s) if !s.is_empty() && s != "idle" => raw_status_state,
            _ => term
                .labels
                .get("cockpit.state")
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .or(raw_status_state),
        };

        let title_indicates_pi = term
            .title
            .as_deref()
            .is_some_and(crate::agents::is_pi_title);
        let mut effective_labels = term.labels.clone();
        if !effective_labels.contains_key("cockpit.agent") && title_indicates_pi {
            effective_labels.insert("cockpit.agent".to_string(), "pi".to_string());
        }

        if !is_coding_agent_terminal(
            term.agent.as_deref(),
            term.name.as_deref(),
            term.foreground.as_ref(),
            &term.argv,
            term.reported,
            status_state.as_deref(),
            status_app.as_deref(),
            &effective_labels,
        ) {
            continue;
        }

        let base_name = term
            .name
            .clone()
            .or_else(|| term.where_label.clone())
            .or_else(|| {
                term.cwd
                    .as_deref()
                    .and_then(|p| p.rsplit('/').next())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| term.address.clone());

        let agent_label = term
            .labels
            .get("cockpit.agent")
            .cloned()
            .or_else(|| status_app.clone())
            .or_else(|| {
                term.foreground
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .filter(|n| !crate::agents::is_plain_shell(n))
            })
            .or_else(|| {
                term.agent
                    .clone()
                    .filter(|n| !crate::agents::is_plain_shell(n))
            })
            .or_else(|| {
                if term
                    .title
                    .as_deref()
                    .is_some_and(crate::agents::is_pi_title)
                {
                    Some("pi".to_string())
                } else {
                    None
                }
            })
            .or_else(|| term.agent.clone());

        let session_name = match &agent_label {
            Some(agent) if !base_name.contains(agent) => format!("{base_name} ({agent})"),
            _ => base_name,
        };

        let kind = term
            .status
            .as_ref()
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str())
            .or_else(|| term.labels.get("cockpit.kind").map(String::as_str))
            .map(|k| k.trim().to_ascii_lowercase())
            .filter(|k| !k.is_empty());

        let msg = term
            .status
            .as_ref()
            .and_then(|v| v.get("msg"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(ToString::to_string);

        sessions.push(HcSession {
            agent_id: term.address,
            session_name,
            agent_program: agent_label,
            cwd: term.cwd,
            state: term.state,
            status: status_state,
            turn: term.turn,
            kind,
            msg,
        });
    }

    Ok(sessions)
}

/// Returns `true` if a line contains only whitespace or Unicode/ASCII box-drawing border characters.
pub fn is_blank_or_box_border_only(line: &str) -> bool {
    line.chars()
        .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c) || c == '|')
}

static SUPERVISOR_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^t\w+\s+\(.+\)\s+\[(WORKING|BLOCKED|IDLE|COMPLETED)\]$")
        .expect("valid supervisor header regex")
});

fn is_supervisor_tui_noise(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("Monitorando sessão ativa do agente.")
        || trimmed.contains("Nenhuma auto-resposta registrada")
        || trimmed.contains("OUTPUT PREVIEW (LAST 25 LINES)")
        || trimmed.contains("AUTO-REPLY AUDIT LOG")
        || trimmed.contains("DURABLE SUPERVISOR")
        || SUPERVISOR_HEADER_RE.is_match(trimmed)
}

fn clean_tui_line(line: &str) -> &str {
    let trimmed = line.trim();
    if trimmed.starts_with('│') || trimmed.ends_with('│') {
        trimmed.trim_start_matches('│').trim_end_matches('│').trim()
    } else {
        line.trim_end()
    }
}

/// Extracts the last `max_lines` from either `hc read --json` (`{"text": "..."}`) or raw text,
/// stripping empty lines, Ratatui/TUI box-drawing border lines, and leftover supervisor TUI lines.
pub fn extract_tail_lines(raw_output: &str, max_lines: usize) -> Result<String> {
    let text = if let Ok(parsed) = serde_json::from_str::<RawHcReadOutput>(raw_output.trim()) {
        parsed.text
    } else {
        raw_output.to_string()
    };

    let meaningful_lines: Vec<&str> = text
        .lines()
        .map(clean_tui_line)
        .filter(|line| !is_blank_or_box_border_only(line) && !is_supervisor_tui_noise(line))
        .collect();

    let start = meaningful_lines.len().saturating_sub(max_lines);
    Ok(meaningful_lines[start..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hc_ls_json_filters_self_finished_and_plain_shells() {
        let sample = r#"{
            "hidden_finished": 5,
            "terminals": [
                {"address":"t7","agent":"pi","cwd":"/home/ubuntu/projetos/teste","state":"live","status":"done","turn":2,"where":"teste / 1"},
                {"address":"t8","agent":"zsh","cwd":"/home/ubuntu/projetos/teste","state":"finished","turn":5,"where":"teste / 1"},
                {"address":"t9","agent":"herdr-supervisor","name":"herdr-supervisor","state":"live"},
                {"address":"t10","agent":"claude","state":"live","where":"auth-service"},
                {"address":"t11","agent":"zsh","cwd":"/home/ubuntu/projetos/teste","state":"live","turn":5,"where":"teste / 1"},
                {"address":"t12","agent":"bash","cwd":"/home/ubuntu/projetos/foo","state":"live","status":"working"},
                {"address":"t13","agent":"zsh","state":"live","foreground":{"name":"opencode","argv":["opencode"]},"where":"ops"},
                {"address":"t14","state":"live","status":{"state":"working","app":"codex"},"where":"api"},
                {"address":"t15","agent":"zsh","state":"live","status":{"state":"working","app":"antigravity"},"where":"web"}
            ]
        }"#;

        let sessions = parse_hc_ls_json(sample, Some("t10")).unwrap();
        assert_eq!(sessions.len(), 4);
        assert_eq!(sessions[0].agent_id, "t7");
        assert_eq!(sessions[0].session_name, "teste / 1 (pi)");
        assert_eq!(sessions[0].status.as_deref(), Some("done"));
        assert_eq!(sessions[1].agent_id, "t13");
        assert_eq!(sessions[2].agent_id, "t14");
        assert_eq!(sessions[2].status.as_deref(), Some("working"));
        assert_eq!(sessions[2].agent_program.as_deref(), Some("codex"));
        assert_eq!(sessions[3].agent_id, "t15");
        assert_eq!(sessions[3].status.as_deref(), Some("working"));
        assert_eq!(sessions[3].agent_program.as_deref(), Some("antigravity"));
    }

    #[test]
    fn test_extract_tail_lines_from_hc_read_json() {
        let raw =
            r#"{"complete":true,"how":"screen","text":"line 1\nline 2\nline 3\nline 4\n\n\n"}"#;
        let tail = extract_tail_lines(raw, 2).unwrap();
        assert_eq!(tail, "line 3\nline 4");
    }

    #[test]
    fn test_extract_tail_lines_strips_empty_box_borders() {
        let raw = "┌───────────────────────────────────────┐\n\
                   │ Monitorando sessão ativa do agente.   │\n\
                   │ t9g (ant-game / 1 (zsh)) [WORKING]    │\n\
                   │ Running cargo test...                 │\n\
                   │ test result: ok. 42 passed            │\n\
                   │                                       │\n\
                   └───────────────────────────────────────┘";
        let tail = extract_tail_lines(raw, 25).unwrap();
        assert_eq!(tail, "Running cargo test...\ntest result: ok. 42 passed");
    }

    #[test]
    fn test_parse_hc_ls_json_filters_plain_shell_with_stale_agent_labels() {
        let sample = r#"{
            "terminals": [
                {
                    "address": "t7",
                    "state": "live",
                    "foreground": {"name": "zsh", "argv": ["/usr/bin/zsh"]},
                    "status": null,
                    "labels": {"cockpit.agent": "pi", "cockpit.state": "working"}
                }
            ]
        }"#;
        let sessions = parse_hc_ls_json(sample, None).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_parse_hc_ls_json_reads_status_msg() {
        // Claude Code reports the pending request in OSC 7501 `msg` (seen on the host, test 8).
        let sample = r#"{"terminals": [
            {"address": "t14c", "state": "live", "reported": true,
             "status": {"state": "blocked", "kind": "permission", "app": "claude-code",
                        "msg": "approve Bash: touch /tmp/x"},
             "labels": {"cockpit.msg": "Permission required"},
             "foreground": {"name": "claude", "argv": ["claude"]}}
        ]}"#;
        let sessions = parse_hc_ls_json(sample, None).unwrap();
        assert_eq!(
            sessions[0].msg.as_deref(),
            Some("approve Bash: touch /tmp/x")
        );
    }

    #[test]
    fn test_parse_hc_ls_json_reads_blocked_kind() {
        let sample = r#"{"terminals": [
            {"address": "t1", "state": "live", "reported": true,
             "status": {"state": "blocked", "kind": "question", "app": "claude"},
             "foreground": {"name": "claude", "argv": ["claude"]}},
            {"address": "t2", "state": "live", "reported": true,
             "status": {"state": "blocked", "app": "claude"},
             "labels": {"cockpit.kind": "permission"},
             "foreground": {"name": "claude", "argv": ["claude"]}}
        ]}"#;
        let sessions = parse_hc_ls_json(sample, None).unwrap();
        let kind = |id: &str| {
            sessions
                .iter()
                .find(|s| s.agent_id == id)
                .unwrap()
                .kind
                .clone()
        };
        assert_eq!(kind("t1").as_deref(), Some("question"));
        assert_eq!(kind("t2").as_deref(), Some("permission"));
    }

    #[test]
    fn test_parse_hc_ls_json_filters_herdr_attach_clients() {
        let sample = r#"{
            "terminals": [
                {
                    "address": "t20",
                    "state": "live",
                    "foreground": {"name": "herdr-core", "argv": ["herdr-core", "attach", "lore-agy"]},
                    "argv": ["herdr-core", "attach", "lore-agy"]
                },
                {
                    "address": "t21",
                    "state": "live",
                    "foreground": {"name": "hc", "argv": ["hc", "attach", "claude"]},
                    "argv": ["hc", "attach", "claude"]
                },
                {
                    "address": "t22",
                    "state": "live",
                    "argv": ["/home/ubuntu/.local/bin/herdr-core", "attach", "codex"]
                }
            ]
        }"#;
        let sessions = parse_hc_ls_json(sample, None).unwrap();
        assert!(sessions.is_empty());
        assert!(!is_coding_agent_terminal(
            None,
            Some("claude"),
            Some(&RawHcForeground {
                name: Some("hc".to_string()),
                argv: vec!["hc".into(), "attach".into(), "claude".into()],
            }),
            &[],
            false,
            None,
            None,
            &BTreeMap::new(),
        ));
    }
}
