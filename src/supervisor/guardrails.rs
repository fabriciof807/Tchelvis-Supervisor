use super::ollama::{CognitiveEvaluation, SupervisorDecision};
use super::state::{AgentDurableState, SupervisorMode, Urgency};
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Dangerous command tokens that must never be executed via `AUTO_REPLY`.
pub const FORBIDDEN_TOKENS: &[&str] = &[
    "rm ",
    "sudo",
    "git push",
    "git reset",
    "--force",
    "drop ",
    "chmod",
];

/// Checks case-insensitively whether `payload` contains any token in `FORBIDDEN_TOKENS`.
pub fn contains_forbidden_payload(payload: &str) -> Option<&'static str> {
    let lower = payload.to_lowercase();
    FORBIDDEN_TOKENS
        .iter()
        .copied()
        .find(|token| lower.contains(token))
}

fn error_signature_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"ERR!|panic:|thread '.*' panicked|Traceback \(most recent call last\)|FATAL|Segmentation fault|error\[E",
        )
        .expect("valid error signature regex")
    })
}

fn prompt_signature_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[y/N\]|\[Y/n\]|\(y/n\)|(?i:Continue\?|Press Enter|Proceed\?)")
            .expect("valid prompt signature regex")
    })
}

/// Detects deterministic error, interactive prompt, or repeated-line loop signatures.
pub fn has_error_or_loop_signature(output: &str) -> bool {
    if error_signature_regex().is_match(output) || prompt_signature_regex().is_match(output) {
        return true;
    }

    let lines: Vec<&str> = output.lines().collect();
    let tail = if lines.len() > 25 {
        &lines[lines.len() - 25..]
    } else {
        &lines[..]
    };

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in tail {
        let trimmed = line.trim();
        if trimmed.chars().count() >= 12 && trimmed.chars().any(|c| c.is_alphanumeric()) {
            let count = counts.entry(trimmed).or_insert(0);
            *count += 1;
            if *count >= 3 {
                return true;
            }
        }
    }

    false
}

fn append_reason(reason: &mut String, note: &str) {
    if reason.trim().is_empty() {
        reason.clear();
        reason.push_str(note);
    } else {
        if !reason.ends_with(' ') {
            reason.push(' ');
        }
        reason.push_str(note);
    }
}

/// Applies deterministic guardrails to an LLM evaluation before any `hc` side effect.
pub fn validate_evaluation(
    state: &AgentDurableState,
    mode: SupervisorMode,
    mut eval: CognitiveEvaluation,
) -> CognitiveEvaluation {
    // Rule 1: Loop Breaker
    if eval.decision == SupervisorDecision::AutoReply
        && state.consecutive_auto_replies >= state.max_auto_replies
    {
        eval.decision = SupervisorDecision::AlertOperator;
        append_reason(
            &mut eval.reason_for_operator,
            "[Limite de auto-resposta atingido]",
        );
    }

    // Rule 2: Forbidden Payload Filter
    if eval.decision == SupervisorDecision::AutoReply {
        if let Some(token) = contains_forbidden_payload(&eval.hc_command_payload) {
            eval.decision = SupervisorDecision::AlertOperator;
            eval.urgency = Urgency::Critical;
            append_reason(
                &mut eval.reason_for_operator,
                &format!("[Payload bloqueado pelo filtro de segurança: {token}]"),
            );
        }
    }

    // Rule 3: Empty Payload Guard
    if eval.decision == SupervisorDecision::AutoReply && eval.hc_command_payload.trim().is_empty() {
        eval.decision = SupervisorDecision::NoOp;
    }

    // Rule 4: Supervisor Mode Filter
    if mode == SupervisorMode::AlertOnly && eval.decision == SupervisorDecision::AutoReply {
        eval.decision = SupervisorDecision::AlertOperator;
        append_reason(&mut eval.reason_for_operator, "[Modo Somente Alerta ativo]");
    }

    eval
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::state::LifecycleState;

    fn sample_eval(decision: SupervisorDecision, payload: &str) -> CognitiveEvaluation {
        CognitiveEvaluation {
            updated_summary: "Agente aguardando confirmação.".to_string(),
            detected_state: LifecycleState::WaitingConfirmation,
            decision,
            urgency: Urgency::Normal,
            hc_command_payload: payload.to_string(),
            reason_for_operator: "Confirmação necessária.".to_string(),
        }
    }

    #[test]
    fn test_loop_breaker_downgrades_auto_reply() {
        let mut state = AgentDurableState::new("t1", "worker", 2, 100);
        state.consecutive_auto_replies = 2;

        let validated = validate_evaluation(
            &state,
            SupervisorMode::SemiAuto,
            sample_eval(SupervisorDecision::AutoReply, "y"),
        );

        assert_eq!(validated.decision, SupervisorDecision::AlertOperator);
        assert!(validated
            .reason_for_operator
            .ends_with("[Limite de auto-resposta atingido]"));
    }

    #[test]
    fn test_forbidden_tokens_downgrade_to_critical_alert() {
        let state = AgentDurableState::new("t1", "worker", 2, 100);

        for token in FORBIDDEN_TOKENS {
            let payload = format!("run {} something", token.to_uppercase());
            let validated = validate_evaluation(
                &state,
                SupervisorMode::SemiAuto,
                sample_eval(SupervisorDecision::AutoReply, &payload),
            );
            assert_eq!(
                validated.decision,
                SupervisorDecision::AlertOperator,
                "token {token} should be blocked"
            );
            assert_eq!(validated.urgency, Urgency::Critical);
            assert!(validated.reason_for_operator.contains(&format!(
                "[Payload bloqueado pelo filtro de segurança: {token}]"
            )));
        }
    }

    #[test]
    fn test_empty_payload_downgrades_to_noop() {
        let state = AgentDurableState::new("t1", "worker", 2, 100);
        let validated = validate_evaluation(
            &state,
            SupervisorMode::SemiAuto,
            sample_eval(SupervisorDecision::AutoReply, "   "),
        );
        assert_eq!(validated.decision, SupervisorDecision::NoOp);
    }

    #[test]
    fn test_alert_only_mode_downgrades_auto_reply() {
        let state = AgentDurableState::new("t1", "worker", 2, 100);
        let validated = validate_evaluation(
            &state,
            SupervisorMode::AlertOnly,
            sample_eval(SupervisorDecision::AutoReply, "y"),
        );
        assert_eq!(validated.decision, SupervisorDecision::AlertOperator);
        assert!(validated
            .reason_for_operator
            .contains("[Modo Somente Alerta ativo]"));
    }

    #[test]
    fn test_error_and_loop_signatures() {
        let normal = "Compiling cockpit v0.1.0\nFinished dev [unoptimized] target(s)";
        assert!(!has_error_or_loop_signature(normal));

        let borders = "================\n================\n================";
        assert!(!has_error_or_loop_signature(borders));

        assert!(has_error_or_loop_signature(
            "Do you want to proceed? [y/N]"
        ));
        assert!(has_error_or_loop_signature("Press Enter to continue"));
        assert!(has_error_or_loop_signature(
            "Traceback (most recent call last):\n  File \"app.py\", line 1"
        ));
        assert!(has_error_or_loop_signature(
            "thread 'main' panicked at 'boom'"
        ));
        assert!(has_error_or_loop_signature(
            "error[E0308]: mismatched types"
        ));

        let repeated = "Error: connection refused on port 5432\n\
                        Error: connection refused on port 5432\n\
                        Error: connection refused on port 5432";
        assert!(has_error_or_loop_signature(repeated));
    }
}
