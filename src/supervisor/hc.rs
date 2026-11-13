use super::config::HcAdapterConfig;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::process::Command;

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
}

impl HcSession {
    /// Returns true if this terminal session is currently live.
    pub fn is_active(&self) -> bool {
        self.state == "live"
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
    /// Sets metadata labels on an `hc` terminal (`hc label <agent_id> k=v...`).
    fn set_labels(&self, agent_id: &str, labels: &[(&str, &str)]) -> Result<()>;
}

/// Production `hc` CLI adapter bound to `/home/ubuntu/.local/bin/hc` or `hc`.
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

impl HcBackend for HcCliAdapter {
    fn list_sessions(&self) -> Result<Vec<HcSession>> {
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
            .with_context(|| format!("Failed to execute `{} read {agent_id}`", self.config.hc_bin))?;

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

        let output = cmd
            .output()
            .with_context(|| format!("Failed to execute `{} send {agent_id}`", self.config.hc_bin))?;

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

fn normalize_program_token(raw: &str) -> String {
    let base = raw
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim();
    base.to_ascii_lowercase()
}

fn is_known_coding_agent(token: &str) -> bool {
    let norm = normalize_program_token(token);
    matches!(
        norm.as_str(),
        "claude"
            | "claude-code"
            | "claudecode"
            | "opencode"
            | "pi"
            | "codex"
            | "aider"
            | "goose"
            | "cursor"
            | "cursor-agent"
            | "amp"
    )
}

fn is_plain_shell_program(token: &str) -> bool {
    let norm = normalize_program_token(token);
    matches!(
        norm.as_str(),
        "zsh"
            | "-zsh"
            | "bash"
            | "-bash"
            | "sh"
            | "-sh"
            | "fish"
            | "-fish"
            | "nu"
            | "nushell"
            | "dash"
            | "ksh"
            | "csh"
            | "tcsh"
            | "tmux"
            | "screen"
            | "login"
    )
}

fn is_supervisor_or_cockpit_name(token: &str) -> bool {
    let norm = normalize_program_token(token);
    matches!(
        norm.as_str(),
        "herdr-supervisor" | "cockpit-supervisor" | "cockpit"
    )
}

fn extract_status_fields(status: Option<&serde_json::Value>) -> (Option<String>, Option<String>) {
    let Some(val) = status else {
        return (None, None);
    };
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        return (
            (!trimmed.is_empty()).then(|| trimmed.to_string()),
            None,
        );
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
/// an idle plain shell or a supervisor/cockpit UI process.
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
    if agent.is_some_and(is_supervisor_or_cockpit_name)
        || name.is_some_and(is_supervisor_or_cockpit_name)
        || fg_name.is_some_and(is_supervisor_or_cockpit_name)
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

    if candidates.into_iter().any(is_known_coding_agent) {
        return true;
    }

    let has_active_status = reported
        || matches!(
            status_state,
            Some("working") | Some("blocked") | Some("done") | Some("error") | Some("waiting")
        );
    if has_active_status {
        let primary = fg_name.or(agent);
        if !primary.is_some_and(is_plain_shell_program) {
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
    let parsed: RawHcListOutput =
        serde_json::from_str(raw_json.trim()).context("Failed to parse `hc ls --json` output")?;

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

        let (status_state, status_app) = extract_status_fields(term.status.as_ref());
        if !is_coding_agent_terminal(
            term.agent.as_deref(),
            term.name.as_deref(),
            term.foreground.as_ref(),
            &term.argv,
            term.reported,
            status_state.as_deref(),
            status_app.as_deref(),
            &term.labels,
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
            .agent
            .clone()
            .or_else(|| status_app.clone())
            .or_else(|| term.foreground.as_ref().and_then(|f| f.name.clone()));

        let session_name = match &agent_label {
            Some(agent) if !base_name.contains(agent) => format!("{base_name} ({agent})"),
            _ => base_name,
        };

        sessions.push(HcSession {
            agent_id: term.address,
            session_name,
            agent_program: agent_label,
            cwd: term.cwd,
            state: term.state,
            status: status_state,
            turn: term.turn,
        });
    }

    Ok(sessions)
}

/// Returns `true` if a line contains only whitespace or Unicode/ASCII box-drawing border characters.
pub fn is_blank_or_box_border_only(line: &str) -> bool {
    line.chars()
        .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c) || c == '|')
}

fn clean_tui_line(line: &str) -> &str {
    let trimmed = line.trim();
    if trimmed.starts_with('│') || trimmed.ends_with('│') {
        trimmed
            .trim_start_matches('│')
            .trim_end_matches('│')
            .trim()
    } else {
        line.trim_end()
    }
}

/// Extracts the last `max_lines` from either `hc read --json` (`{"text": "..."}`) or raw text,
/// stripping empty lines and Ratatui/TUI box-drawing border lines.
pub fn extract_tail_lines(raw_output: &str, max_lines: usize) -> Result<String> {
    let text = if let Ok(parsed) = serde_json::from_str::<RawHcReadOutput>(raw_output.trim()) {
        parsed.text
    } else {
        raw_output.to_string()
    };

    let meaningful_lines: Vec<&str> = text
        .lines()
        .map(clean_tui_line)
        .filter(|line| !is_blank_or_box_border_only(line))
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
                {"address":"t14","state":"live","status":{"state":"working","app":"codex"},"where":"api"}
            ]
        }"#;

        let sessions = parse_hc_ls_json(sample, Some("t10")).unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].agent_id, "t7");
        assert_eq!(sessions[0].session_name, "teste / 1 (pi)");
        assert_eq!(sessions[0].status.as_deref(), Some("done"));
        assert_eq!(sessions[1].agent_id, "t13");
        assert_eq!(sessions[2].agent_id, "t14");
        assert_eq!(sessions[2].status.as_deref(), Some("working"));
        assert_eq!(sessions[2].agent_program.as_deref(), Some("codex"));
    }

    #[test]
    fn test_extract_tail_lines_from_hc_read_json() {
        let raw = r#"{"complete":true,"how":"screen","text":"line 1\nline 2\nline 3\nline 4\n\n\n"}"#;
        let tail = extract_tail_lines(raw, 2).unwrap();
        assert_eq!(tail, "line 3\nline 4");
    }

    #[test]
    fn test_extract_tail_lines_strips_empty_box_borders() {
        let raw = "┌───────────────────────────────────────┐\n\
                   │ Monitorando sessão ativa do agente.   │\n\
                   │ t9g (ant-game / 1 (zsh)) [WORKING]    │\n\
                   │                                       │\n\
                   │                                       │\n\
                   └───────────────────────────────────────┘";
        let tail = extract_tail_lines(raw, 25).unwrap();
        assert_eq!(
            tail,
            "Monitorando sessão ativa do agente.\nt9g (ant-game / 1 (zsh)) [WORKING]"
        );
    }
}
