use super::guardrails::{decide_fixed, FixedDecision};
use super::rules::{GuardrailRules, RuleWarning};
use serde::Serialize;

const CLAUDE_PERMISSION_MENU: &str = "Do you want to proceed?\n❯ 1. Yes\n  2. No";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleTestReport {
    pub approved: bool,
    pub rule_id: String,
    pub command: Option<String>,
    pub payload: Option<String>,
    pub warnings: Vec<RuleWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RulesReport<'a> {
    pub status: &'static str,
    #[serde(flatten)]
    pub rules: &'a GuardrailRules,
}

impl<'a> RulesReport<'a> {
    pub fn from_rules(rules: &'a GuardrailRules) -> Self {
        Self {
            status: if rules.invalid.is_some() {
                "invalid"
            } else {
                "ok"
            },
            rules,
        }
    }
}

fn decision_to_report(decision: FixedDecision, rules: &GuardrailRules) -> RuleTestReport {
    let approved = decision.approval.is_some();
    let (command, payload) = match decision.approval {
        Some(a) => (Some(a.command), Some(a.payload)),
        None => (None, None),
    };
    RuleTestReport {
        approved,
        rule_id: decision.rule_id,
        command,
        payload,
        warnings: rules.warnings.clone(),
    }
}

pub fn test_command(rules: &GuardrailRules, cmd: &str, tool: Option<&str>) -> RuleTestReport {
    let tool_name = tool
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Bash");
    let msg = format!("approve {tool_name}: {cmd}");
    let decision = decide_fixed(
        CLAUDE_PERMISSION_MENU,
        Some("permission"),
        Some(&msg),
        rules,
    );
    decision_to_report(decision, rules)
}

pub fn test_screen(
    rules: &GuardrailRules,
    screen: &str,
    msg: Option<&str>,
    kind: Option<&str>,
) -> RuleTestReport {
    let decision = decide_fixed(screen, kind, msg, rules);
    decision_to_report(decision, rules)
}

pub fn format_rules_human(rules: &GuardrailRules) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let status = if rules.invalid.is_some() {
        "invalid"
    } else {
        "ok"
    };
    let _ = writeln!(out, "Estado: {status}");
    if let Some(reason) = &rules.invalid {
        let _ = writeln!(out, "Motivo: {reason}");
        let _ = writeln!(
            out,
            "Efeito: todas as respostas automáticas (fixas e LLM) estão desativadas."
        );
    }

    let _ = writeln!(out, "\nOrigem: piso (fixo, imutável)");
    let _ = writeln!(
        out,
        "  - bloqueia tokens destrutivos, redirecionamentos `> /`, `curl|sh`, `wget|sh`, metacaracteres/expansões, glob em segmento `.`, caminhos sensíveis, opções perigosas de rg/git, kind=question e opções always-allow"
    );

    let _ = writeln!(out, "\nOrigem: embutida");
    let _ = writeln!(out, "  builtin_read_only: {}", rules.builtin_read_only);
    let _ = writeln!(
        out,
        "  allow_tools (Read): {}",
        if rules.allow_read_tool {
            "ativo"
        } else {
            "desativado"
        }
    );
    let _ = writeln!(out, "  builtin_build_test: {}", rules.builtin_build_test);
    let _ = writeln!(out, "  llm_auto_reply: {}", rules.llm_auto_reply);

    let _ = writeln!(out, "\nOrigem: usuário ([guardrails])");
    let _ = writeln!(
        out,
        "  allow_commands: {}",
        format_list(&rules.allow_commands)
    );
    let _ = writeln!(
        out,
        "  deny_commands: {}",
        format_list(&rules.deny_commands)
    );
    let _ = writeln!(out, "  deny_paths: {}", format_list(&rules.deny_paths));

    if !rules.warnings.is_empty() {
        let _ = writeln!(out, "\nAvisos ({}):", rules.warnings.len());
        for w in &rules.warnings {
            let _ = writeln!(out, "  - [{}] {:?}: {}", w.key, w.entry, w.reason);
        }
    }
    out
}

fn format_list(items: &[String]) -> String {
    if items.is_empty() {
        "(nenhuma)".to_string()
    } else {
        items.join(", ")
    }
}

pub fn format_rules_test_human(report: &RuleTestReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let verdict = if report.approved {
        "APROVADO"
    } else {
        "NÃO APROVADO"
    };
    let _ = writeln!(out, "{verdict} ({})", report.rule_id);
    if let Some(cmd) = &report.command {
        let _ = writeln!(out, "Comando: {cmd}");
    }
    if let Some(payload) = &report.payload {
        let _ = writeln!(out, "Payload: {payload}");
    }
    if !report.warnings.is_empty() {
        for w in &report.warnings {
            let _ = writeln!(out, "Aviso [{}] {:?}: {}", w.key, w.entry, w.reason);
        }
    }
    let _ = writeln!(
        out,
        "Simulação local: nenhuma resposta foi enviada ao terminal."
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::rules::parse_guardrails;

    const MENU: &str = "Do you want to proceed?\n❯ 1. Yes\n  2. No";

    fn rules(body: &str) -> GuardrailRules {
        parse_guardrails(&format!("[guardrails]\n{body}"))
    }

    #[test]
    fn report_matches_the_supervisor_decision() {
        let default_rules = GuardrailRules::default();
        let allow_just = rules(r#"allow_commands = ["just test"]"#);
        let no_readonly = rules("builtin_read_only = false");
        let no_tools = rules("allow_tools = []");
        let deny_git_log = rules(r#"deny_commands = ["git log"]"#);
        let deny_prod = rules(r#"deny_paths = ["prod"]"#);
        let allow_just_star_deny_deploy =
            rules("allow_commands = [\"just *\"]\ndeny_commands = [\"just deploy\"]");
        let deny_cargo_publish = rules(r#"deny_commands = ["cargo publish"]"#);
        let deny_cargo_test = rules(r#"deny_commands = ["cargo test"]"#);
        let invalid_rules = GuardrailRules::invalid("x");

        let cases: Vec<(&str, Option<&str>, &GuardrailRules)> = vec![
            ("just test", None, &allow_just),
            ("just test x", None, &allow_just),
            ("just test & evil", None, &allow_just),
            ("git status", None, &default_rules),
            ("a.rs", Some("Read"), &default_rules),
            ("git status", None, &no_readonly),
            ("a.rs", Some("Read"), &no_tools),
            ("git log", None, &deny_git_log),
            ("cat prod/x", None, &deny_prod),
            ("prod/x", Some("Read"), &deny_prod),
            ("just test", None, &allow_just_star_deny_deploy),
            ("just deploy", None, &allow_just_star_deny_deploy),
            ("cargo publish", None, &deny_cargo_publish),
            ("cargo test", None, &default_rules),
            ("cargo test auth_token", None, &default_rules),
            ("pytest -k password", None, &default_rules),
            ("npm run test key", None, &default_rules),
            ("cargo test auth_token", None, &deny_cargo_test),
            ("ls", None, &invalid_rules),
            ("ls", Some("Bash"), &default_rules),
            ("ls\npython evil.py", None, &default_rules),
        ];

        for (cmd, tool, r) in cases {
            let tool_name = tool.unwrap_or("Bash");
            let msg = format!("approve {tool_name}: {cmd}");
            let expected = decide_fixed(MENU, Some("permission"), Some(&msg), r);
            let report = test_command(r, cmd, tool);
            assert_eq!(
                report.approved,
                expected.approval.is_some(),
                "cmd={cmd:?} tool={tool:?}"
            );
            assert_eq!(
                report.rule_id, expected.rule_id,
                "cmd={cmd:?} tool={tool:?}"
            );
            assert_eq!(
                report.payload,
                expected.approval.as_ref().map(|a| a.payload.clone()),
                "cmd={cmd:?} tool={tool:?}"
            );
            assert_eq!(
                report.command,
                expected.approval.as_ref().map(|a| a.command.clone()),
                "cmd={cmd:?} tool={tool:?}"
            );
        }
    }

    #[test]
    fn command_with_newline_is_not_approved() {
        assert!(!test_command(&GuardrailRules::default(), "ls\npython evil.py", None).approved);
    }

    #[test]
    fn test_screen_matches_decide_fixed() {
        let r = rules(r#"allow_commands = ["just test"]"#);
        let screen = "Bash command\n  just test\nAllow? [y/N]";
        let expected = decide_fixed(screen, None, None, &r);
        let report = test_screen(&r, screen, None, None);
        assert!(report.approved);
        assert_eq!(report.rule_id, expected.rule_id);
        assert_eq!(report.payload.as_deref(), Some("y"));
        assert_eq!(report.command.as_deref(), Some("just test"));

        let q = test_screen(&r, MENU, Some("approve Bash: just test"), Some("question"));
        assert!(!q.approved);
        assert_eq!(q.rule_id, "floor:question");
    }

    #[test]
    fn human_outputs_are_in_portuguese_and_state_dry_run() {
        let ok = test_command(&GuardrailRules::default(), "ls", None);
        let text = format_rules_test_human(&ok);
        let first_line = text.lines().next().unwrap();
        assert_eq!(first_line, "APROVADO (builtin_read_only)");
        assert!(text.contains("nenhuma resposta foi enviada"));

        let bad = test_command(&GuardrailRules::default(), "rm -rf x", None);
        let bad_text = format_rules_test_human(&bad);
        assert_eq!(
            bad_text.lines().next().unwrap(),
            "NÃO APROVADO (floor:destructive)"
        );

        let rules_text = format_rules_human(&GuardrailRules::default());
        assert!(rules_text.contains("Estado: ok"));
        assert!(rules_text.contains("Origem: piso"));
        assert!(rules_text.contains("Origem: embutida"));
        assert!(rules_text.contains("Origem: usuário"));
    }
}
