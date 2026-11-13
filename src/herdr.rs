use crate::model::{
    sanitize_terminal_text, AgentLifecycle, AttentionState, BlockedKind, FleetSummaryProjection,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub const BLOCKED_CLEAR_HYSTERESIS_MS: i64 = 1_500;

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
pub struct HcTerminal {
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
}

impl HcTerminal {
    pub fn display_name(&self) -> String {
        self.name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(&self.address)
            .to_string()
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

    pub fn raw_attention_state(&self, now_ms: i64) -> AttentionState {
        let Some(st) = &self.status else {
            return AttentionState::Idle;
        };
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
                AttentionState::Blocked {
                    kind,
                    message: msg,
                    since_ms: self.status_ms.unwrap_or(now_ms),
                }
            }
            "working" => AttentionState::Working { message: msg },
            "done" => AttentionState::Done { message: msg },
            "error" | "failed" => AttentionState::Error { message: msg },
            _ => AttentionState::Idle,
        }
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

    /// Evaluates effective attention state for `addr` at `now_ms`, applying 1.5s clear-hysteresis.
    pub fn effective_attention(&mut self, addr: &str, now_ms: i64) -> AttentionState {
        let Some(term) = self.terminals.get(addr) else {
            return AttentionState::Idle;
        };
        if term.state == "finished" {
            self.blocked_hold.remove(addr);
            return term.raw_attention_state(now_ms);
        }
        let raw = term.raw_attention_state(now_ms);
        if matches!(raw, AttentionState::Blocked { .. }) {
            self.blocked_hold
                .insert(addr.to_string(), (raw.clone(), None));
            return raw;
        }
        if let Some((held_state, clear_since)) = self.blocked_hold.get_mut(addr) {
            let since = *clear_since.get_or_insert(now_ms);
            if now_ms - since < BLOCKED_CLEAR_HYSTERESIS_MS {
                return held_state.clone();
            }
            let _ = self.blocked_hold.remove(addr);
        }
        raw
    }

    /// Computes child terminals (`parent == Some(parent_addr)`) that are currently in `Blocked` state.
    pub fn blocked_children_map(&mut self, now_ms: i64) -> BTreeMap<String, Vec<String>> {
        let addresses: Vec<String> = self.terminals.keys().cloned().collect();
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for addr in addresses {
            let att = self.effective_attention(&addr, now_ms);
            if matches!(att, AttentionState::Blocked { .. }) {
                if let Some(parent) = self
                    .terminals
                    .get(&addr)
                    .and_then(|t| t.parent.clone())
                    .filter(|p| !p.is_empty())
                {
                    map.entry(parent).or_default().push(addr);
                }
            }
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

impl Default for HerdrClient {
    fn default() -> Self {
        Self::new("hc")
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
            .spawn()
            .with_context(|| format!("failed spawning `{} events --json`", self.hc_bin))?;
        let stdout = BufReader::new(child.stdout.take().context("missing hc events stdout")?);
        Ok((child, stdout))
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
}
