use crate::model::{
    sanitize_terminal_text, AgentLifecycle, AttentionState, BlockedKind, FleetSummaryProjection,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub const BLOCKED_CLEAR_HYSTERESIS_MS: i64 = 1_500;

#[derive(Debug, Clone)]
pub struct HcRunError {
    pub exit_code: Option<i32>,
    pub stderr: String,
    pub stdout: String,
    pub message: String,
}

impl fmt::Display for HcRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HcRunError {}

pub const DEFAULT_HC_SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HcErrorInfo {
    pub kind: Option<String>,
    pub message: String,
}

fn extract_hc_error_from_str(raw: &str) -> Option<HcErrorInfo> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let try_parse = |s: &str| -> Option<HcErrorInfo> {
        let val: Value = serde_json::from_str(s).ok()?;
        let err = val.get("error")?;
        let kind = err
            .get("kind")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if kind.is_some() || !message.is_empty() {
            Some(HcErrorInfo { kind, message })
        } else {
            None
        }
    };
    if let Some(info) = try_parse(trimmed) {
        return Some(info);
    }
    for line in trimmed.lines().rev() {
        if let Some(info) = try_parse(line.trim()) {
            return Some(info);
        }
    }
    None
}

pub fn parse_hc_json_error(stdout: &[u8], stderr: &[u8]) -> Option<HcErrorInfo> {
    let stdout_str = String::from_utf8_lossy(stdout);
    if let Some(info) = extract_hc_error_from_str(&stdout_str) {
        return Some(info);
    }
    let stderr_str = String::from_utf8_lossy(stderr);
    if let Some(info) = extract_hc_error_from_str(&stderr_str) {
        return Some(info);
    }
    let fallback = stderr_str.trim();
    if !fallback.is_empty() {
        return Some(HcErrorInfo {
            kind: None,
            message: fallback.to_string(),
        });
    }
    let out_fallback = stdout_str.trim();
    if !out_fallback.is_empty() {
        return Some(HcErrorInfo {
            kind: None,
            message: out_fallback.to_string(),
        });
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedSendMode {
    IfIdle,
    DoneRecheckThenPlain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardedSendOutcome {
    Sent {
        used_plain_send: bool,
    },
    Deferred {
        reason: String,
        detail: Option<String>,
    },
    Error {
        reason: String,
        detail: Option<String>,
    },
}

impl GuardedSendOutcome {
    pub fn is_sent(&self) -> bool {
        matches!(self, Self::Sent { .. })
    }
}

pub fn classify_terminal_for_send(
    term: Option<&HcTerminal>,
    attention_override: Option<AttentionState>,
) -> (String, std::result::Result<GuardedSendMode, &'static str>) {
    let Some(term) = term else {
        return ("missing".to_string(), Err("terminal_missing_from_tracker"));
    };
    if term.state == "finished" {
        return ("finished".to_string(), Err("terminal_finished"));
    }
    let att = attention_override.unwrap_or_else(|| term.raw_attention_state(0));
    match att {
        AttentionState::Blocked { .. } => {
            return ("blocked".to_string(), Err("terminal_blocked"));
        }
        AttentionState::Working { .. } => {
            return ("working".to_string(), Err("terminal_working"));
        }
        AttentionState::Error { .. } => {
            return ("error".to_string(), Err("terminal_error"));
        }
        AttentionState::Done { .. } => {
            return (
                "done".to_string(),
                Ok(GuardedSendMode::DoneRecheckThenPlain),
            );
        }
        AttentionState::Idle => {}
    }

    match term.status_name() {
        Some("idle") => ("idle".to_string(), Ok(GuardedSendMode::IfIdle)),
        Some("done") => (
            "done".to_string(),
            Ok(GuardedSendMode::DoneRecheckThenPlain),
        ),
        Some("working") => ("working".to_string(), Err("terminal_working")),
        Some("blocked") => ("blocked".to_string(), Err("terminal_blocked")),
        Some("error") | Some("failed") => ("error".to_string(), Err("terminal_error")),
        None | Some("") => ("unknown".to_string(), Err("terminal_unknown")),
        Some(other) => (other.to_string(), Err("terminal_unknown")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HcForeground {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HcStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HcFirstMessage {
    #[serde(default)]
    pub sent: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub started: Option<bool>,
    #[serde(default)]
    pub turn: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HcTerminal {
    #[serde(default, alias = "id", alias = "terminal")]
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub reported: bool,
    #[serde(default)]
    pub status: Option<HcStatus>,
    #[serde(default)]
    pub status_ms: Option<i64>,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground: Option<HcForeground>,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub created_ms: Option<i64>,
    #[serde(default)]
    pub finished_ms: Option<i64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub r#where: Option<String>,
    #[serde(default)]
    pub first_message: Option<HcFirstMessage>,
}

impl HcTerminal {
    pub fn loop_run_id(&self) -> Option<&str> {
        self.labels
            .get("cockpit.loop_run")
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn is_attach_client(&self) -> bool {
        let (fg_name, fg_argv) = match &self.foreground {
            Some(fg) => (fg.name.as_deref(), fg.argv.as_slice()),
            None => (None, &[][..]),
        };
        crate::agents::is_attach_client_invocation(fg_name, fg_argv)
            || crate::agents::is_attach_client_invocation(None, &self.argv)
    }

    pub fn display_name(&self) -> String {
        if let Some(run_id) = self.loop_run_id().filter(|_| !self.is_attach_client()) {
            if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
                if name.contains(run_id) {
                    return name.to_string();
                }
            }
            return format!("loop-{run_id}");
        }
        self.name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(&self.address)
            .to_string()
    }

    pub fn status_name(&self) -> Option<&str> {
        self.status
            .as_ref()
            .map(|s| s.state.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.labels
                    .get("cockpit.state")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
            })
    }

    pub fn status_msg(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.msg.as_deref())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.labels
                    .get("cockpit.msg")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
            })
    }

    pub fn effective_cwd(&self) -> String {
        self.foreground
            .as_ref()
            .and_then(|f| f.cwd.as_deref())
            .or(self.cwd.as_deref())
            .or(self.r#where.as_deref())
            .unwrap_or(".")
            .to_string()
    }

    pub fn lifecycle(&self) -> AgentLifecycle {
        if self.state == "finished" {
            AgentLifecycle::Finished {
                exit_code: self.exit_code,
                finished_ms: self.finished_ms,
                reason: self.reason.as_deref().map(sanitize_terminal_text),
            }
        } else {
            AgentLifecycle::Live
        }
    }

    pub fn is_plain_shell_foreground(&self) -> bool {
        let (fg_name, fg_argv) = match &self.foreground {
            Some(fg) => (fg.name.as_deref(), fg.argv.as_slice()),
            None => (None, &[][..]),
        };
        let (status_state, status_app) = match &self.status {
            Some(st) => (Some(st.state.as_str()), st.app.as_deref()),
            None => (None, None),
        };
        crate::agents::is_plain_shell_foreground_parts(
            &self.argv,
            fg_name,
            fg_argv,
            status_state,
            status_app,
        )
    }

    pub fn raw_attention_state(&self, now_ms: i64) -> AttentionState {
        if self.state == "finished" {
            if self.exit_code.unwrap_or(0) != 0 {
                return AttentionState::Error {
                    message: self
                        .reason
                        .as_deref()
                        .map(sanitize_terminal_text)
                        .filter(|m| !m.is_empty()),
                };
            }
            return AttentionState::Done { message: None };
        }
        if self.is_attach_client() {
            return AttentionState::Idle;
        }
        let loop_run = self.loop_run_id();
        if loop_run.is_none() && self.is_plain_shell_foreground() {
            return AttentionState::Idle;
        }
        if let Some(st) = &self.status {
            let msg = st
                .msg
                .as_deref()
                .map(sanitize_terminal_text)
                .filter(|m| !m.is_empty());
            match st.state.as_str() {
                "blocked" => {
                    let kind = match st.kind.as_deref() {
                        Some("auth") => BlockedKind::Auth,
                        Some("permission") => BlockedKind::Permission,
                        Some("question") | None => BlockedKind::Question,
                        Some(_) => BlockedKind::Unknown,
                    };
                    return AttentionState::Blocked {
                        kind,
                        message: msg,
                        since_ms: self.status_ms.unwrap_or(now_ms),
                    };
                }
                "working" => {
                    return AttentionState::Working {
                        message: msg.or_else(|| loop_run.map(|id| format!("loop {id}"))),
                    };
                }
                "done" => return AttentionState::Done { message: msg },
                "error" | "failed" => return AttentionState::Error { message: msg },
                "" | "idle" => {}
                _ if loop_run.is_none() => return AttentionState::Idle,
                _ => {}
            }
        }

        if let Some(lbl_state) = self.labels.get("cockpit.state") {
            let msg = self
                .labels
                .get("cockpit.msg")
                .map(String::as_str)
                .map(sanitize_terminal_text)
                .filter(|m| !m.is_empty());
            match lbl_state.trim().to_ascii_lowercase().as_str() {
                "blocked" => {
                    let kind = match self
                        .labels
                        .get("cockpit.kind")
                        .map(|s| s.trim().to_ascii_lowercase())
                        .as_deref()
                    {
                        Some("auth") => BlockedKind::Auth,
                        Some("permission") => BlockedKind::Permission,
                        Some("question") | Some("") | None => BlockedKind::Question,
                        Some(_) => BlockedKind::Unknown,
                    };
                    return AttentionState::Blocked {
                        kind,
                        message: msg,
                        since_ms: self.status_ms.unwrap_or(now_ms),
                    };
                }
                "working" => {
                    return AttentionState::Working {
                        message: msg.or_else(|| loop_run.map(|id| format!("loop {id}"))),
                    };
                }
                "done" => return AttentionState::Done { message: msg },
                "error" | "failed" => return AttentionState::Error { message: msg },
                _ => {}
            }
        }

        if let Some(run_id) = loop_run {
            return AttentionState::Working {
                message: Some(format!("loop {run_id}")),
            };
        }

        AttentionState::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SubscribeResult {
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub terminals: Vec<HcTerminal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ListResult {
    #[serde(default)]
    pub hidden_finished: u64,
    #[serde(default)]
    pub terminals: Vec<HcTerminal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocGetResult {
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSetResult {
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HcStreamItem {
    InitialSnapshot(SubscribeResult),
    Event {
        seq: u64,
        terminal: Option<String>,
        event_type: String,
        data: Value,
    },
    Heartbeat,
    Missed(Option<SubscribeResult>),
}

/// Parses one JSON line from `hc events --json`.
pub fn parse_hc_event_line(line: &str) -> Result<HcStreamItem> {
    let val: Value = serde_json::from_str(line).context("invalid JSON line from hc events")?;
    if val.get("terminals").is_some() && val.get("method").is_none() {
        let snap: SubscribeResult = serde_json::from_value(val)?;
        return Ok(HcStreamItem::InitialSnapshot(snap));
    }

    if let Some(method) = val.get("method").and_then(|m| m.as_str()) {
        let params = val.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "event" => {
                let seq = params.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
                let terminal = params
                    .get("terminal")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let event_type = params
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let data = params.get("data").cloned().unwrap_or(Value::Null);
                return Ok(HcStreamItem::Event {
                    seq,
                    terminal,
                    event_type,
                    data,
                });
            }
            "heartbeat" => return Ok(HcStreamItem::Heartbeat),
            "missed" => {
                let snap = if params.get("terminals").is_some() {
                    serde_json::from_value::<SubscribeResult>(params).ok()
                } else if let Some(inner) = params.get("snapshot") {
                    serde_json::from_value::<SubscribeResult>(inner.clone()).ok()
                } else {
                    None
                };
                return Ok(HcStreamItem::Missed(snap));
            }
            _ => {}
        }
    }

    bail!("unrecognized hc event line: {}", line)
}

/// Tracks terminals in memory and enforces asymmetric 1.5s clear-hysteresis for `blocked` state.
#[derive(Debug, Default, Clone)]
pub struct FleetTracker {
    pub terminals: BTreeMap<String, HcTerminal>,
    /// Maps terminal address -> `(Blocked AttentionState, clear_requested_at_ms)`
    blocked_hold: BTreeMap<String, (AttentionState, Option<i64>)>,
}

impl FleetTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_snapshot(&mut self, terminals: Vec<HcTerminal>, now_ms: i64) {
        let mut next_map = BTreeMap::new();
        for t in terminals {
            let raw_att = t.raw_attention_state(now_ms);
            if matches!(raw_att, AttentionState::Blocked { .. }) {
                self.blocked_hold.insert(t.address.clone(), (raw_att, None));
            } else if t.state == "finished" {
                self.blocked_hold.remove(&t.address);
            } else if let Some((_, clear_ts)) = self.blocked_hold.get_mut(&t.address) {
                match clear_ts {
                    None => *clear_ts = Some(now_ms),
                    Some(since) if now_ms - *since >= BLOCKED_CLEAR_HYSTERESIS_MS => {
                        self.blocked_hold.remove(&t.address);
                    }
                    _ => {}
                }
            }
            next_map.insert(t.address.clone(), t);
        }
        self.terminals = next_map;
    }

    pub fn apply_stream_item(&mut self, item: HcStreamItem, now_ms: i64) -> bool {
        match item {
            HcStreamItem::InitialSnapshot(snap) => {
                self.apply_snapshot(snap.terminals, now_ms);
                true
            }
            HcStreamItem::Heartbeat => false,
            HcStreamItem::Missed(Some(snap)) => {
                self.apply_snapshot(snap.terminals, now_ms);
                true
            }
            HcStreamItem::Missed(None) => true,
            HcStreamItem::Event {
                terminal,
                event_type,
                data,
                ..
            } => {
                let Some(addr) = terminal else {
                    return event_type == "doc";
                };
                match event_type.as_str() {
                    "created" | "restarted" => {
                        if let Ok(mut term) = serde_json::from_value::<HcTerminal>(data) {
                            if term.address.is_empty() {
                                term.address = addr.clone();
                            }
                            let att = term.raw_attention_state(now_ms);
                            if matches!(att, AttentionState::Blocked { .. }) {
                                self.blocked_hold.insert(addr.clone(), (att, None));
                            }
                            self.terminals.insert(addr, term);
                            return true;
                        }
                        false
                    }
                    "foreground" => {
                        if let Ok(fg) = serde_json::from_value::<HcForeground>(data) {
                            let entry =
                                self.terminals
                                    .entry(addr.clone())
                                    .or_insert_with(|| HcTerminal {
                                        address: addr,
                                        state: "live".into(),
                                        ..Default::default()
                                    });
                            entry.foreground = Some(fg);
                            return true;
                        }
                        false
                    }
                    "status" => {
                        let entry =
                            self.terminals
                                .entry(addr.clone())
                                .or_insert_with(|| HcTerminal {
                                    address: addr.clone(),
                                    state: "live".into(),
                                    ..Default::default()
                                });
                        if data.is_null() {
                            entry.status = None;
                        } else if let Ok(st) = serde_json::from_value::<HcStatus>(data) {
                            entry.status = Some(st);
                            entry.reported = true;
                            entry.status_ms = Some(now_ms);
                        }
                        let new_att = entry.raw_attention_state(now_ms);
                        if matches!(new_att, AttentionState::Blocked { .. }) {
                            // Immediate entry into Blocked
                            self.blocked_hold.insert(addr, (new_att, None));
                        } else if let Some((_, clear_ts)) = self.blocked_hold.get_mut(&addr) {
                            if clear_ts.is_none() {
                                *clear_ts = Some(now_ms);
                            }
                        }
                        true
                    }
                    "turn" => {
                        let turn_val = data
                            .get("turn")
                            .and_then(|v| v.as_u64())
                            .or_else(|| data.as_u64());
                        if let Some(t_num) = turn_val {
                            if let Some(entry) = self.terminals.get_mut(&addr) {
                                if t_num > entry.turn {
                                    entry.turn = t_num;
                                    // Turn advance immediately clears any held Blocked state
                                    if !matches!(
                                        entry.raw_attention_state(now_ms),
                                        AttentionState::Blocked { .. }
                                    ) {
                                        self.blocked_hold.remove(&addr);
                                    }
                                }
                            }
                            return true;
                        }
                        false
                    }
                    "labels" => {
                        if let Ok(labels) = serde_json::from_value::<BTreeMap<String, String>>(data)
                        {
                            let entry =
                                self.terminals
                                    .entry(addr.clone())
                                    .or_insert_with(|| HcTerminal {
                                        address: addr,
                                        state: "live".into(),
                                        ..Default::default()
                                    });
                            entry.labels = labels;
                            return true;
                        }
                        false
                    }
                    "finished" => {
                        if let Some(entry) = self.terminals.get_mut(&addr) {
                            entry.state = "finished".into();
                            entry.finished_ms = Some(now_ms);
                            if let Some(code) = data.get("exit_code").and_then(|c| c.as_i64()) {
                                entry.exit_code = Some(code as i32);
                            }
                            if let Some(r) = data.get("reason").and_then(|r| r.as_str()) {
                                entry.reason = Some(r.to_string());
                            }
                        }
                        self.blocked_hold.remove(&addr);
                        true
                    }
                    "removed" => {
                        self.terminals.remove(&addr);
                        self.blocked_hold.remove(&addr);
                        true
                    }
                    _ => false,
                }
            }
        }
    }

    pub fn resolved_parent(term: &HcTerminal) -> Option<&str> {
        term.parent
            .as_deref()
            .filter(|p| !p.is_empty())
            .or_else(|| {
                term.labels
                    .get("cockpit.parent")
                    .map(String::as_str)
                    .filter(|p| !p.is_empty())
            })
    }

    /// Evaluates effective attention state for `addr` at `now_ms` (Unix epoch milliseconds),
    /// applying 1.5s clear-hysteresis.
    pub fn effective_attention(&self, addr: &str, now_ms: i64) -> AttentionState {
        let Some(term) = self.terminals.get(addr) else {
            return AttentionState::Idle;
        };
        if term.state == "finished" {
            return term.raw_attention_state(now_ms);
        }
        let raw = term.raw_attention_state(now_ms);
        if matches!(raw, AttentionState::Blocked { .. }) {
            return raw;
        }
        if let Some((held_state, clear_since)) = self.blocked_hold.get(addr) {
            let since = clear_since.unwrap_or(now_ms);
            if now_ms - since < BLOCKED_CLEAR_HYSTERESIS_MS {
                return held_state.clone();
            }
        }
        raw
    }

    /// Computes child terminals (`parent == Some(parent_addr)` or `labels["cockpit.parent"]`) that are currently in `Blocked` state.
    /// `now_ms` is Unix epoch milliseconds.
    pub fn blocked_children_map(&self, now_ms: i64) -> BTreeMap<String, Vec<String>> {
        let addresses: Vec<String> = self.terminals.keys().cloned().collect();
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for addr in addresses {
            let att = self.effective_attention(&addr, now_ms);
            if matches!(att, AttentionState::Blocked { .. }) {
                if let Some(parent) = self
                    .terminals
                    .get(&addr)
                    .and_then(Self::resolved_parent)
                    .map(str::to_string)
                {
                    map.entry(parent).or_default().push(addr);
                }
            }
        }
        for children in map.values_mut() {
            children.sort();
            children.dedup();
        }
        map
    }

    /// Computes active child terminals per parent at `now_ms` (Unix epoch milliseconds):
    /// 1. Live child terminals listed in `active_orch_children` (`status IN ('starting', 'dispatched', 'waiting_for_reply')`), AND
    /// 2. Non-orchestrated live child terminals (`cockpit.orch.role != Some("worker")`) whose `effective_attention` is `Working` or `Blocked`.
    pub fn active_children_map(
        &self,
        active_orch_children: &BTreeMap<String, Vec<String>>,
        now_ms: i64,
    ) -> BTreeMap<String, Vec<String>> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for (parent, child_ids) in active_orch_children {
            for child_id in child_ids {
                if self
                    .terminals
                    .get(child_id)
                    .is_some_and(|term| term.state != "finished")
                {
                    map.entry(parent.clone())
                        .or_default()
                        .push(child_id.clone());
                }
            }
        }

        for (addr, term) in &self.terminals {
            if term.state == "finished" {
                continue;
            }
            if term.labels.get("cockpit.orch.role").map(String::as_str) == Some("worker") {
                continue;
            }
            if let Some(parent) = Self::resolved_parent(term).map(str::to_string) {
                let att = self.effective_attention(addr, now_ms);
                if matches!(
                    att,
                    AttentionState::Working { .. } | AttentionState::Blocked { .. }
                ) {
                    map.entry(parent).or_default().push(addr.clone());
                }
            }
        }

        for children in map.values_mut() {
            children.sort();
            children.dedup();
        }
        map
    }
}

pub struct HcRpcSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl HcRpcSession {
    pub async fn connect(hc_bin: &str) -> Result<Self> {
        let mut child = Command::new(hc_bin)
            .arg("connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed spawning `{hc_bin} connect`"))?;

        let stdin = child.stdin.take().context("missing hc connect stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("missing hc connect stdout")?);
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        })
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        tokio::time::timeout(Duration::from_secs(5), async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await?;
            let mut resp_line = String::new();
            loop {
                resp_line.clear();
                let n = self.stdout.read_line(&mut resp_line).await?;
                if n == 0 {
                    bail!("hc connect closed stdout unexpectedly");
                }
                let val: Value = serde_json::from_str(&resp_line)?;
                if val.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    if let Some(err) = val.get("error") {
                        bail!("hc rpc error on {method}: {err}");
                    }
                    return Ok(val.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        })
        .await
        .map_err(|_| anyhow!("timeout waiting for hc rpc {method}"))?
    }
}

impl Drop for HcRpcSession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[derive(Clone)]
pub struct HerdrClient {
    hc_bin: String,
}

pub fn default_hc_bin() -> String {
    default_hc_bin_with(|k| std::env::var(k).ok(), |p| p.exists())
}

pub(crate) fn default_hc_bin_with(
    get_env: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> String {
    if let Some(bin) = get_env("HC_BIN") {
        if !bin.trim().is_empty() {
            return bin;
        }
    }
    if let Some(home) = get_env("HOME") {
        if !home.trim().is_empty() {
            let home_path = std::path::PathBuf::from(home);
            for candidate in [
                home_path.join(".local/bin/hc"),
                home_path.join(".cargo/bin/hc"),
            ] {
                if exists(&candidate) {
                    return candidate.to_string_lossy().into_owned();
                }
            }
        }
    }
    "hc".to_string()
}

impl Default for HerdrClient {
    fn default() -> Self {
        Self::new(default_hc_bin())
    }
}

impl HerdrClient {
    pub fn new(hc_bin: impl Into<String>) -> Self {
        Self {
            hc_bin: hc_bin.into(),
        }
    }

    pub fn hc_bin(&self) -> &str {
        &self.hc_bin
    }

    pub async fn probe_version(&self) -> Result<String> {
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&self.hc_bin)
                .arg("--version")
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("hc --version timed out")??;

        if !out.status.success() {
            bail!("hc --version failed with status {}", out.status);
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub async fn list_terminals_full(
        &self,
        rpc: &mut Option<HcRpcSession>,
    ) -> Result<Vec<HcTerminal>> {
        let res = self
            .rpc_call(rpc, "terminal.list", json!({"all": true}))
            .await?;
        let parsed: ListResult = serde_json::from_value(res)?;
        Ok(parsed.terminals)
    }

    pub async fn get_terminal(
        &self,
        rpc: &mut Option<HcRpcSession>,
        terminal: &str,
    ) -> Result<HcTerminal> {
        let res = self
            .rpc_call(rpc, "terminal.get", json!({"terminal": terminal}))
            .await?;
        Ok(serde_json::from_value(res)?)
    }

    pub async fn publish_doc_snapshot(
        &self,
        rpc: &mut Option<HcRpcSession>,
        projection: &FleetSummaryProjection,
    ) -> Result<u64> {
        let val = serde_json::to_value(projection)?;
        for attempt in 0..2 {
            let get_res = self
                .rpc_call(rpc, "doc.get", json!({"name": "cockpit.snapshot"}))
                .await?;
            let current: DocGetResult = serde_json::from_value(get_res)?;
            match self
                .rpc_call(
                    rpc,
                    "doc.set",
                    json!({
                        "name": "cockpit.snapshot",
                        "value": val,
                        "version": current.version
                    }),
                )
                .await
            {
                Ok(set_res) => {
                    let out: DocSetResult = serde_json::from_value(set_res)?;
                    return Ok(out.version);
                }
                Err(e) if attempt == 0 && e.to_string().contains("conflict") => {
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        bail!("doc.set conflict after retry")
    }

    pub async fn rpc_call(
        &self,
        rpc: &mut Option<HcRpcSession>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if rpc.is_none() {
            *rpc = Some(HcRpcSession::connect(&self.hc_bin).await?);
        }
        let res = rpc.as_mut().unwrap().call(method, params.clone()).await;
        if res.is_err() {
            // Reconnect once
            *rpc = Some(HcRpcSession::connect(&self.hc_bin).await?);
            rpc.as_mut().unwrap().call(method, params).await
        } else {
            res
        }
    }

    pub fn spawn_events_stream(&self) -> Result<(Child, BufReader<ChildStdout>)> {
        let mut child = Command::new(&self.hc_bin)
            .args(["events", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed spawning `{} events --json`", self.hc_bin))?;
        let stdout = BufReader::new(child.stdout.take().context("missing hc events stdout")?);
        Ok((child, stdout))
    }

    pub async fn send_guarded(
        &self,
        term: &HcTerminal,
        attention_override: Option<AttentionState>,
        text: &str,
        timeout: Option<Duration>,
    ) -> GuardedSendOutcome {
        let (_status, mode_res) = classify_terminal_for_send(Some(term), attention_override);
        match mode_res {
            Ok(mode) => {
                self.send_guarded_with_mode(
                    &term.address,
                    text,
                    mode,
                    timeout.unwrap_or(DEFAULT_HC_SEND_TIMEOUT),
                )
                .await
            }
            Err(reason) => GuardedSendOutcome::Deferred {
                reason: reason.to_string(),
                detail: None,
            },
        }
    }

    pub async fn send_guarded_with_mode(
        &self,
        terminal: &str,
        text: &str,
        mode: GuardedSendMode,
        timeout: Duration,
    ) -> GuardedSendOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        let use_plain = match mode {
            GuardedSendMode::IfIdle => false,
            GuardedSendMode::DoneRecheckThenPlain => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return GuardedSendOutcome::Deferred {
                        reason: "send_timeout".to_string(),
                        detail: Some(format!("hc get {terminal} timed out before send")),
                    };
                }
                let get_out = match tokio::time::timeout(
                    remaining,
                    Command::new(&self.hc_bin)
                        .args(["get", terminal, "--json"])
                        .stdin(Stdio::null())
                        .kill_on_drop(true)
                        .output(),
                )
                .await
                {
                    Ok(Ok(out)) => out,
                    Ok(Err(err)) => {
                        return GuardedSendOutcome::Error {
                            reason: "hc_spawn_failed".to_string(),
                            detail: Some(err.to_string()),
                        };
                    }
                    Err(_) => {
                        return GuardedSendOutcome::Deferred {
                            reason: "send_timeout".to_string(),
                            detail: Some(format!("hc get {terminal} timed out before send")),
                        };
                    }
                };

                if !get_out.status.success() {
                    let info = parse_hc_json_error(&get_out.stdout, &get_out.stderr);
                    return GuardedSendOutcome::Deferred {
                        reason: info
                            .as_ref()
                            .and_then(|i| i.kind.clone())
                            .unwrap_or_else(|| "hc_get_failed".to_string()),
                        detail: info.map(|i| i.message).filter(|m| !m.is_empty()),
                    };
                }

                let Ok(fresh) = serde_json::from_slice::<HcTerminal>(&get_out.stdout) else {
                    return GuardedSendOutcome::Deferred {
                        reason: "hc_get_invalid_json".to_string(),
                        detail: Some(format!("invalid JSON from `hc get {terminal} --json`")),
                    };
                };

                if fresh.state == "finished" {
                    return GuardedSendOutcome::Deferred {
                        reason: "terminal_finished".to_string(),
                        detail: Some(format!("{terminal} is finished on recheck")),
                    };
                }

                match fresh.status_name() {
                    Some("done") => true,
                    Some("idle") => false,
                    Some("working") => {
                        return GuardedSendOutcome::Deferred {
                            reason: "terminal_working".to_string(),
                            detail: Some(format!("{terminal} became working on recheck")),
                        };
                    }
                    Some("blocked") => {
                        return GuardedSendOutcome::Deferred {
                            reason: "terminal_blocked".to_string(),
                            detail: Some(format!("{terminal} became blocked on recheck")),
                        };
                    }
                    Some("error") | Some("failed") => {
                        return GuardedSendOutcome::Deferred {
                            reason: "terminal_error".to_string(),
                            detail: Some(format!("{terminal} became error on recheck")),
                        };
                    }
                    other => {
                        let st = other.filter(|s| !s.is_empty()).unwrap_or("unknown");
                        return GuardedSendOutcome::Deferred {
                            reason: "terminal_unknown".to_string(),
                            detail: Some(format!("{terminal} status is '{st}' on recheck")),
                        };
                    }
                }
            }
        };

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return GuardedSendOutcome::Deferred {
                reason: "send_timeout".to_string(),
                detail: Some(format!("hc send timed out after {}ms", timeout.as_millis())),
            };
        }

        let mut cmd = Command::new(&self.hc_bin);
        if use_plain {
            cmd.args(["send", terminal, text, "--json"]);
        } else {
            cmd.args(["send", "--if", "idle", terminal, text, "--json"]);
        }
        cmd.stdin(Stdio::null()).kill_on_drop(true);

        let out = match tokio::time::timeout(remaining, cmd.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(err)) => {
                return GuardedSendOutcome::Error {
                    reason: "hc_spawn_failed".to_string(),
                    detail: Some(err.to_string()),
                };
            }
            Err(_) => {
                return GuardedSendOutcome::Deferred {
                    reason: "send_timeout".to_string(),
                    detail: Some(format!("hc send timed out after {}ms", timeout.as_millis())),
                };
            }
        };

        match out.status.code() {
            Some(0) => GuardedSendOutcome::Sent {
                used_plain_send: use_plain,
            },
            Some(9) => {
                let info = parse_hc_json_error(&out.stdout, &out.stderr);
                GuardedSendOutcome::Deferred {
                    reason: info
                        .as_ref()
                        .and_then(|i| i.kind.clone())
                        .unwrap_or_else(|| "blocked".to_string()),
                    detail: info.map(|i| i.message).filter(|m| !m.is_empty()),
                }
            }
            Some(10) => {
                let info = parse_hc_json_error(&out.stdout, &out.stderr);
                GuardedSendOutcome::Deferred {
                    reason: info
                        .as_ref()
                        .and_then(|i| i.kind.clone())
                        .unwrap_or_else(|| "status_mismatch".to_string()),
                    detail: info.map(|i| i.message).filter(|m| !m.is_empty()),
                }
            }
            other => {
                let info = parse_hc_json_error(&out.stdout, &out.stderr);
                let reason = info
                    .as_ref()
                    .and_then(|i| i.kind.clone())
                    .unwrap_or_else(|| "hc_send_failed".to_string());
                let detail = info
                    .map(|i| i.message)
                    .filter(|m| !m.is_empty())
                    .or_else(|| Some(format!("exit code {:?}", other)));
                GuardedSendOutcome::Error { reason, detail }
            }
        }
    }

    pub fn build_run_worker_args(
        parent_terminal: &str,
        cwd: &Path,
        labels: &[(&str, &str)],
        agent_argv: &[String],
        first_message: &str,
    ) -> Result<Vec<OsString>> {
        Self::build_run_worker_args_with_name(
            parent_terminal,
            cwd,
            labels,
            agent_argv,
            first_message,
            None,
        )
    }

    pub fn build_run_worker_args_with_name(
        parent_terminal: &str,
        cwd: &Path,
        labels: &[(&str, &str)],
        agent_argv: &[String],
        first_message: &str,
        name: Option<&str>,
    ) -> Result<Vec<OsString>> {
        if agent_argv.is_empty() || agent_argv[0].is_empty() {
            bail!("agent_cmd cannot be empty");
        }
        let mut args: Vec<OsString> = vec![
            "run".into(),
            "--bg".into(),
            "--no-notify".into(),
            "--outlive-parent".into(),
            "--parent".into(),
            parent_terminal.into(),
            "--cwd".into(),
            cwd.as_os_str().to_os_string(),
        ];
        if let Some(trimmed) = name.map(str::trim).filter(|n| !n.is_empty()) {
            args.push("--name".into());
            args.push(trimmed.into());
        }
        for (k, v) in labels {
            args.push("--label".into());
            args.push(format!("{k}={v}").into());
        }
        args.push("--json".into());
        for tok in agent_argv {
            args.push(tok.into());
        }
        if !first_message.is_empty() {
            args.push(first_message.into());
        }
        Ok(args)
    }

    /// Only the refusal `hc` gives before starting anything (measured on 0.1.21). A taken name is
    /// not one of them: that terminal may already be this worker.
    pub fn is_hc_name_rejection_error(err: &HcRunError) -> bool {
        parse_hc_json_error(err.stdout.as_bytes(), err.stderr.as_bytes()).is_some_and(|info| {
            info.kind.as_deref() == Some("invalid_params") && info.message.starts_with("bad name ")
        })
    }

    pub async fn run_worker(
        &self,
        parent_terminal: &str,
        cwd: &Path,
        labels: &[(&str, &str)],
        agent_argv: &[String],
        first_message: &str,
    ) -> Result<HcTerminal> {
        let args =
            Self::build_run_worker_args(parent_terminal, cwd, labels, agent_argv, first_message)?;
        self.run_worker_with_args(&args).await
    }

    pub async fn run_worker_named(
        &self,
        parent_terminal: &str,
        cwd: &Path,
        labels: &[(&str, &str)],
        agent_argv: &[String],
        first_message: &str,
        name: Option<&str>,
    ) -> Result<HcTerminal> {
        let args = Self::build_run_worker_args_with_name(
            parent_terminal,
            cwd,
            labels,
            agent_argv,
            first_message,
            name,
        )?;
        match self.run_worker_with_args(&args).await {
            Ok(term) => Ok(term),
            Err(err)
                if name.is_some()
                    && err
                        .downcast_ref::<HcRunError>()
                        .is_some_and(Self::is_hc_name_rejection_error) =>
            {
                let fallback_args = Self::build_run_worker_args(
                    parent_terminal,
                    cwd,
                    labels,
                    agent_argv,
                    first_message,
                )?;
                self.run_worker_with_args(&fallback_args).await
            }
            Err(err) => Err(err),
        }
    }

    async fn run_worker_with_args(&self, args: &[OsString]) -> Result<HcTerminal> {
        let mut cmd = Command::new(&self.hc_bin);
        cmd.args(args);
        cmd.stdin(Stdio::null());

        let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
            .await
            .context("hc run timed out")??;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let detail = if !stderr.is_empty() {
                stderr.clone()
            } else {
                stdout.clone()
            };
            let message = format!("hc run failed with status {}: {}", out.status, detail);
            return Err(anyhow::Error::new(HcRunError {
                exit_code: out.status.code(),
                stderr,
                stdout,
                message,
            }));
        }

        let val: Value = serde_json::from_slice(&out.stdout)
            .context("invalid JSON output from hc run --json")?;
        let parsed_fm: Option<HcFirstMessage> = val
            .get("first_message")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // `hc run --json` (0.1.21) omits `created_ms`; only trust it when present,
        // otherwise fetch the terminal so attempt fencing gets the real value.
        if val.get("created_ms").is_some() {
            if let Ok(mut term) = serde_json::from_value::<HcTerminal>(val.clone()) {
                if !term.address.is_empty() {
                    if term.first_message.is_none() {
                        term.first_message = parsed_fm;
                    }
                    return Ok(term);
                }
            }
        }

        let id = val
            .get("address")
            .or_else(|| val.get("terminal"))
            .or_else(|| val.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .context("hc run --json output missing terminal id/address")?;

        let mut rpc = None;
        if let Ok(mut fetched) = self.get_terminal(&mut rpc, &id).await {
            if fetched.first_message.is_none() {
                fetched.first_message = parsed_fm;
            }
            return Ok(fetched);
        }

        if let Ok(Ok(get_out)) = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&self.hc_bin)
                .args(["get", &id, "--json"])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        {
            if get_out.status.success() {
                if let Ok(mut fetched) = serde_json::from_slice::<HcTerminal>(&get_out.stdout) {
                    if fetched.first_message.is_none() {
                        fetched.first_message = parsed_fm;
                    }
                    return Ok(fetched);
                }
            }
        }

        let mut term: HcTerminal = serde_json::from_value(val)?;
        if term.address.is_empty() {
            term.address = id;
        }
        if term.state.is_empty() {
            term.state = "live".to_string();
        }
        if term.first_message.is_none() {
            term.first_message = parsed_fm;
        }
        Ok(term)
    }

    pub async fn read_tail_lines(&self, terminal: &str, lines: usize) -> Result<String> {
        let lines_arg = lines.to_string();
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&self.hc_bin)
                .args(["read", terminal, "--lines", &lines_arg])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("hc read timed out")??;

        if !out.status.success() {
            bail!(
                "hc read {} failed with status {}: {}",
                terminal,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        crate::supervisor::hc::extract_tail_lines(&raw, lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hc_event_lines_initial_and_incremental() {
        let line1 = r#"{"channel":null,"seq":4,"terminals":[{"address":"t1","name":"agent1","state":"live","reported":true,"status":{"state":"blocked","kind":"permission","msg":"Allow bash?"},"turn":2,"cwd":"/repo","foreground":{"argv":["claude"],"cwd":"/repo/sub","name":"claude","pid":123},"labels":{"cockpit.agent":"claude"}}]}"#;
        let item1 = parse_hc_event_line(line1).unwrap();
        match item1 {
            HcStreamItem::InitialSnapshot(snap) => {
                assert_eq!(snap.seq, 4);
                assert_eq!(snap.terminals.len(), 1);
                assert_eq!(snap.terminals[0].effective_cwd(), "/repo/sub");
            }
            _ => panic!("expected InitialSnapshot"),
        }

        let line2 = r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"argv":["claude"],"cwd":"/repo/wt2","name":"claude","pid":123},"seq":5,"terminal":"t1","type":"foreground"}}"#;
        let item2 = parse_hc_event_line(line2).unwrap();
        match item2 {
            HcStreamItem::Event {
                seq,
                terminal,
                event_type,
                ..
            } => {
                assert_eq!(seq, 5);
                assert_eq!(terminal.as_deref(), Some("t1"));
                assert_eq!(event_type, "foreground");
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn test_asymmetric_clear_hysteresis_and_child_rollup() {
        let mut tracker = FleetTracker::new();
        let parent = HcTerminal {
            address: "t1".into(),
            name: Some("lead".into()),
            state: "live".into(),
            reported: true,
            status: Some(HcStatus {
                state: "working".into(),
                ..Default::default()
            }),
            turn: 1,
            ..Default::default()
        };
        let child = HcTerminal {
            address: "t2".into(),
            name: Some("sub".into()),
            parent: Some("t1".into()),
            state: "live".into(),
            reported: true,
            status: Some(HcStatus {
                state: "blocked".into(),
                kind: Some("permission".into()),
                msg: Some("Run rm?".into()),
                app: None,
            }),
            status_ms: Some(1_000),
            turn: 1,
            ..Default::default()
        };
        tracker.apply_snapshot(vec![parent, child], 1_000);

        // Child blocked rolls up to parent
        let rollup = tracker.blocked_children_map(1_000);
        assert_eq!(rollup.get("t1"), Some(&vec!["t2".to_string()]));

        // Child transitions to working at t=1100ms -> still held as Blocked at t=2000ms (<1500ms)
        let ev = parse_hc_event_line(
            r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"working"},"seq":2,"terminal":"t2","type":"status"}}"#,
        )
        .unwrap();
        tracker.apply_stream_item(ev, 1_100);
        assert!(matches!(
            tracker.effective_attention("t2", 2_000),
            AttentionState::Blocked { .. }
        ));

        // After 1500ms (t=2700ms), clears to Working
        assert!(matches!(
            tracker.effective_attention("t2", 2_700),
            AttentionState::Working { .. }
        ));
    }

    #[test]
    fn test_finished_terminal_with_stale_blocked_label_returns_done() {
        let mut labels = BTreeMap::new();
        labels.insert("cockpit.state".into(), "blocked".into());
        labels.insert("cockpit.kind".into(), "permission".into());
        let term = HcTerminal {
            address: "t99".into(),
            state: "finished".into(),
            exit_code: Some(0),
            labels,
            ..Default::default()
        };
        assert_eq!(
            term.raw_attention_state(1_000),
            AttentionState::Done { message: None }
        );
    }

    #[test]
    fn test_attach_client_foreground_is_treated_as_plain_shell() {
        let term = HcTerminal {
            address: "t50".into(),
            state: "live".into(),
            foreground: Some(HcForeground {
                name: Some("herdr-core".into()),
                argv: vec!["herdr-core".into(), "attach".into(), "lore-agy".into()],
                cwd: None,
                pid: Some(1234),
            }),
            ..Default::default()
        };
        assert!(term.is_plain_shell_foreground());

        let attached_to_named_agent = HcTerminal {
            address: "t51".into(),
            state: "live".into(),
            foreground: Some(HcForeground {
                name: Some("hc".into()),
                argv: vec!["hc".into(), "attach".into(), "claude".into()],
                cwd: None,
                pid: Some(5678),
            }),
            ..Default::default()
        };
        assert!(attached_to_named_agent.is_plain_shell_foreground());
    }

    #[test]
    fn test_active_children_map_and_resolved_parent() {
        let mut tracker = FleetTracker::new();
        let coord = HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "idle".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        // t2: orchestrated worker with active dispatch (even if currently idle at prompt)
        let mut t2_labels = BTreeMap::new();
        t2_labels.insert("cockpit.orch.role".into(), "worker".into());
        t2_labels.insert("cockpit.parent".into(), "t1".into());
        let worker_active = HcTerminal {
            address: "t2".into(),
            state: "live".into(),
            labels: t2_labels,
            status: Some(HcStatus {
                state: "idle".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        // t3: orchestrated worker whose dispatch already completed (not in active_orch_children)
        let mut t3_labels = BTreeMap::new();
        t3_labels.insert("cockpit.orch.role".into(), "worker".into());
        t3_labels.insert("cockpit.parent".into(), "t1".into());
        let worker_completed = HcTerminal {
            address: "t3".into(),
            state: "live".into(),
            labels: t3_labels,
            status: Some(HcStatus {
                state: "working".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        // t4: non-orchestrated child via cockpit.parent label that is Working
        let mut t4_labels = BTreeMap::new();
        t4_labels.insert("cockpit.parent".into(), "t1".into());
        let adhoc_working = HcTerminal {
            address: "t4".into(),
            state: "live".into(),
            labels: t4_labels,
            status: Some(HcStatus {
                state: "working".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        // t5: non-orchestrated child that is Idle -> should NOT be in active_children
        let adhoc_idle = HcTerminal {
            address: "t5".into(),
            parent: Some("t1".into()),
            state: "live".into(),
            status: Some(HcStatus {
                state: "idle".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        tracker.apply_snapshot(
            vec![
                coord,
                worker_active,
                worker_completed,
                adhoc_working,
                adhoc_idle,
            ],
            1_000,
        );

        let mut active_orch = BTreeMap::new();
        active_orch.insert("t1".to_string(), vec!["t2".to_string()]);
        let active_map = tracker.active_children_map(&active_orch, 1_000);
        assert_eq!(
            active_map.get("t1"),
            Some(&vec!["t2".to_string(), "t4".to_string()])
        );
    }

    #[test]
    fn test_hc_first_message_deserialization() {
        let raw = r#"{"address":"t8","state":"live","created_ms":5000,"turn":1,"first_message":{"sent":false,"reason":"trust_dialog","started":false,"turn":0}}"#;
        let term: HcTerminal = serde_json::from_str(raw).unwrap();
        let fm = term.first_message.unwrap();
        assert!(!fm.sent);
        assert_eq!(fm.reason.as_deref(), Some("trust_dialog"));
        assert_eq!(fm.started, Some(false));
        assert_eq!(fm.turn, Some(0));
    }

    #[tokio::test]
    async fn test_run_worker_and_read_tail_lines() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        let args_file = tmp.path().join("args.txt");
        let script = format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"run\" ]; then\n\
               printf '%s\\n' \"$@\" > \"{}\"\n\
               echo '{{\"address\":\"t12\",\"state\":\"live\",\"created_ms\":4200,\"turn\":1,\"first_message\":{{\"sent\":true,\"started\":true,\"turn\":1}}}}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"read\" ]; then\n\
               printf 'line 1\\nline 2\\nline 3\\n'\n\
               exit 0\n\
             fi\n\
             exit 1\n",
            args_file.display()
        );
        crate::test_support::write_executable(&fake_hc, script);

        let client = HerdrClient::new(fake_hc.to_string_lossy().to_string());
        let labels = [
            ("cockpit.orch.role", "worker"),
            ("cockpit.orch.run", "run_1"),
        ];
        let agent_argv = vec!["codex".to_string(), "--no-daemon".to_string()];
        let term = client
            .run_worker("t1", tmp.path(), &labels, &agent_argv, "[TCHELVIS] Do task")
            .await
            .unwrap();
        assert_eq!(term.address, "t12");
        assert_eq!(term.created_ms, Some(4200));
        assert!(term.first_message.unwrap().sent);

        let recorded_args = fs::read_to_string(&args_file).unwrap();
        let arg_lines: Vec<&str> = recorded_args.lines().collect();
        assert!(arg_lines.contains(&"--bg"));
        assert!(arg_lines.contains(&"--no-notify"));
        assert!(arg_lines.contains(&"--outlive-parent"));
        assert!(arg_lines.contains(&"codex"));
        assert!(arg_lines.contains(&"--no-daemon"));
        assert_eq!(arg_lines.last().copied(), Some("[TCHELVIS] Do task"));

        let tail = client.read_tail_lines("t12", 2).await.unwrap();
        assert_eq!(tail, "line 2\nline 3");
    }

    /// hc 0.1.21 `run --json` returns `{address, agent, cwd, state, turn}` with no
    /// `created_ms`; the real creation time only comes from `hc get`. Attempt
    /// fencing compares against that value, so it must not be left empty.
    #[tokio::test]
    async fn test_run_worker_fetches_created_ms_when_run_output_lacks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_hc = tmp.path().join("fake-hc");
        let script = "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"run\" ]; then\n\
               echo '{\"address\":\"t4\",\"agent\":\"pi\",\"cwd\":\"/r\",\"state\":\"live\",\"turn\":0}'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"get\" ]; then\n\
               echo '{\"address\":\"t4\",\"created_ms\":1795046353550,\"state\":\"live\",\"turn\":0,\"labels\":{}}'\n\
               exit 0\n\
             fi\n\
             exit 1\n";
        crate::test_support::write_executable(&fake_hc, script);

        let client = HerdrClient::new(fake_hc.to_string_lossy().to_string());
        let agent_argv = vec!["pi".to_string()];
        let term = client
            .run_worker("t1", tmp.path(), &[], &agent_argv, "[TCHELVIS] Do task")
            .await
            .unwrap();
        assert_eq!(term.address, "t4");
        assert_eq!(term.created_ms, Some(1795046353550));
    }

    #[test]
    fn lot_l_build_run_worker_args_keeps_spaced_model_as_single_argv_entry() {
        let agent_argv = vec![
            "agy".to_string(),
            "--model".to_string(),
            "GPT-OSS 120B (Medium)".to_string(),
            "-i".to_string(),
        ];
        let args = HerdrClient::build_run_worker_args(
            "t1",
            Path::new("/tmp/wt"),
            &[("cockpit.orch.role", "worker")],
            &agent_argv,
            "[TCHELVIS] Do task",
        )
        .unwrap();
        let str_args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let agy_pos = str_args
            .iter()
            .position(|a| a == "agy")
            .expect("agy should be present");
        assert_eq!(
            &str_args[agy_pos..],
            &[
                "agy",
                "--model",
                "GPT-OSS 120B (Medium)",
                "-i",
                "[TCHELVIS] Do task",
            ]
        );

        assert!(HerdrClient::build_run_worker_args(
            "t1",
            Path::new("/tmp/wt"),
            &[],
            &[],
            "[TCHELVIS] Do task",
        )
        .is_err());
    }

    #[test]
    fn test_effective_attention_ms_no_heuristic() {
        let mut tracker = FleetTracker::new();
        let term = HcTerminal {
            address: "t2".into(),
            state: "live".into(),
            reported: true,
            status: Some(HcStatus {
                state: "blocked".into(),
                kind: Some("permission".into()),
                msg: Some("Approve?".into()),
                app: None,
            }),
            status_ms: Some(1_000_000),
            turn: 1,
            ..Default::default()
        };
        tracker.apply_snapshot(vec![term], 1_000_000);

        let ev = parse_hc_event_line(
            r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"idle"},"seq":2,"terminal":"t2","type":"status"}}"#,
        )
        .unwrap();
        tracker.apply_stream_item(ev, 1_000_000);

        // 2_000 <= 1_000_000 / 500 previously triggered the *1000 heuristic and falsely cleared Blocked.
        assert!(matches!(
            tracker.effective_attention("t2", 2_000),
            AttentionState::Blocked { .. }
        ));
        // Still held within 1500ms hysteresis window.
        assert!(matches!(
            tracker.effective_attention("t2", 1_001_400),
            AttentionState::Blocked { .. }
        ));
        // Clears once 1500ms has elapsed.
        assert_eq!(
            tracker.effective_attention("t2", 1_001_500),
            AttentionState::Idle
        );
    }

    #[test]
    fn test_active_children_map_uses_ms_hysteresis() {
        let mut tracker = FleetTracker::new();
        let parent = HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            status: Some(HcStatus {
                state: "idle".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let child = HcTerminal {
            address: "t2".into(),
            parent: Some("t1".into()),
            state: "live".into(),
            reported: true,
            status: Some(HcStatus {
                state: "blocked".into(),
                kind: Some("permission".into()),
                msg: Some("Confirm?".into()),
                app: None,
            }),
            status_ms: Some(10_000),
            turn: 1,
            ..Default::default()
        };
        tracker.apply_snapshot(vec![parent, child], 10_000);

        let clear_ev = parse_hc_event_line(
            r#"{"jsonrpc":"2.0","method":"event","params":{"data":{"state":"idle"},"seq":2,"terminal":"t2","type":"status"}}"#,
        )
        .unwrap();
        tracker.apply_stream_item(clear_ev, 10_200);

        let empty_orch = BTreeMap::new();
        assert_eq!(
            tracker.active_children_map(&empty_orch, 11_600).get("t1"),
            Some(&vec!["t2".to_string()])
        );
        assert!(!tracker
            .active_children_map(&empty_orch, 11_800)
            .contains_key("t1"));
    }

    #[test]
    fn test_default_hc_bin_precedence_and_no_home_fallback() {
        let local_hc = std::path::PathBuf::from("/home/alice/.local/bin/hc");
        let cargo_hc = std::path::PathBuf::from("/home/alice/.cargo/bin/hc");

        // 1. HC_BIN wins over everything
        assert_eq!(
            default_hc_bin_with(
                |k| match k {
                    "HC_BIN" => Some("/custom/hc".to_string()),
                    "HOME" => Some("/home/alice".to_string()),
                    _ => None,
                },
                |_| true,
            ),
            "/custom/hc"
        );

        // 2. $HOME/.local/bin/hc wins over $HOME/.cargo/bin/hc
        assert_eq!(
            default_hc_bin_with(
                |k| (k == "HOME").then(|| "/home/alice".to_string()),
                |p| p == local_hc || p == cargo_hc,
            ),
            "/home/alice/.local/bin/hc"
        );

        // 3. $HOME/.cargo/bin/hc used when .local/bin/hc absent
        assert_eq!(
            default_hc_bin_with(
                |k| (k == "HOME").then(|| "/home/alice".to_string()),
                |p| p == cargo_hc,
            ),
            "/home/alice/.cargo/bin/hc"
        );

        // 4. Unknown HOME skips home candidates and falls back to "hc" on PATH
        assert_eq!(default_hc_bin_with(|_| None, |_| true), "hc");
    }

    #[test]
    fn test_parse_hc_json_error_extracts_kind_and_message() {
        let json = br#"{"error":{"hint":"nothing was typed","kind":"status_mismatch","message":"t1b3 is done, not idle"}}"#;
        let info = parse_hc_json_error(json, b"").expect("expected parsed HcErrorInfo");
        assert_eq!(info.kind.as_deref(), Some("status_mismatch"));
        assert_eq!(info.message, "t1b3 is done, not idle");

        let from_stderr = br#"{"error":{"kind":"blocked","message":"t2 is blocked"}}"#;
        let info2 = parse_hc_json_error(b"", from_stderr).expect("expected stderr parsed");
        assert_eq!(info2.kind.as_deref(), Some("blocked"));
        assert_eq!(info2.message, "t2 is blocked");
    }

    #[test]
    fn test_classify_terminal_for_send_matrix() {
        let make = |state: &str, status_state: Option<&str>| HcTerminal {
            address: "t1".into(),
            state: state.into(),
            reported: status_state.is_some(),
            status: status_state.map(|s| HcStatus {
                state: s.into(),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(
            classify_terminal_for_send(None, None),
            ("missing".into(), Err("terminal_missing_from_tracker"))
        );
        assert_eq!(
            classify_terminal_for_send(Some(&make("finished", Some("idle"))), None),
            ("finished".into(), Err("terminal_finished"))
        );
        assert_eq!(
            classify_terminal_for_send(Some(&make("live", Some("idle"))), None),
            ("idle".into(), Ok(GuardedSendMode::IfIdle))
        );
        assert_eq!(
            classify_terminal_for_send(Some(&make("live", Some("done"))), None),
            ("done".into(), Ok(GuardedSendMode::DoneRecheckThenPlain))
        );
        assert_eq!(
            classify_terminal_for_send(Some(&make("live", Some("working"))), None),
            ("working".into(), Err("terminal_working"))
        );
        assert_eq!(
            classify_terminal_for_send(Some(&make("live", Some("blocked"))), None),
            ("blocked".into(), Err("terminal_blocked"))
        );
        assert_eq!(
            classify_terminal_for_send(Some(&make("live", Some("error"))), None),
            ("error".into(), Err("terminal_error"))
        );
        assert_eq!(
            classify_terminal_for_send(Some(&make("live", None)), None),
            ("unknown".into(), Err("terminal_unknown"))
        );
        // Hysteresis attention override holds blocked even when status turned idle
        assert_eq!(
            classify_terminal_for_send(
                Some(&make("live", Some("idle"))),
                Some(AttentionState::Blocked {
                    kind: crate::model::BlockedKind::Permission,
                    message: None,
                    since_ms: 0,
                }),
            ),
            ("blocked".into(), Err("terminal_blocked"))
        );
    }

    #[tokio::test]
    async fn test_send_guarded_by_status_recheck_and_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let calls_log = tmp.path().join("calls.log");
        let get_state_file = tmp.path().join("get_state");
        let send_mode_file = tmp.path().join("send_behavior");
        std::fs::write(&get_state_file, "done").unwrap();
        std::fs::write(&send_mode_file, "ok").unwrap();

        let fake_hc = tmp.path().join("fake-hc");
        let script = format!(
            "#!/usr/bin/env bash\n\
             echo \"$*\" >> \"{}\"\n\
             if [ \"$1\" = \"get\" ]; then\n\
               st=$(cat \"{}\")\n\
               echo \"{{\\\"address\\\":\\\"$2\\\",\\\"state\\\":\\\"live\\\",\\\"status\\\":{{\\\"state\\\":\\\"$st\\\",\\\"app\\\":\\\"claude\\\"}}}}\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"send\" ]; then\n\
               beh=$(cat \"{}\")\n\
               if [ \"$beh\" = \"mismatch\" ]; then\n\
                 echo '{{\"error\":{{\"hint\":\"nothing was typed\",\"kind\":\"status_mismatch\",\"message\":\"t1b3 is done, not idle\"}}}}'\n\
                 exit 10\n\
               elif [ \"$beh\" = \"sleep\" ]; then\n\
                 sleep 2\n\
                 exit 0\n\
               fi\n\
               echo '{{\"started\":false,\"status\":\"done\"}}'\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            calls_log.display(),
            get_state_file.display(),
            send_mode_file.display(),
        );
        crate::test_support::write_executable(&fake_hc, script);

        let herdr = HerdrClient::new(fake_hc.to_string_lossy().to_string());
        let make = |status: &str| HcTerminal {
            address: "t1b3".into(),
            state: "live".into(),
            reported: true,
            status: Some(HcStatus {
                state: status.into(),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 1. Working terminal -> deferred without spawning hc
        let res_work = herdr
            .send_guarded(&make("working"), None, "hello", None)
            .await;
        assert_eq!(
            res_work,
            GuardedSendOutcome::Deferred {
                reason: "terminal_working".into(),
                detail: None,
            }
        );
        assert!(!calls_log.exists());

        // 2. Idle terminal -> calls `send --if idle`
        let res_idle = herdr.send_guarded(&make("idle"), None, "hello", None).await;
        assert_eq!(
            res_idle,
            GuardedSendOutcome::Sent {
                used_plain_send: false
            }
        );
        let calls1 = std::fs::read_to_string(&calls_log).unwrap();
        assert!(calls1.contains("send --if idle t1b3 hello --json"));

        // 3. Done terminal -> calls `get` first, sees `done`, then plain `send` (no `--if idle`)
        std::fs::write(&calls_log, "").unwrap();
        std::fs::write(&get_state_file, "done").unwrap();
        let res_done = herdr.send_guarded(&make("done"), None, "hello", None).await;
        assert_eq!(
            res_done,
            GuardedSendOutcome::Sent {
                used_plain_send: true
            }
        );
        let calls2 = std::fs::read_to_string(&calls_log).unwrap();
        let lines2: Vec<&str> = calls2.lines().collect();
        assert_eq!(lines2.len(), 2, "expected get + plain send, got: {calls2}");
        assert!(lines2[0].starts_with("get t1b3 --json"));
        assert!(lines2[1].starts_with("send t1b3 hello --json"));
        assert!(!lines2[1].contains("--if"));

        // 4. Done terminal flips to `working` during recheck -> deferred, no `send` issued
        std::fs::write(&calls_log, "").unwrap();
        std::fs::write(&get_state_file, "working").unwrap();
        let res_flipped = herdr.send_guarded(&make("done"), None, "hello", None).await;
        assert!(matches!(
            res_flipped,
            GuardedSendOutcome::Deferred { ref reason, .. } if reason == "terminal_working"
        ));
        let calls3 = std::fs::read_to_string(&calls_log).unwrap();
        assert_eq!(calls3.lines().count(), 1, "must not call send: {calls3}");

        // 5. Idle terminal returns exit 10 status_mismatch JSON -> detail preserved
        std::fs::write(&send_mode_file, "mismatch").unwrap();
        let res_mismatch = herdr.send_guarded(&make("idle"), None, "hello", None).await;
        assert_eq!(
            res_mismatch,
            GuardedSendOutcome::Deferred {
                reason: "status_mismatch".into(),
                detail: Some("t1b3 is done, not idle".into()),
            }
        );

        // 6. Slow send hits timeout -> Deferred(send_timeout)
        std::fs::write(&send_mode_file, "sleep").unwrap();
        let res_timeout = herdr
            .send_guarded(
                &make("idle"),
                None,
                "hello",
                Some(Duration::from_millis(60)),
            )
            .await;
        assert!(matches!(
            res_timeout,
            GuardedSendOutcome::Deferred { ref reason, .. } if reason == "send_timeout"
        ));
    }

    #[test]
    fn test_build_run_worker_args_with_name_and_name_rejection_filter() {
        let args = HerdrClient::build_run_worker_args_with_name(
            "t1",
            Path::new("/repo"),
            &[("cockpit.loop_run", "20261124-193000-cafe")],
            &["tchelvis".into(), "loop".into(), "engine".into()],
            "20261124-193000-cafe",
            Some("loop-20261124-193000-cafe"),
        )
        .unwrap();
        let strs: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let name_pos = strs.iter().position(|s| s == "--name").unwrap();
        assert_eq!(strs[name_pos + 1], "loop-20261124-193000-cafe");

        // Real hc JSON invalid_params for --name -> retries safely
        let valid_name_err = HcRunError {
            exit_code: Some(6),
            stderr: String::new(),
            stdout: r#"{"error":{"hint":"see `hc schema`","kind":"invalid_params","message":"bad name \"foo\": use 1-32"}}"#.into(),
            message: "hc run failed".into(),
        };
        assert!(HerdrClient::is_hc_name_rejection_error(&valid_name_err));

        // Ambiguous/internal error -> NEVER retries
        let internal_err = HcRunError {
            exit_code: Some(1),
            stderr: "daemon timeout".into(),
            stdout: String::new(),
            message: "hc run failed".into(),
        };
        assert!(!HerdrClient::is_hc_name_rejection_error(&internal_err));

        let name_taken = HcRunError {
            exit_code: Some(1),
            stderr: String::new(),
            stdout: r#"{"error":{"hint":"pick another name, or kill the one holding it","kind":"name_taken","message":"a live terminal is already named \"loop-x\""}}"#.into(),
            message: "hc run failed".into(),
        };
        assert!(!HerdrClient::is_hc_name_rejection_error(&name_taken));

        let non_name_invalid_params = HcRunError {
            exit_code: Some(6),
            stderr: String::new(),
            stdout: r#"{"error":{"kind":"invalid_params","message":"unknown parent terminal"}}"#
                .into(),
            message: "hc run failed".into(),
        };
        assert!(!HerdrClient::is_hc_name_rejection_error(
            &non_name_invalid_params
        ));
    }
}
