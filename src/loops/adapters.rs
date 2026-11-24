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

pub struct EventParser {
    provider: Provider,
    saw_result: bool,
    usage: Usage,
    error: Option<String>,
    agy_turns: u64,
    opencode_steps: u64,
    opencode_in_sum: u64,
    opencode_in_seen: u64,
    opencode_out_sum: u64,
    opencode_out_seen: u64,
    opencode_cache_sum: u64,
    opencode_cache_seen: u64,
    opencode_cost_sum: f64,
    opencode_cost_seen: u64,
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
            opencode_in_sum: 0,
            opencode_in_seen: 0,
            opencode_out_sum: 0,
            opencode_out_seen: 0,
            opencode_cache_sum: 0,
            opencode_cache_seen: 0,
            opencode_cost_sum: 0.0,
            opencode_cost_seen: 0,
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

        match self.provider {
            Provider::Claude => self.handle_claude(&val),
            Provider::Agy => self.handle_agy(&val),
            Provider::Opencode => self.handle_opencode(&val),
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

    pub fn finish(mut self) -> AdapterResult {
        match self.provider {
            Provider::Claude => {
                if !self.saw_result && self.error.is_none() {
                    self.error = Some("claude terminou sem evento 'result'".to_string());
                }
            }
            Provider::Agy => {
                self.usage.turns = (self.agy_turns > 0).then_some(self.agy_turns);
                if !self.saw_result && self.error.is_none() {
                    self.error = Some("agy terminou sem evento 'result'".to_string());
                }
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
        }
        AdapterResult {
            usage: self.usage,
            error: self.error,
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
}
