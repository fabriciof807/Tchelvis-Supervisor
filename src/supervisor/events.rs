use super::ollama::SupervisorDecision;
use super::state::Urgency;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::fs;
use std::path::{Path, PathBuf};

/// Maximum size in bytes for `supervisor_events.jsonl` before rotating to `.1` (2 MiB).
pub const MAX_SUPERVISOR_EVENTS_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_EVENT_SUMMARY_CHARS: usize = 200;
pub const MAX_EVENT_DETAIL_CHARS: usize = 500;

/// Stable `snake_case` event kinds for the durable supervisor activity log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEventKind {
    AutoReply,
    AutoReplyBlocked,
    Alert,
    Evaluation,
    EvalError,
    ModeChange,
    LlmLoad,
    LlmUnload,
    GpuToggle,
    Respond,
}

impl SupervisorEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutoReply => "auto_reply",
            Self::AutoReplyBlocked => "auto_reply_blocked",
            Self::Alert => "alert",
            Self::Evaluation => "evaluation",
            Self::EvalError => "eval_error",
            Self::ModeChange => "mode_change",
            Self::LlmLoad => "llm_load",
            Self::LlmUnload => "llm_unload",
            Self::GpuToggle => "gpu_toggle",
            Self::Respond => "respond",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto_reply" => Ok(Self::AutoReply),
            "auto_reply_blocked" => Ok(Self::AutoReplyBlocked),
            "alert" => Ok(Self::Alert),
            "evaluation" => Ok(Self::Evaluation),
            "eval_error" => Ok(Self::EvalError),
            "mode_change" => Ok(Self::ModeChange),
            "llm_load" => Ok(Self::LlmLoad),
            "llm_unload" => Ok(Self::LlmUnload),
            "gpu_toggle" => Ok(Self::GpuToggle),
            "respond" => Ok(Self::Respond),
            other => anyhow::bail!(
                "Invalid event kind '{other}'; expected one of: auto_reply, auto_reply_blocked, alert, evaluation, eval_error, mode_change, llm_load, llm_unload, gpu_toggle, respond"
            ),
        }
    }
}

/// Durable supervisor activity event written to `supervisor_events.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorEvent {
    pub ts_ms: u64,
    pub kind: SupervisorEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<Urgency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
}

fn cap_chars(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = input.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        let trimmed: String = head.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{trimmed}…")
    } else {
        head
    }
}

/// Redacts secrets, collapses newlines/whitespace to a single line, and caps length.
pub fn sanitize_summary(raw: &str) -> String {
    let redacted = crate::logging::redact_secrets(raw);
    let one_line = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    cap_chars(&one_line, MAX_EVENT_SUMMARY_CHARS)
}

/// Redacts secrets, trims, and caps optional detail text. Never emits empty strings.
pub fn sanitize_detail(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let redacted = crate::logging::redact_secrets(raw);
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(cap_chars(trimmed, MAX_EVENT_DETAIL_CHARS))
}

fn opt_non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|v| !v.is_empty())
        .map(crate::logging::redact_secrets)
}

fn empty_base(ts_ms: u64, kind: SupervisorEventKind, summary: String) -> SupervisorEvent {
    SupervisorEvent {
        ts_ms,
        kind,
        agent_id: None,
        session_name: None,
        summary,
        detail: None,
        source: None,
        key: None,
        model: None,
        urgency: None,
        decision: None,
        latency_ms: None,
        from: None,
        to: None,
        action: None,
        rule_id: None,
    }
}

impl SupervisorEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn auto_reply(
        ts_ms: u64,
        agent_id: &str,
        session_name: &str,
        source: &str,
        key: &str,
        model: Option<&str>,
        eval_summary: &str,
        rule_id: Option<&str>,
    ) -> Self {
        let key_disp = if key.is_empty() { "Enter" } else { key };
        let origin = if source == "fixed_rule" {
            "regra fixa".to_string()
        } else if let Some(m) = opt_non_empty(model) {
            format!("LLM · {m}")
        } else {
            "LLM".to_string()
        };
        let summary_tail = sanitize_summary(eval_summary);
        let summary = if summary_tail.is_empty() {
            sanitize_summary(&format!("Auto-resposta ({origin}) enviou '{key_disp}'"))
        } else {
            sanitize_summary(&format!(
                "Auto-resposta ({origin}) '{key_disp}': {summary_tail}"
            ))
        };
        let mut ev = empty_base(ts_ms, SupervisorEventKind::AutoReply, summary);
        ev.agent_id = opt_non_empty(Some(agent_id));
        ev.session_name = opt_non_empty(Some(session_name));
        ev.source = Some(source.to_string());
        ev.key = Some(key.to_string());
        ev.model = opt_non_empty(model);
        ev.detail = sanitize_detail(Some(eval_summary));
        ev.rule_id = opt_non_empty(rule_id);
        ev
    }

    pub fn auto_reply_blocked(
        ts_ms: u64,
        agent_id: &str,
        session_name: &str,
        reason: &str,
        detail: Option<&str>,
        rule_id: Option<&str>,
    ) -> Self {
        let clean_reason = sanitize_summary(reason);
        let summary = if clean_reason.is_empty() {
            "Auto-resposta bloqueada pelos guardrails".to_string()
        } else {
            sanitize_summary(&format!("Auto-resposta bloqueada: {clean_reason}"))
        };
        let mut ev = empty_base(ts_ms, SupervisorEventKind::AutoReplyBlocked, summary);
        ev.agent_id = opt_non_empty(Some(agent_id));
        ev.session_name = opt_non_empty(Some(session_name));
        ev.detail = sanitize_detail(detail);
        ev.rule_id = opt_non_empty(rule_id);
        ev
    }

    pub fn alert(
        ts_ms: u64,
        agent_id: &str,
        session_name: &str,
        urgency: Urgency,
        reason: &str,
        updated_summary: &str,
    ) -> Self {
        let urgency_pt = match urgency {
            Urgency::Critical => "crítico",
            Urgency::Normal => "normal",
            Urgency::Low => "baixo",
        };
        let short_reason = if !reason.trim().is_empty() {
            sanitize_summary(reason)
        } else {
            sanitize_summary(updated_summary)
        };
        let summary = if short_reason.is_empty() {
            format!("Alerta ({urgency_pt}) para operador")
        } else {
            sanitize_summary(&format!("Alerta ({urgency_pt}): {short_reason}"))
        };
        let mut ev = empty_base(ts_ms, SupervisorEventKind::Alert, summary);
        ev.agent_id = opt_non_empty(Some(agent_id));
        ev.session_name = opt_non_empty(Some(session_name));
        ev.urgency = Some(urgency);
        let full_detail = if !reason.trim().is_empty() && !updated_summary.trim().is_empty() {
            if reason.trim() == updated_summary.trim() {
                reason.to_string()
            } else {
                format!("{reason}\nResumo: {updated_summary}")
            }
        } else if !reason.trim().is_empty() {
            reason.to_string()
        } else {
            updated_summary.to_string()
        };
        ev.detail = sanitize_detail(Some(&full_detail));
        ev
    }

    #[allow(clippy::too_many_arguments)]
    pub fn evaluation(
        ts_ms: u64,
        agent_id: &str,
        session_name: &str,
        decision: SupervisorDecision,
        raw_llm_decision: SupervisorDecision,
        latency_ms: u64,
        model: Option<&str>,
        updated_summary: &str,
    ) -> Self {
        let model_clean = opt_non_empty(model);
        let model_tag = model_clean
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();
        let summary = sanitize_summary(&format!(
            "Avaliação{model_tag} → {} ({latency_ms} ms)",
            decision.as_str()
        ));
        let detail_text = if raw_llm_decision != decision {
            format!(
                "Decisão LLM: {} → final: {}\n{}",
                raw_llm_decision.as_str(),
                decision.as_str(),
                updated_summary
            )
        } else {
            updated_summary.to_string()
        };
        let mut ev = empty_base(ts_ms, SupervisorEventKind::Evaluation, summary);
        ev.agent_id = opt_non_empty(Some(agent_id));
        ev.session_name = opt_non_empty(Some(session_name));
        ev.decision = Some(decision.as_str().to_string());
        ev.latency_ms = Some(latency_ms);
        ev.model = model_clean;
        ev.detail = sanitize_detail(Some(&detail_text));
        ev
    }

    pub fn eval_error(
        ts_ms: u64,
        agent_id: Option<&str>,
        session_name: Option<&str>,
        model: Option<&str>,
        err_msg: &str,
    ) -> Self {
        let short_err = sanitize_summary(err_msg);
        let summary = if short_err.is_empty() {
            "Erro na avaliação do supervisor".to_string()
        } else {
            sanitize_summary(&format!("Erro: {short_err}"))
        };
        let mut ev = empty_base(ts_ms, SupervisorEventKind::EvalError, summary);
        ev.agent_id = opt_non_empty(agent_id);
        ev.session_name = opt_non_empty(session_name);
        ev.model = opt_non_empty(model);
        ev.detail = sanitize_detail(Some(err_msg));
        ev
    }

    pub fn mode_change(ts_ms: u64, from: &str, to: &str, to_label: &str) -> Self {
        let summary = sanitize_summary(&format!("Modo alterado: {from} → {to} ({to_label})"));
        let mut ev = empty_base(ts_ms, SupervisorEventKind::ModeChange, summary);
        ev.from = Some(from.to_string());
        ev.to = Some(to.to_string());
        ev
    }

    pub fn llm_load(ts_ms: u64, model: Option<&str>, detail: Option<&str>) -> Self {
        let model_clean = opt_non_empty(model);
        let summary = match &model_clean {
            Some(m) => sanitize_summary(&format!("LLM carregado: {m}")),
            None => "LLM carregado".to_string(),
        };
        let mut ev = empty_base(ts_ms, SupervisorEventKind::LlmLoad, summary);
        ev.model = model_clean;
        ev.detail = sanitize_detail(detail);
        ev
    }

    pub fn llm_unload(
        ts_ms: u64,
        model: Option<&str>,
        summary: &str,
        detail: Option<&str>,
    ) -> Self {
        let mut ev = empty_base(
            ts_ms,
            SupervisorEventKind::LlmUnload,
            sanitize_summary(summary),
        );
        ev.model = opt_non_empty(model);
        ev.detail = sanitize_detail(detail);
        ev
    }

    pub fn gpu_toggle(ts_ms: u64, from_gpu: bool, to_gpu: bool, detail: Option<&str>) -> Self {
        let from = if from_gpu { "gpu" } else { "cpu" };
        let to = if to_gpu { "gpu" } else { "cpu" };
        let summary = if to_gpu {
            "GPU ativada (offload total)".to_string()
        } else {
            "GPU desativada (somente CPU)".to_string()
        };
        let mut ev = empty_base(ts_ms, SupervisorEventKind::GpuToggle, summary);
        ev.from = Some(from.to_string());
        ev.to = Some(to.to_string());
        ev.detail = sanitize_detail(detail);
        ev
    }

    pub fn respond(
        ts_ms: u64,
        agent_id: &str,
        session_name: Option<&str>,
        action: &str,
        key: Option<&str>,
        summary: &str,
        detail: Option<&str>,
    ) -> Self {
        let mut ev = empty_base(
            ts_ms,
            SupervisorEventKind::Respond,
            sanitize_summary(summary),
        );
        ev.agent_id = opt_non_empty(Some(agent_id));
        ev.session_name = opt_non_empty(session_name);
        ev.action = opt_non_empty(Some(action));
        ev.key = key.map(ToString::to_string);
        ev.detail = sanitize_detail(detail);
        ev
    }
}

/// Extracts the concise guardrail block reason from `reason_for_operator`
/// (e.g., `"comando fora da lista permitida"`).
pub fn extract_block_reason(reason_for_operator: &str, decision: SupervisorDecision) -> String {
    let prefixes = [
        "[Resposta automática bloqueada: ",
        "[Comando destrutivo detectado no terminal: ",
        "[Payload bloqueado pelo filtro de segurança: ",
    ];
    for prefix in prefixes {
        if let Some(start) = reason_for_operator.find(prefix) {
            let rest = &reason_for_operator[start + prefix.len()..];
            let inner = rest.split(']').next().unwrap_or(rest).trim();
            if !inner.is_empty() {
                if prefix == "[Comando destrutivo detectado no terminal: " {
                    return format!("comando destrutivo detectado ({inner})");
                }
                if prefix == "[Payload bloqueado pelo filtro de segurança: " {
                    return format!("payload bloqueado ({inner})");
                }
                return inner.to_string();
            }
        }
    }
    if reason_for_operator.contains("[Limite de auto-resposta atingido]") {
        return "limite de auto-resposta atingido".to_string();
    }
    if reason_for_operator.contains("[Modo Somente Alerta ativo]") {
        return "modo somente alerta ativo".to_string();
    }
    let cleaned = reason_for_operator
        .replace("[Sugestão da LLM não enviada]", "")
        .trim()
        .to_string();
    if !cleaned.is_empty() {
        return sanitize_summary(&cleaned);
    }
    if decision == SupervisorDecision::NoOp {
        "agente não bloqueado ou payload vazio".to_string()
    } else {
        "bloqueada pelos guardrails".to_string()
    }
}

/// Returns the single rotated backup path (`<path>.1`) for an event log file.
pub fn rotated_events_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".1");
    PathBuf::from(os)
}

#[derive(Debug, Clone, Default)]
pub struct EventLogQuery {
    pub limit: usize,
    pub kinds: Vec<SupervisorEventKind>,
    pub agent_id: Option<String>,
}

fn read_events_file(
    path: &Path,
    seq: &mut usize,
    out: &mut Vec<(usize, SupervisorEvent)>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read events log {:?}", path))?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<SupervisorEvent>(trimmed) {
            out.push((*seq, ev));
            *seq += 1;
        }
    }
    Ok(())
}

/// Reads events from the rotated `.1` file and the active `supervisor_events.jsonl`,
/// returning matches newest-first up to `query.limit`.
pub fn read_events(path: &Path, query: &EventLogQuery) -> Result<Vec<SupervisorEvent>> {
    let mut seq = 0usize;
    let mut raw = Vec::new();
    let rotated = rotated_events_path(path);
    read_events_file(&rotated, &mut seq, &mut raw)?;
    read_events_file(path, &mut seq, &mut raw)?;

    let agent_filter = query
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut filtered: Vec<(usize, SupervisorEvent)> = raw
        .into_iter()
        .filter(|(_, ev)| {
            if !query.kinds.is_empty() && !query.kinds.contains(&ev.kind) {
                return false;
            }
            if let Some(agent) = agent_filter {
                if ev.agent_id.as_deref() != Some(agent) {
                    return false;
                }
            }
            true
        })
        .collect();

    filtered.sort_by_key(|(order, ev)| (Reverse(ev.ts_ms), Reverse(*order)));
    if filtered.len() > query.limit {
        filtered.truncate(query.limit);
    }
    Ok(filtered.into_iter().map(|(_, ev)| ev).collect())
}

/// Formats one event as a concise human-readable line for `tchelvis supervisor log`.
pub fn format_event_line(ev: &SupervisorEvent) -> String {
    let ts_str = DateTime::from_timestamp_millis(ev.ts_ms as i64)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| ev.ts_ms.to_string());
    let agent_part = match (&ev.agent_id, &ev.session_name) {
        (Some(id), Some(name)) if !name.is_empty() && name != id => format!(" {id} ({name})"),
        (Some(id), _) => format!(" {id}"),
        (None, Some(name)) if !name.is_empty() => format!(" {name}"),
        _ => String::new(),
    };
    format!("{ts_str} [{}]{agent_part} {}", ev.kind.as_str(), ev.summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::state::append_jsonl_record;
    use tempfile::tempdir;

    #[test]
    fn all_event_kinds_serialize_with_stable_snake_case() {
        let cases = [
            (
                SupervisorEvent::auto_reply(
                    1000,
                    "t1",
                    "auth",
                    "fixed_rule",
                    "y",
                    None,
                    "read-only",
                    None,
                ),
                "auto_reply",
            ),
            (
                SupervisorEvent::auto_reply_blocked(
                    1001,
                    "t1",
                    "auth",
                    "comando fora da lista permitida",
                    None,
                    None,
                ),
                "auto_reply_blocked",
            ),
            (
                SupervisorEvent::alert(1002, "t1", "auth", Urgency::Critical, "rm -rf", "perigo"),
                "alert",
            ),
            (
                SupervisorEvent::evaluation(
                    1003,
                    "t1",
                    "auth",
                    SupervisorDecision::NoOp,
                    SupervisorDecision::NoOp,
                    245,
                    Some("qwen2.5:1.5b"),
                    "trabalhando",
                ),
                "evaluation",
            ),
            (
                SupervisorEvent::eval_error(1004, Some("t1"), Some("auth"), None, "timeout"),
                "eval_error",
            ),
            (
                SupervisorEvent::mode_change(1005, "SemiAuto", "AlertOnly", "Alert Only"),
                "mode_change",
            ),
            (
                SupervisorEvent::llm_load(1006, Some("lfm3:1.7b"), None),
                "llm_load",
            ),
            (
                SupervisorEvent::llm_unload(1007, None, "LLM descarregado por inatividade", None),
                "llm_unload",
            ),
            (
                SupervisorEvent::gpu_toggle(1008, false, true, Some("warm=true")),
                "gpu_toggle",
            ),
            (
                SupervisorEvent::respond(
                    1009,
                    "t1",
                    Some("auth"),
                    "approve",
                    Some("enter"),
                    "Operador aprovou prompt em t1",
                    None,
                ),
                "respond",
            ),
        ];

        for (ev, expected_kind) in cases {
            let val = serde_json::to_value(&ev).unwrap();
            assert_eq!(val["kind"], expected_kind);
            assert_eq!(SupervisorEventKind::parse(expected_kind).unwrap(), ev.kind);
            let roundtrip: SupervisorEvent = serde_json::from_value(val).unwrap();
            assert_eq!(roundtrip, ev);
        }
    }

    #[test]
    fn event_sanitization_redacts_secrets_and_caps_length() {
        let long_secret = format!(
            "Authorization: Bearer ghp_1234567890abcdef1234 line1\nline2 {}",
            "x".repeat(800)
        );
        let ev = SupervisorEvent::eval_error(1000, Some("t1"), Some("sess"), None, &long_secret);
        assert!(!ev.summary.contains("ghp_1234567890abcdef1234"));
        assert!(!ev.summary.contains('\n'));
        assert!(ev.summary.chars().count() <= MAX_EVENT_SUMMARY_CHARS);
        let detail = ev.detail.as_deref().unwrap();
        assert!(!detail.contains("ghp_1234567890abcdef1234"));
        assert!(detail.contains("[REDACTED]"));
        assert!(detail.chars().count() <= MAX_EVENT_DETAIL_CHARS);
    }

    #[test]
    fn rotation_moves_to_dot_1_and_read_events_returns_newest_first_from_both() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("supervisor_events.jsonl");

        for i in 1..=6u64 {
            let ev = SupervisorEvent::mode_change(
                i * 1000,
                "SemiAuto",
                "AlertOnly",
                &format!("evento-{i}"),
            );
            append_jsonl_record(&log_path, &ev, Some(260)).unwrap();
        }

        let rotated = rotated_events_path(&log_path);
        assert!(rotated.exists(), "rotated .1 file must exist");
        assert!(log_path.exists(), "active log file must exist");

        let all = read_events(
            &log_path,
            &EventLogQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!all.is_empty());
        assert_eq!(all[0].ts_ms, 6000);
        // Must include entries from both rotated and current files in descending order
        assert!(all.windows(2).all(|w| w[0].ts_ms >= w[1].ts_ms));
        assert!(all.len() >= 3);
    }

    #[test]
    fn read_events_filters_by_kind_agent_and_limit() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("supervisor_events.jsonl");

        let events = vec![
            SupervisorEvent::auto_reply(1000, "t1", "api", "fixed_rule", "y", None, "ls", None),
            SupervisorEvent::alert(2000, "t2", "ui", Urgency::Critical, "rm -rf", "perigo"),
            SupervisorEvent::evaluation(
                2000,
                "t1",
                "api",
                SupervisorDecision::NoOp,
                SupervisorDecision::NoOp,
                42,
                Some("qwen"),
                "ok",
            ),
            SupervisorEvent::eval_error(3000, Some("t1"), Some("api"), None, "boom"),
        ];
        for ev in &events {
            append_jsonl_record(&log_path, ev, Some(MAX_SUPERVISOR_EVENTS_BYTES)).unwrap();
        }

        let by_kind = read_events(
            &log_path,
            &EventLogQuery {
                limit: 100,
                kinds: vec![SupervisorEventKind::Alert, SupervisorEventKind::EvalError],
                agent_id: None,
            },
        )
        .unwrap();
        assert_eq!(by_kind.len(), 2);
        assert_eq!(by_kind[0].kind, SupervisorEventKind::EvalError);
        assert_eq!(by_kind[1].kind, SupervisorEventKind::Alert);

        let by_agent = read_events(
            &log_path,
            &EventLogQuery {
                limit: 1,
                kinds: vec![],
                agent_id: Some("t1".to_string()),
            },
        )
        .unwrap();
        assert_eq!(by_agent.len(), 1);
        assert_eq!(by_agent[0].ts_ms, 3000);
    }

    #[test]
    fn extract_block_reason_extracts_guardrail_messages() {
        assert_eq!(
            extract_block_reason(
                "[Sugestão da LLM não enviada] X [Resposta automática bloqueada: comando fora da lista permitida]",
                SupervisorDecision::AlertOperator
            ),
            "comando fora da lista permitida"
        );
        assert_eq!(
            extract_block_reason(
                "[Comando destrutivo detectado no terminal: rm]",
                SupervisorDecision::AlertOperator
            ),
            "comando destrutivo detectado (rm)"
        );
        assert_eq!(
            extract_block_reason(
                "[Modo Somente Alerta ativo]",
                SupervisorDecision::AlertOperator
            ),
            "modo somente alerta ativo"
        );
    }

    #[test]
    fn old_lines_without_rule_id_still_parse() {
        let old = r#"{"ts_ms":1000,"kind":"auto_reply","agent_id":"t1","session_name":"auth","summary":"s","source":"fixed_rule","key":"y"}"#;
        let ev: SupervisorEvent = serde_json::from_str(old).unwrap();
        assert_eq!(ev.rule_id, None);
        let serialized = serde_json::to_string(&ev).unwrap();
        assert!(!serialized.contains("rule_id"));
    }
}
