use crate::logging::redact_secrets;
use crate::loops::state::{sanitize_single_line, Usage};
use crate::loops::workflow::{Provider, Stage};
use anyhow::{bail, Result};
use std::ffi::OsString;
use std::fmt::Write as _;

pub const MAX_AGY_PROMPT_BYTES: usize = 100 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCommand {
    pub argv: Vec<OsString>,
    pub stdin: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdapterResult {
    pub usage: Usage,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamState {
    pub last_event: Option<String>,
    pub running_tools: Option<Vec<String>>,
    pub usage: Usage,
    pub error: Option<String>,
}

pub fn command(p: Provider, model: Option<&str>, prompt: &str) -> Result<AdapterCommand> {
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    match p {
        Provider::Claude => {
            let mut argv: Vec<OsString> = [
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--setting-sources",
                "project",
                "--permission-mode",
                "bypassPermissions",
            ]
            .into_iter()
            .map(OsString::from)
            .collect();
            if let Some(m) = model {
                argv.push(OsString::from("--model"));
                argv.push(OsString::from(m));
            }
            Ok(AdapterCommand {
                argv,
                stdin: prompt.as_bytes().to_vec(),
            })
        }
        Provider::Agy => {
            if prompt.len() > MAX_AGY_PROMPT_BYTES {
                bail!(
                    "prompt para 'agy' excede o limite de {} bytes ({} bytes)",
                    MAX_AGY_PROMPT_BYTES,
                    prompt.len()
                );
            }
            if prompt.contains('\0') {
                bail!("prompt para 'agy' contém byte nulo");
            }
            let mut argv: Vec<OsString> = vec![
                OsString::from("agy"),
                OsString::from("-p"),
                OsString::from(prompt),
                OsString::from("--output-format"),
                OsString::from("stream-json"),
                OsString::from("--print-timeout"),
                OsString::from("0"),
                OsString::from("--dangerously-skip-permissions"),
            ];
            if let Some(m) = model {
                argv.push(OsString::from("--model"));
                argv.push(OsString::from(m));
            }
            Ok(AdapterCommand {
                argv,
                stdin: Vec::new(),
            })
        }
        Provider::Opencode => {
            let mut argv: Vec<OsString> = [
                "opencode",
                "run",
                "--standalone",
                "--format",
                "json",
                "--auto",
            ]
            .into_iter()
            .map(OsString::from)
            .collect();
            if let Some(m) = model {
                argv.push(OsString::from("-m"));
                argv.push(OsString::from(m));
            }
            Ok(AdapterCommand {
                argv,
                stdin: prompt.as_bytes().to_vec(),
            })
        }
        Provider::Pi => {
            let Some(m) = model else {
                bail!("'pi' exige modelo no formato provedor/id");
            };
            let argv: Vec<OsString> = [
                "pi",
                "-p",
                "--mode",
                "json",
                "--no-session",
                "--offline",
                "--no-approve",
                "--no-extensions",
                "--no-skills",
                "--no-mcp",
                "--no-prompt-templates",
                "--no-themes",
                "--model",
                m,
            ]
            .into_iter()
            .map(OsString::from)
            .collect();
            Ok(AdapterCommand {
                argv,
                stdin: prompt.as_bytes().to_vec(),
            })
        }
    }
}

fn sanitize_error_text(raw: &str) -> String {
    let single = sanitize_single_line(raw, None);
    let redacted = redact_secrets(&single);
    if redacted.is_empty() {
        "erro desconhecido".to_string()
    } else {
        redacted
    }
}

fn claude_tool_name(val: &serde_json::Value) -> Option<String> {
    if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
        return None;
    }
    val.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                    return None;
                }
                block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
        })
}

/// What the CLI said about the tools it has open right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolView {
    Open(Vec<String>),
    NoneOpen,
    /// The CLI never says when a tool starts (`opencode`); `step` counts step boundaries.
    Unseen {
        step: u64,
    },
}

pub struct EventParser {
    provider: Provider,
    saw_result: bool,
    usage: Usage,
    error: Option<String>,
    agy_turns: u64,
    opencode_steps: u64,
    /// Step boundaries, the only progress signal `opencode` gives while it is silent.
    opencode_boundaries: u64,
    opencode_in_sum: u64,
    opencode_in_seen: u64,
    opencode_out_sum: u64,
    opencode_out_seen: u64,
    opencode_cache_sum: u64,
    opencode_cache_seen: u64,
    opencode_cost_sum: f64,
    opencode_cost_seen: u64,
    pi_turns: u64,
    pi_assistant_msgs: u64,
    pi_in_sum: u64,
    pi_in_seen: u64,
    pi_out_sum: u64,
    pi_out_seen: u64,
    pi_cache_sum: u64,
    pi_cache_seen: u64,
    pi_cost_sum: f64,
    pi_cost_seen: u64,
    pi_last_stop: Option<String>,
    pi_last_error: Option<String>,
    pi_retry_failed: Option<String>,
    pi_settled: bool,
    pi_aborted: bool,
    last_event: Option<String>,
    open_tools: Vec<(String, String)>,
}

impl EventParser {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            saw_result: false,
            usage: Usage::default(),
            error: None,
            agy_turns: 0,
            opencode_steps: 0,
            opencode_boundaries: 0,
            opencode_in_sum: 0,
            opencode_in_seen: 0,
            opencode_out_sum: 0,
            opencode_out_seen: 0,
            opencode_cache_sum: 0,
            opencode_cache_seen: 0,
            opencode_cost_sum: 0.0,
            opencode_cost_seen: 0,
            pi_turns: 0,
            pi_assistant_msgs: 0,
            pi_in_sum: 0,
            pi_in_seen: 0,
            pi_out_sum: 0,
            pi_out_seen: 0,
            pi_cache_sum: 0,
            pi_cache_seen: 0,
            pi_cost_sum: 0.0,
            pi_cost_seen: 0,
            pi_last_stop: None,
            pi_last_error: None,
            pi_retry_failed: None,
            pi_settled: false,
            pi_aborted: false,
            last_event: None,
            open_tools: Vec::new(),
        }
    }

    pub fn line(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };

        self.record_event(&val);
        match self.provider {
            Provider::Claude => self.handle_claude(&val),
            Provider::Agy => self.handle_agy(&val),
            Provider::Opencode => self.handle_opencode(&val),
            Provider::Pi => self.handle_pi(&val),
        }
    }

    fn handle_claude(&mut self, val: &serde_json::Value) {
        if val.get("type").and_then(|v| v.as_str()) != Some("result") {
            return;
        }
        self.saw_result = true;
        self.usage.turns = val.get("num_turns").and_then(|v| v.as_u64());
        if let Some(u) = val.get("usage") {
            self.usage.input_tokens = u.get("input_tokens").and_then(|v| v.as_u64());
            self.usage.output_tokens = u.get("output_tokens").and_then(|v| v.as_u64());
            self.usage.cache_read_tokens =
                u.get("cache_read_input_tokens").and_then(|v| v.as_u64());
        } else {
            self.usage.input_tokens = None;
            self.usage.output_tokens = None;
            self.usage.cache_read_tokens = None;
        }
        self.usage.cost_usd = val
            .get("total_cost_usd")
            .and_then(|v| v.as_f64())
            .filter(|c| c.is_finite());

        if val.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
            let msg = val
                .get("result")
                .and_then(|v| v.as_str())
                .or_else(|| val.get("error").and_then(|v| v.as_str()))
                .unwrap_or("claude retornou is_error=true");
            self.error = Some(sanitize_error_text(msg));
        } else {
            self.error = None;
        }
    }

    fn handle_agy(&mut self, val: &serde_json::Value) {
        match val.get("event").and_then(|v| v.as_str()) {
            Some("step_update") => {
                if let Some(su) = val.get("step_update") {
                    let is_done = su.get("state").and_then(|v| v.as_str()) == Some("DONE");
                    let is_agent =
                        su.get("step_type").and_then(|v| v.as_str()) == Some("agent_response");
                    if is_done && is_agent {
                        self.agy_turns = self.agy_turns.saturating_add(1);
                    }
                }
            }
            Some("result") => {
                self.saw_result = true;
                let Some(res) = val.get("result") else {
                    self.error = Some("agy retornou evento 'result' sem corpo".to_string());
                    return;
                };
                if let Some(u) = res.get("usage") {
                    self.usage.input_tokens = u.get("input_tokens").and_then(|v| v.as_u64());
                    self.usage.output_tokens = u.get("output_tokens").and_then(|v| v.as_u64());
                    self.usage.cache_read_tokens =
                        u.get("cache_read_tokens").and_then(|v| v.as_u64());
                } else {
                    self.usage.input_tokens = None;
                    self.usage.output_tokens = None;
                    self.usage.cache_read_tokens = None;
                }
                self.usage.cost_usd = None;

                let status = res.get("status").and_then(|v| v.as_str());
                if status != Some("SUCCESS") {
                    let fallback =
                        format!("agy retornou status {}", status.unwrap_or("desconhecido"));
                    let msg = res
                        .get("error")
                        .and_then(|v| v.as_str())
                        .or_else(|| res.get("message").and_then(|v| v.as_str()))
                        .or_else(|| {
                            res.get("response")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.trim().is_empty())
                        })
                        .unwrap_or(&fallback);
                    self.error = Some(sanitize_error_text(msg));
                } else {
                    self.error = None;
                }
            }
            _ => {}
        }
    }

    fn handle_opencode(&mut self, val: &serde_json::Value) {
        match val.get("type").and_then(|v| v.as_str()) {
            Some("step_finish") => {
                self.opencode_steps = self.opencode_steps.saturating_add(1);
                if let Some(part) = val.get("part") {
                    if let Some(tokens) = part.get("tokens") {
                        if let Some(v) = tokens.get("input").and_then(|v| v.as_u64()) {
                            self.opencode_in_sum = self.opencode_in_sum.saturating_add(v);
                            self.opencode_in_seen += 1;
                        }
                        if let Some(v) = tokens.get("output").and_then(|v| v.as_u64()) {
                            self.opencode_out_sum = self.opencode_out_sum.saturating_add(v);
                            self.opencode_out_seen += 1;
                        }
                        if let Some(v) = tokens
                            .get("cache")
                            .and_then(|c| c.get("read"))
                            .and_then(|v| v.as_u64())
                        {
                            self.opencode_cache_sum = self.opencode_cache_sum.saturating_add(v);
                            self.opencode_cache_seen += 1;
                        }
                    }
                    if let Some(c) = part
                        .get("cost")
                        .and_then(|v| v.as_f64())
                        .filter(|c| c.is_finite())
                    {
                        self.opencode_cost_sum += c;
                        self.opencode_cost_seen += 1;
                    }
                }
            }
            Some("error") => {
                let msg = val
                    .get("error")
                    .and_then(|e| {
                        e.get("message")
                            .and_then(|m| m.as_str())
                            .or_else(|| e.as_str())
                    })
                    .or_else(|| val.get("message").and_then(|m| m.as_str()))
                    .unwrap_or("opencode retornou evento de erro");
                self.error = Some(sanitize_error_text(msg));
            }
            _ => {}
        }
    }

    fn handle_pi(&mut self, val: &serde_json::Value) {
        let message = val.get("message");
        let assistant =
            message.and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("assistant");
        match val.get("type").and_then(|v| v.as_str()) {
            Some("message_end") if assistant => {
                self.pi_assistant_msgs = self.pi_assistant_msgs.saturating_add(1);
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    if let Some(v) = usage.get("input").and_then(|v| v.as_u64()) {
                        self.pi_in_sum = self.pi_in_sum.saturating_add(v);
                        self.pi_in_seen += 1;
                    }
                    if let Some(v) = usage.get("output").and_then(|v| v.as_u64()) {
                        self.pi_out_sum = self.pi_out_sum.saturating_add(v);
                        self.pi_out_seen += 1;
                    }
                    if let Some(v) = usage.get("cacheRead").and_then(|v| v.as_u64()) {
                        self.pi_cache_sum = self.pi_cache_sum.saturating_add(v);
                        self.pi_cache_seen += 1;
                    }
                    if let Some(c) = usage
                        .get("cost")
                        .and_then(|c| c.get("total"))
                        .and_then(|v| v.as_f64())
                        .filter(|c| c.is_finite())
                    {
                        self.pi_cost_sum += c;
                        self.pi_cost_seen += 1;
                    }
                }
                self.pi_last_stop = message
                    .and_then(|m| m.get("stopReason"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                self.pi_last_error = message
                    .and_then(|m| m.get("errorMessage"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
            }
            Some("turn_end") if assistant => {
                let stop = message
                    .and_then(|m| m.get("stopReason"))
                    .and_then(|v| v.as_str());
                if matches!(stop, Some(s) if s != "error" && s != "aborted") {
                    self.pi_turns = self.pi_turns.saturating_add(1);
                }
            }
            Some("auto_retry_end") => {
                if val.get("success").and_then(|v| v.as_bool()) == Some(false) {
                    self.pi_retry_failed = Some(
                        val.get("finalError")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pi esgotou as tentativas automáticas")
                            .to_string(),
                    );
                }
            }
            Some("agent_settled") => {
                self.pi_settled = true;
                self.pi_aborted = val.get("aborted").and_then(|v| v.as_bool()) == Some(true);
            }
            _ => {}
        }
    }

    fn record_event(&mut self, val: &serde_json::Value) {
        let mut tool: Option<String> = None;
        let base: Option<String> = match self.provider {
            Provider::Claude => {
                self.track_claude_tools(val);
                tool = claude_tool_name(val);
                val.get("type").and_then(|v| v.as_str()).map(str::to_string)
            }
            Provider::Agy => {
                let event = val.get("event").and_then(|v| v.as_str());
                let mut base = event.map(str::to_string);
                if event == Some("step_update") {
                    if let Some(step) = val.get("step_update") {
                        self.track_agy_tool(step);
                        if let Some(step_type) = step.get("step_type").and_then(|v| v.as_str()) {
                            base = Some(format!("step_update {step_type}"));
                        }
                        tool = step
                            .get("tool_name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                    }
                }
                base
            }
            Provider::Opencode => {
                let kind = val.get("type").and_then(|v| v.as_str());
                if matches!(kind, Some("step_start") | Some("step_finish")) {
                    self.opencode_boundaries = self.opencode_boundaries.saturating_add(1);
                }
                if kind == Some("tool_use") {
                    tool = val
                        .get("part")
                        .and_then(|p| p.get("tool"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                kind.map(str::to_string)
            }
            Provider::Pi => {
                let kind = val.get("type").and_then(|v| v.as_str());
                self.track_pi_tool(kind, val);
                if matches!(kind, Some(k) if k.starts_with("tool_execution_")) {
                    tool = val
                        .get("toolName")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                kind.map(str::to_string)
            }
        };
        if let Some(base) = base {
            self.last_event = Some(match tool.as_deref() {
                Some(name) if !name.is_empty() => format!("{base} ({name})"),
                _ => base,
            });
        }
    }

    fn track_claude_tools(&mut self, val: &serde_json::Value) {
        let kind = val.get("type").and_then(|v| v.as_str());
        let Some(blocks) = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return;
        };
        match kind {
            Some("assistant") => {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                        continue;
                    }
                    if let (Some(id), Some(name)) = (
                        block.get("id").and_then(|v| v.as_str()),
                        block.get("name").and_then(|v| v.as_str()),
                    ) {
                        self.open_tools.push((id.to_string(), name.to_string()));
                    }
                }
            }
            Some("user") => {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                        continue;
                    }
                    if let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                        self.open_tools.retain(|(open, _)| open != id);
                    }
                }
            }
            _ => {}
        }
    }

    fn track_agy_tool(&mut self, step: &serde_json::Value) {
        if step.get("step_type").and_then(|v| v.as_str()) != Some("tool") {
            return;
        }
        let Some(index) = step.get("step_index") else {
            return;
        };
        let Some(key) = index
            .as_u64()
            .map(|n| n.to_string())
            .or_else(|| index.as_str().map(str::to_string))
        else {
            return;
        };
        if step.get("state").and_then(|v| v.as_str()) == Some("ACTIVE") {
            let name = step
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !name.is_empty() && !self.open_tools.iter().any(|(open, _)| open == &key) {
                self.open_tools.push((key, name.to_string()));
            }
        } else {
            self.open_tools.retain(|(open, _)| open != &key);
        }
    }

    fn track_pi_tool(&mut self, kind: Option<&str>, val: &serde_json::Value) {
        let Some(id) = val.get("toolCallId").and_then(|v| v.as_str()) else {
            return;
        };
        match kind {
            Some("tool_execution_start") => {
                let name = val
                    .get("toolName")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !name.is_empty() {
                    self.open_tools.push((id.to_string(), name.to_string()));
                }
            }
            Some("tool_execution_end") => self.open_tools.retain(|(open, _)| open != id),
            _ => {}
        }
    }

    fn apply_usage(&mut self) {
        match self.provider {
            Provider::Claude => {}
            Provider::Agy => {
                self.usage.turns = (self.agy_turns > 0).then_some(self.agy_turns);
            }
            Provider::Opencode => {
                if self.opencode_steps > 0 {
                    self.usage.turns = Some(self.opencode_steps);
                    self.usage.input_tokens = (self.opencode_in_seen == self.opencode_steps)
                        .then_some(self.opencode_in_sum);
                    self.usage.output_tokens = (self.opencode_out_seen == self.opencode_steps)
                        .then_some(self.opencode_out_sum);
                    self.usage.cache_read_tokens = (self.opencode_cache_seen
                        == self.opencode_steps)
                        .then_some(self.opencode_cache_sum);
                    self.usage.cost_usd = (self.opencode_cost_seen == self.opencode_steps)
                        .then_some(self.opencode_cost_sum);
                }
            }
            Provider::Pi => {
                if self.pi_turns > 0 {
                    self.usage.turns = Some(self.pi_turns);
                }
                if self.pi_assistant_msgs > 0 {
                    let responses = self.pi_assistant_msgs;
                    self.usage.input_tokens =
                        (self.pi_in_seen == responses).then_some(self.pi_in_sum);
                    self.usage.output_tokens =
                        (self.pi_out_seen == responses).then_some(self.pi_out_sum);
                    self.usage.cache_read_tokens =
                        (self.pi_cache_seen == responses).then_some(self.pi_cache_sum);
                    self.usage.cost_usd =
                        (self.pi_cost_seen == responses).then_some(self.pi_cost_sum);
                }
            }
        }
    }

    pub fn tool_view(&self) -> ToolView {
        match self.provider {
            Provider::Opencode => ToolView::Unseen {
                step: self.opencode_boundaries,
            },
            _ => {
                if self.open_tools.is_empty() {
                    ToolView::NoneOpen
                } else {
                    ToolView::Open(
                        self.open_tools
                            .iter()
                            .map(|(_, name)| name.clone())
                            .collect(),
                    )
                }
            }
        }
    }

    /// Errors the stream itself reported, never the "ended without event" ones.
    fn explicit_error(&mut self) -> Option<String> {
        if self.provider != Provider::Pi {
            return self.error.take();
        }
        let stop = self.pi_last_stop.clone();
        if let Some(final_error) = self.pi_retry_failed.take() {
            return Some(sanitize_error_text(&final_error));
        }
        if matches!(stop.as_deref(), Some("error") | Some("aborted")) {
            let reason = self
                .pi_last_error
                .clone()
                .unwrap_or_else(|| format!("pi retornou stopReason {}", stop.unwrap_or_default()));
            return Some(sanitize_error_text(&reason));
        }
        None
    }

    pub fn finish(mut self) -> AdapterResult {
        self.apply_usage();
        let mut error = self.explicit_error();
        if error.is_none() {
            match self.provider {
                Provider::Claude => {
                    if !self.saw_result {
                        error = Some("claude terminou sem evento 'result'".to_string());
                    }
                }
                Provider::Agy => {
                    if !self.saw_result {
                        error = Some("agy terminou sem evento 'result'".to_string());
                    }
                }
                Provider::Opencode => {}
                Provider::Pi => {
                    let stop = self.pi_last_stop.clone();
                    error = if self.pi_settled && self.pi_aborted {
                        Some("pi abortou a execução".to_string())
                    } else if stop.as_deref() == Some("length") {
                        Some("resposta truncada no limite de saída".to_string())
                    } else if !self.pi_settled {
                        Some("pi terminou sem evento 'agent_settled'".to_string())
                    } else {
                        None
                    };
                }
            }
        }
        AdapterResult {
            usage: self.usage,
            error,
        }
    }

    pub fn finish_interrupted(mut self) -> StreamState {
        self.apply_usage();
        let error = self.explicit_error();
        let running_tools = match self.provider {
            Provider::Opencode => None,
            _ => Some(
                self.open_tools
                    .iter()
                    .map(|(_, name)| name.clone())
                    .collect(),
            ),
        };
        StreamState {
            last_event: self.last_event.take(),
            running_tools,
            usage: self.usage,
            error,
        }
    }
}

pub fn build_prompt(role: &str, stage: &Stage, available_reads: &[String]) -> String {
    let mut out = String::new();
    let trimmed_role = role.trim();
    if !trimmed_role.is_empty() {
        out.push_str(trimmed_role);
        out.push_str("\n\n");
    }

    let _ = writeln!(out, "## Contexto da etapa");
    let _ = writeln!(out, "- Etapa: `{}`", stage.id);
    if stage.edits {
        let _ = writeln!(
            out,
            "- Permissão no repositório: pode editar os arquivos do workspace."
        );
    } else {
        let _ = writeln!(
            out,
            "- Permissão no repositório: somente leitura (`edits = false`); não altere arquivos fora de `.tchelvis-loop/out/`."
        );
    }

    if !available_reads.is_empty() {
        let _ = writeln!(out, "\n## Entradas disponíveis em `.tchelvis-loop/in/`");
        for name in available_reads {
            let _ = writeln!(out, "- `.tchelvis-loop/in/{name}`");
        }
    }

    if let Some(output_name) = &stage.writes {
        let _ = writeln!(out, "\n## Saída obrigatória");
        let _ = writeln!(
            out,
            "Grave o artefato desta etapa em `.tchelvis-loop/out/{output_name}`."
        );
        if stage.verdict_gate {
            let _ = writeln!(
                out,
                "Encerre `.tchelvis-loop/out/{output_name}` com uma linha única de resumo (até 140 caracteres) e, na última linha não vazia, escreva exatamente `VERDICT: PASS` ou `VERDICT: RETRY`."
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/loop")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()))
    }

    fn parse_fixture(provider: Provider, name: &str) -> AdapterResult {
        let raw = fixture(name);
        let mut parser = EventParser::new(provider);
        for line in raw.lines() {
            parser.line(line);
        }
        parser.finish()
    }

    fn os_vec(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn argv_is_exact() {
        let claude = command(Provider::Claude, None, "hello prompt").unwrap();
        assert_eq!(
            claude.argv,
            os_vec(&[
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--setting-sources",
                "project",
                "--permission-mode",
                "bypassPermissions",
            ])
        );
        assert_eq!(claude.stdin, b"hello prompt");

        let claude_model = command(Provider::Claude, Some("claude-haiku-5-5"), "p").unwrap();
        assert_eq!(
            claude_model.argv,
            os_vec(&[
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--setting-sources",
                "project",
                "--permission-mode",
                "bypassPermissions",
                "--model",
                "claude-haiku-5-5",
            ])
        );

        let agy = command(Provider::Agy, None, "agy prompt").unwrap();
        assert_eq!(
            agy.argv,
            os_vec(&[
                "agy",
                "-p",
                "agy prompt",
                "--output-format",
                "stream-json",
                "--print-timeout",
                "0",
                "--dangerously-skip-permissions",
            ])
        );
        assert!(agy.stdin.is_empty());

        let agy_model = command(Provider::Agy, Some("gemini-4-hydrogen"), "agy prompt").unwrap();
        assert_eq!(
            agy_model.argv,
            os_vec(&[
                "agy",
                "-p",
                "agy prompt",
                "--output-format",
                "stream-json",
                "--print-timeout",
                "0",
                "--dangerously-skip-permissions",
                "--model",
                "gemini-4-hydrogen",
            ])
        );

        let exact_100k = "x".repeat(MAX_AGY_PROMPT_BYTES);
        assert!(command(Provider::Agy, None, &exact_100k).is_ok());
        let too_big = "x".repeat(MAX_AGY_PROMPT_BYTES + 1);
        assert!(command(Provider::Agy, None, &too_big).is_err());

        let opencode = command(Provider::Opencode, None, "oc prompt").unwrap();
        assert_eq!(
            opencode.argv,
            os_vec(&[
                "opencode",
                "run",
                "--standalone",
                "--format",
                "json",
                "--auto"
            ])
        );
        assert_eq!(opencode.stdin, b"oc prompt");

        let opencode_model =
            command(Provider::Opencode, Some("zai/glm-5.3-flash"), "oc prompt").unwrap();
        assert_eq!(
            opencode_model.argv,
            os_vec(&[
                "opencode",
                "run",
                "--standalone",
                "--format",
                "json",
                "--auto",
                "-m",
                "zai/glm-5.3-flash",
            ])
        );

        let pi = command(
            Provider::Pi,
            Some("opencode-go/glm-5.3:high"),
            "hello prompt",
        )
        .unwrap();
        assert_eq!(
            pi.argv,
            os_vec(&[
                "pi",
                "-p",
                "--mode",
                "json",
                "--no-session",
                "--offline",
                "--no-approve",
                "--no-extensions",
                "--no-skills",
                "--no-mcp",
                "--no-prompt-templates",
                "--no-themes",
                "--model",
                "opencode-go/glm-5.3:high",
            ])
        );
        assert_eq!(pi.stdin, b"hello prompt");
        assert!(
            command(Provider::Pi, None, "x").is_err(),
            "pi must refuse a stage without a model"
        );
    }

    #[test]
    fn ok_fixtures() {
        let claude = parse_fixture(Provider::Claude, "claude_ok.jsonl");
        assert_eq!(claude.error, None);
        assert_eq!(claude.usage.turns, Some(2));
        assert_eq!(claude.usage.input_tokens, Some(4));
        assert_eq!(claude.usage.output_tokens, Some(129));
        assert_eq!(claude.usage.cache_read_tokens, Some(7000));
        assert!((claude.usage.cost_usd.unwrap() - 0.0010269).abs() < 1e-12);

        let agy = parse_fixture(Provider::Agy, "agy_ok.jsonl");
        assert_eq!(agy.error, None);
        assert_eq!(agy.usage.turns, Some(2));
        assert_eq!(agy.usage.input_tokens, Some(16537));
        assert_eq!(agy.usage.output_tokens, Some(296));
        assert_eq!(agy.usage.cache_read_tokens, Some(12092));
        assert_eq!(agy.usage.cost_usd, None);

        let opencode = parse_fixture(Provider::Opencode, "opencode_ok.jsonl");
        assert_eq!(opencode.error, None);
        assert_eq!(opencode.usage.turns, Some(2));
        assert_eq!(opencode.usage.input_tokens, Some(14995));
        assert_eq!(opencode.usage.output_tokens, Some(99));
        assert_eq!(opencode.usage.cache_read_tokens, Some(15616));
        assert_eq!(opencode.usage.cost_usd, Some(0.0));

        let pi = parse_fixture(Provider::Pi, "pi_ok.jsonl");
        assert_eq!(pi.error, None);
        assert_eq!(pi.usage.turns, Some(1));
        assert_eq!(pi.usage.input_tokens, Some(1493));
        assert_eq!(pi.usage.output_tokens, Some(39));
        assert_eq!(pi.usage.cache_read_tokens, Some(0));
        assert_eq!(pi.usage.cost_usd, Some(0.0));
    }

    #[test]
    fn bad_model_fixtures_report_an_error() {
        let claude = parse_fixture(Provider::Claude, "claude_bad_model.jsonl");
        assert!(
            claude.error.is_some(),
            "expected error for claude_bad_model"
        );

        let agy = parse_fixture(Provider::Agy, "agy_bad_model.jsonl");
        assert!(agy.error.is_some(), "expected error for agy_bad_model");

        let opencode = parse_fixture(Provider::Opencode, "opencode_bad_model.jsonl");
        assert!(
            opencode.error.is_some(),
            "expected error for opencode_bad_model"
        );

        let pi = parse_fixture(Provider::Pi, "pi_bad_model.jsonl");
        let pi_err = pi.error.unwrap_or_default();
        assert!(
            pi_err.contains("isn't included in OpenCode Go"),
            "unexpected pi error: {pi_err}"
        );
        assert!(!pi_err.contains('\n') && !pi_err.contains('\r'));
        assert_eq!(pi.usage.turns, None);

        let mut claude_no_result = EventParser::new(Provider::Claude);
        claude_no_result.line(r#"{"type":"system","subtype":"init"}"#);
        assert!(claude_no_result.finish().error.is_some());

        let mut agy_no_result = EventParser::new(Provider::Agy);
        agy_no_result.line(r#"{"event":"init"}"#);
        assert!(agy_no_result.finish().error.is_some());
    }

    #[test]
    fn garbage_missing_or_renamed_fields_become_null_without_failing() {
        // Claude with garbage lines and renamed usage fields, but valid non-error result
        let mut claude = EventParser::new(Provider::Claude);
        claude.line("not json at all");
        claude.line("");
        claude.line(r#"{"type":"result","is_error":false,"turns_renamed":3,"usage":{"in":10}}"#);
        let res = claude.finish();
        assert_eq!(res.error, None);
        assert_eq!(res.usage, Usage::default());

        // Agy with garbage lines and missing usage, valid SUCCESS result
        let mut agy = EventParser::new(Provider::Agy);
        agy.line("{broken json");
        agy.line(r#"{"event":"step_update","step_update":{"state":"DONE","step_type":"other"}}"#);
        agy.line(r#"{"event":"result","result":{"status":"SUCCESS","usage":{"in_renamed":5}}}"#);
        let res = agy.finish();
        assert_eq!(res.error, None);
        assert_eq!(res.usage, Usage::default());

        // OpenCode with garbage lines and renamed fields inside step_finish
        let mut opencode = EventParser::new(Provider::Opencode);
        opencode.line("non-json banner");
        opencode.line(r#"{"type":"step_finish","part":{"tokens_renamed":{"input":10}}}"#);
        let res = opencode.finish();
        assert_eq!(res.error, None);
        assert_eq!(res.usage.turns, Some(1));
        assert_eq!(res.usage.input_tokens, None);
        assert_eq!(res.usage.output_tokens, None);
        assert_eq!(res.usage.cache_read_tokens, None);
        assert_eq!(res.usage.cost_usd, None);

        // Pi with garbage lines and renamed usage fields, settled without errors
        let mut pi = EventParser::new(Provider::Pi);
        pi.line("non-json banner");
        pi.line(
            r#"{"type":"message_end","message":{"role":"assistant","usage_renamed":{"input":10}}}"#,
        );
        pi.line(
            r#"{"type":"turn_end","message":{"role":"assistant","stop_reason_renamed":"stop"}}"#,
        );
        pi.line(r#"{"type":"agent_settled","aborted":false}"#);
        let res = pi.finish();
        assert_eq!(res.error, None);
        assert_eq!(res.usage.turns, None);
        assert_eq!(res.usage.input_tokens, None);
        assert_eq!(res.usage.output_tokens, None);
        assert_eq!(res.usage.cache_read_tokens, None);
        assert_eq!(res.usage.cost_usd, None);
    }

    #[test]
    fn pi_error_rules() {
        fn parse(lines: &[serde_json::Value]) -> AdapterResult {
            let mut parser = EventParser::new(Provider::Pi);
            for val in lines {
                parser.line(&val.to_string());
            }
            parser.finish()
        }

        let aborted = parse(&[serde_json::json!({
            "type": "message_end",
            "message": {"role": "assistant", "stopReason": "aborted", "errorMessage": "pedido cancelado"}
        })]);
        assert!(aborted
            .error
            .unwrap_or_default()
            .contains("pedido cancelado"));

        let truncated = parse(&[serde_json::json!({
            "type": "message_end",
            "message": {"role": "assistant", "stopReason": "length"}
        })]);
        assert!(truncated.error.unwrap_or_default().contains("truncada"));

        let settled_aborted =
            parse(&[serde_json::json!({"type": "agent_settled", "aborted": true})]);
        assert!(settled_aborted
            .error
            .unwrap_or_default()
            .contains("abortou"));

        let never_settled = parse(&[serde_json::json!({
            "type": "message_end",
            "message": {"role": "assistant", "stopReason": "stop"}
        })]);
        assert_eq!(
            never_settled.error,
            Some("pi terminou sem evento 'agent_settled'".to_string())
        );

        // A retried turn that then succeeds is not an error.
        let retried_then_ok = parse(&[
            serde_json::json!({
                "type": "message_end",
                "message": {"role": "assistant", "stopReason": "error", "errorMessage": "tentativa 1"}
            }),
            serde_json::json!({"type": "turn_end", "message": {"role": "assistant", "stopReason": "error"}}),
            serde_json::json!({
                "type": "message_end",
                "message": {"role": "assistant", "stopReason": "stop"}
            }),
            serde_json::json!({"type": "turn_end", "message": {"role": "assistant", "stopReason": "stop"}}),
            serde_json::json!({"type": "agent_settled", "aborted": false}),
        ]);
        assert_eq!(retried_then_ok.error, None);
        assert_eq!(retried_then_ok.usage.turns, Some(1));
    }

    #[test]
    fn pi_usage_null_rules() {
        let mut parser = EventParser::new(Provider::Pi);
        parser.line(
            &serde_json::json!({
                "type": "message_end",
                "message": {"role": "assistant", "stopReason": "stop", "usage": {"input": 10, "output": 2, "cacheRead": 3, "cost": {"total": 0.5, "totalFromProvider": false}}}
            })
            .to_string(),
        );
        parser.line(
            &serde_json::json!({
                "type": "message_end",
                "message": {"role": "assistant", "stopReason": "stop"}
            })
            .to_string(),
        );
        parser.line(
            &serde_json::json!({"type": "turn_end", "message": {"role": "assistant", "stopReason": "stop"}})
                .to_string(),
        );
        parser.line(
            &serde_json::json!({"type": "turn_end", "message": {"role": "assistant", "stopReason": "stop"}})
                .to_string(),
        );
        parser.line(&serde_json::json!({"type": "agent_settled", "aborted": false}).to_string());
        let res = parser.finish();
        assert_eq!(res.error, None);
        assert_eq!(res.usage.turns, Some(2));
        assert_eq!(res.usage.input_tokens, None);
        assert_eq!(res.usage.output_tokens, None);
        assert_eq!(res.usage.cache_read_tokens, None);
        assert_eq!(res.usage.cost_usd, None);

        let mut costed = EventParser::new(Provider::Pi);
        costed.line(
            &serde_json::json!({
                "type": "message_end",
                "message": {"role": "assistant", "stopReason": "stop", "usage": {"input": 7, "output": 1, "cacheRead": 0, "cost": {"total": 0.25, "totalFromProvider": false}}}
            })
            .to_string(),
        );
        costed.line(
            &serde_json::json!({ "type": "turn_end", "message": {"role": "assistant", "stopReason": "stop"} })
                .to_string(),
        );
        costed.line(&serde_json::json!({"type": "agent_settled", "aborted": false}).to_string());
        let res = costed.finish();
        assert_eq!(res.usage.turns, Some(1));
        assert_eq!(res.usage.input_tokens, Some(7));
        assert_eq!(res.usage.output_tokens, Some(1));
        assert_eq!(res.usage.cache_read_tokens, Some(0));
        assert_eq!(res.usage.cost_usd, Some(0.25));
    }

    #[test]
    fn error_text_is_redacted_and_single_line() {
        let mut claude = EventParser::new(Provider::Claude);
        claude.line(
            r#"{"type":"result","is_error":true,"result":"Falhou\nAuthorization: Bearer super_secret_tok\n\u001b[31mghp_1234567890abcdef1234\u001b[0m"}"#,
        );
        let err = claude.finish().error.unwrap();
        assert!(!err.contains('\n') && !err.contains('\r'));
        assert!(!err.contains('\x1b'));
        assert!(!err.contains("super_secret_tok"));
        assert!(!err.contains("ghp_1234567890abcdef1234"));
        assert!(err.contains("[REDACTED]"));

        let mut agy = EventParser::new(Provider::Agy);
        agy.line(
            r#"{"event":"result","result":{"status":"ERROR","error":"line1\nAuthorization: Basic dXNlcjpwYXNz\nline2"}}"#,
        );
        let err = agy.finish().error.unwrap();
        assert!(!err.contains('\n'));
        assert!(!err.contains("dXNlcjpwYXNz"));
        assert!(err.contains("[REDACTED]"));

        let mut opencode = EventParser::new(Provider::Opencode);
        opencode.line(
            r#"{"type":"error","error":{"message":"bad\r\n\u001b]0;evil\u0007token glpat-abcdefghijklmn"}}"#,
        );
        let err = opencode.finish().error.unwrap();
        assert!(!err.contains('\n') && !err.contains('\r'));
        assert!(!err.contains('\x1b'));
        assert!(!err.contains("glpat-abcdefghijklmn"));
        assert!(err.contains("[REDACTED]"));

        let mut pi = EventParser::new(Provider::Pi);
        pi.line(
            &serde_json::json!({
                "type": "auto_retry_end",
                "success": false,
                "attempt": 3,
                "finalError": "Authorization: Bearer sk-abcdef0123456789\nsegunda linha"
            })
            .to_string(),
        );
        let err = pi.finish().error.unwrap();
        assert!(!err.contains('\n') && !err.contains('\r'));
        assert!(!err.contains("sk-abcdef0123456789"));
        assert!(err.contains("[REDACTED]"));
    }

    #[test]
    fn build_prompt_omits_missing_reads_and_includes_output_contract() {
        let stage = Stage {
            id: "audit".into(),
            role: "critic".into(),
            provider: Provider::Claude,
            model: None,
            reads: vec!["input.md".into(), "review.md".into(), "current.diff".into()],
            writes: Some("critic.md".into()),
            edits: false,
            next: None,
            verdict_gate: true,
            on_pass: Some("done".into()),
            on_retry: Some("implement".into()),
        };
        let prompt = build_prompt(
            "Voce e o critic.",
            &stage,
            &["input.md".into(), "current.diff".into()],
        );
        assert!(prompt.contains("Voce e o critic."));
        assert!(prompt.contains(".tchelvis-loop/in/input.md"));
        assert!(prompt.contains(".tchelvis-loop/in/current.diff"));
        assert!(!prompt.contains("review.md"));
        assert!(prompt.contains(".tchelvis-loop/out/critic.md"));
        assert!(prompt.contains("VERDICT: PASS"));
        assert!(prompt.contains("VERDICT: RETRY"));
    }

    fn interrupted_after(provider: Provider, name: &str, lines: usize) -> StreamState {
        let raw = fixture(name);
        let mut parser = EventParser::new(provider);
        for line in raw.lines().take(lines) {
            parser.line(line);
        }
        parser.finish_interrupted()
    }

    #[test]
    fn interrupted_pi_reports_the_open_tool() {
        let mut parser = EventParser::new(Provider::Pi);
        for val in [
            serde_json::json!({
                "type": "tool_execution_start",
                "toolCallId": "c1",
                "toolName": "bash"
            }),
            serde_json::json!({
                "type": "tool_execution_update",
                "toolCallId": "c1",
                "toolName": "bash"
            }),
        ] {
            parser.line(&val.to_string());
        }
        let state = parser.finish_interrupted();
        assert_eq!(
            state.last_event.as_deref(),
            Some("tool_execution_update (bash)")
        );
        assert_eq!(state.running_tools, Some(vec!["bash".to_string()]));
        assert_eq!(state.error, None);

        let mut parser = EventParser::new(Provider::Pi);
        for (kind, id) in [
            ("tool_execution_start", "c1"),
            ("tool_execution_update", "c1"),
            ("tool_execution_end", "c1"),
        ] {
            parser.line(
                &serde_json::json!({"type": kind, "toolCallId": id, "toolName": "bash"})
                    .to_string(),
            );
        }
        let state = parser.finish_interrupted();
        assert_eq!(state.running_tools, Some(Vec::new()));
        assert_eq!(
            state.last_event.as_deref(),
            Some("tool_execution_end (bash)")
        );
    }

    fn view_after(provider: Provider, name: &str, lines: usize) -> ToolView {
        let raw = fixture(name);
        let mut parser = EventParser::new(provider);
        for line in raw.lines().take(lines) {
            parser.line(line);
        }
        parser.tool_view()
    }

    #[test]
    fn tool_view_follows_open_tools() {
        assert_eq!(
            view_after(Provider::Claude, "claude_ok.jsonl", 1),
            ToolView::NoneOpen
        );
        assert_eq!(
            view_after(Provider::Claude, "claude_ok.jsonl", 2),
            ToolView::Open(vec!["Read".to_string()])
        );
        assert_eq!(
            view_after(Provider::Claude, "claude_ok.jsonl", 3),
            ToolView::NoneOpen
        );
        assert_eq!(
            view_after(Provider::Agy, "agy_ok.jsonl", 4),
            ToolView::Open(vec!["read_file".to_string()])
        );
        assert_eq!(
            view_after(Provider::Agy, "agy_ok.jsonl", 5),
            ToolView::NoneOpen
        );

        let raw = fixture("claude_ok.jsonl");
        let mut parser = EventParser::new(Provider::Claude);
        for line in raw.lines().take(2) {
            parser.line(line);
        }
        let first = parser.tool_view();
        assert_eq!(first, parser.tool_view(), "the view is stable");
        assert_eq!(
            parser.finish_interrupted().running_tools,
            Some(vec!["Read".to_string()])
        );
    }

    #[test]
    fn tool_view_keeps_parallel_tools_with_one_name() {
        let mut parser = EventParser::new(Provider::Pi);
        for line in [
            r#"{"type":"tool_execution_start","toolCallId":"a","toolName":"bash"}"#,
            r#"{"type":"tool_execution_start","toolCallId":"b","toolName":"bash"}"#,
        ] {
            parser.line(line);
        }
        assert_eq!(
            parser.tool_view(),
            ToolView::Open(vec!["bash".to_string(), "bash".to_string()])
        );
        parser.line(r#"{"type":"tool_execution_end","toolCallId":"a"}"#);
        assert_eq!(parser.tool_view(), ToolView::Open(vec!["bash".to_string()]));
        parser.line(r#"{"type":"tool_execution_end","toolCallId":"b"}"#);
        assert_eq!(parser.tool_view(), ToolView::NoneOpen);
    }

    #[test]
    fn tool_view_ignores_bad_lines() {
        let mut pi = EventParser::new(Provider::Pi);
        for line in [
            "not json",
            r#"{"type":"tool_execution_start","toolName":"bash"}"#,
            r#"{"type":"tool_execution_start","toolCallId":"x","name":"bash"}"#,
        ] {
            pi.line(line);
            assert_eq!(pi.tool_view(), ToolView::NoneOpen, "{line}");
        }
        pi.line(r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash"}"#);
        pi.line(r#"{"type":"tool_execution_end","toolCallId":"c1""#);
        assert_eq!(pi.tool_view(), ToolView::Open(vec!["bash".to_string()]));

        let mut claude = EventParser::new(Provider::Claude);
        claude.line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
        );
        assert_eq!(claude.tool_view(), ToolView::NoneOpen);

        let mut agy = EventParser::new(Provider::Agy);
        agy.line(
            r#"{"event":"step_update","step_update":{"state":"ACTIVE","step_type":"tool","tool_name":"shell"}}"#,
        );
        assert_eq!(agy.tool_view(), ToolView::NoneOpen);
    }

    #[test]
    fn tool_view_counts_opencode_boundaries() {
        let expected = [0, 1, 1, 2, 3, 3, 4];
        for (lines, step) in expected.into_iter().enumerate() {
            assert_eq!(
                view_after(Provider::Opencode, "opencode_ok.jsonl", lines),
                ToolView::Unseen { step },
                "after {lines} lines"
            );
        }
    }

    #[test]
    fn interrupted_claude_and_agy_track_open_tools() {
        let claude_open = interrupted_after(Provider::Claude, "claude_ok.jsonl", 2);
        assert_eq!(claude_open.running_tools, Some(vec!["Read".to_string()]));
        assert_eq!(claude_open.last_event.as_deref(), Some("assistant (Read)"));
        let claude_closed = interrupted_after(Provider::Claude, "claude_ok.jsonl", 3);
        assert_eq!(claude_closed.running_tools, Some(Vec::new()));

        let agy_open = interrupted_after(Provider::Agy, "agy_ok.jsonl", 4);
        assert_eq!(agy_open.running_tools, Some(vec!["read_file".to_string()]));
        assert_eq!(
            agy_open.last_event.as_deref(),
            Some("step_update tool (read_file)")
        );
        let agy_closed = interrupted_after(Provider::Agy, "agy_ok.jsonl", 5);
        assert_eq!(agy_closed.running_tools, Some(Vec::new()));
    }

    #[test]
    fn interrupted_opencode_does_not_know_tools() {
        let started = interrupted_after(Provider::Opencode, "opencode_ok.jsonl", 1);
        assert_eq!(started.last_event.as_deref(), Some("step_start"));
        assert_eq!(started.running_tools, None);

        let tool = interrupted_after(Provider::Opencode, "opencode_ok.jsonl", 2);
        assert_eq!(tool.last_event.as_deref(), Some("tool_use (read)"));
        assert_eq!(tool.running_tools, None);
    }

    #[test]
    fn parallel_tools_with_one_name_close_by_id() {
        let mut parser = EventParser::new(Provider::Pi);
        for (kind, id) in [("tool_execution_start", "a"), ("tool_execution_start", "b")] {
            parser.line(
                &serde_json::json!({"type": kind, "toolCallId": id, "toolName": "bash"})
                    .to_string(),
            );
        }
        assert_eq!(
            parser.finish_interrupted().running_tools,
            Some(vec!["bash".to_string(), "bash".to_string()])
        );

        let mut parser = EventParser::new(Provider::Pi);
        for (kind, id) in [
            ("tool_execution_start", "a"),
            ("tool_execution_start", "b"),
            ("tool_execution_end", "a"),
        ] {
            parser.line(
                &serde_json::json!({"type": kind, "toolCallId": id, "toolName": "bash"})
                    .to_string(),
            );
        }
        assert_eq!(
            parser.finish_interrupted().running_tools,
            Some(vec!["bash".to_string()])
        );

        let mut parser = EventParser::new(Provider::Pi);
        for (kind, id) in [("tool_execution_start", "a"), ("tool_execution_end", "z")] {
            parser.line(
                &serde_json::json!({"type": kind, "toolCallId": id, "toolName": "bash"})
                    .to_string(),
            );
        }
        assert_eq!(
            parser.finish_interrupted().running_tools,
            Some(vec!["bash".to_string()])
        );

        let mut parser = EventParser::new(Provider::Pi);
        parser.line(
            &serde_json::json!({"type": "tool_execution_start", "toolName": "bash"}).to_string(),
        );
        assert_eq!(parser.finish_interrupted().running_tools, Some(Vec::new()));
    }

    #[test]
    fn interrupted_keeps_explicit_errors() {
        let mut parser = EventParser::new(Provider::Pi);
        parser.line(
            &serde_json::json!({
                "type": "auto_retry_end",
                "success": false,
                "finalError": "x"
            })
            .to_string(),
        );
        let state = parser.finish_interrupted();
        assert_eq!(state.error, Some("x".to_string()));
    }
}
