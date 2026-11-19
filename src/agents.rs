use crate::herdr::HcTerminal;
use crate::model::AgentKind;

/// Canonical known coding agent executable/identifier tokens (all lowercase).
pub const KNOWN_AGENTS: &[&str] = &[
    "claude",
    "claude-code",
    "claudecode",
    "opencode",
    "pi",
    "codex",
    "antigravity",
    "agy",
    "gemini",
    "aider",
    "goose",
    "cursor",
    "cursor-agent",
    "amp",
];

/// Canonical plain-shell and multiplexer/attach-wrapper executable tokens (all lowercase).
pub const PLAIN_SHELLS: &[&str] = &[
    "zsh",
    "bash",
    "fish",
    "sh",
    "nu",
    "nushell",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "tmux",
    "screen",
    "login",
    "herdr-core",
    "hc",
];

/// Canonical supervisor / dashboard UI executable tokens (all lowercase).
pub const SUPERVISOR_PROGRAMS: &[&str] = &["herdr-supervisor", "tchelvis-supervisor", "tchelvis"];

/// Normalizes a raw program path or argv[0] token:
/// trims whitespace and trailing path separators, extracts basename across `/` and `\`,
/// strips a single leading `-` (login shell convention like `-zsh`), and lowercases.
pub fn normalize_program_token(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches(['/', '\\']);
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    let unprefixed = base.strip_prefix('-').unwrap_or(base);
    unprefixed.to_ascii_lowercase()
}

/// Maps a program/label token to `AgentKind` using exact normalized-token matching.
pub fn agent_kind_from_token(token: &str) -> Option<AgentKind> {
    match normalize_program_token(token).as_str() {
        "claude" | "claude-code" | "claudecode" => Some(AgentKind::ClaudeCode),
        "opencode" => Some(AgentKind::OpenCode),
        "pi" => Some(AgentKind::Pi),
        "codex" => Some(AgentKind::Codex),
        "antigravity" | "agy" => Some(AgentKind::Antigravity),
        "gemini" | "aider" | "goose" | "cursor" | "cursor-agent" | "amp" => {
            Some(AgentKind::CustomAgent)
        }
        _ => None,
    }
}

/// Returns `true` if `token` normalizes to a canonical known coding agent.
pub fn is_known_agent(token: &str) -> bool {
    agent_kind_from_token(token).is_some()
}

/// Returns `true` if `token` normalizes to a canonical plain shell or wrapper binary.
pub fn is_plain_shell(token: &str) -> bool {
    let norm = normalize_program_token(token);
    PLAIN_SHELLS.contains(&norm.as_str())
}

/// Returns `true` if `token` normalizes to a supervisor or dashboard UI binary.
pub fn is_supervisor_program(token: &str) -> bool {
    let norm = normalize_program_token(token);
    SUPERVISOR_PROGRAMS.contains(&norm.as_str())
}

/// Returns `true` if `(program, args)` represents a `herdr-core attach` or `hc attach` client.
pub fn is_attach_client_invocation(program: Option<&str>, args: &[String]) -> bool {
    let prog_matches = program
        .map(normalize_program_token)
        .is_some_and(|p| p == "herdr-core" || p == "hc");
    let argv0_matches = args
        .first()
        .map(|s| normalize_program_token(s))
        .is_some_and(|p| p == "herdr-core" || p == "hc");
    (prog_matches || argv0_matches)
        && args
            .iter()
            .take_while(|arg| arg.trim() != "--")
            .any(|arg| arg.trim() == "attach")
}

/// Returns `true` if a terminal title/name indicates a Pi session.
pub fn is_pi_title(title: &str) -> bool {
    let trimmed = title.trim();
    trimmed.starts_with("π ") || trimmed.starts_with("π-") || trimmed.contains("π -")
}

fn is_active_status_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "working" | "blocked" | "done" | "error" | "waiting"
    )
}

/// Returns `true` when the terminal is an attach client or currently idling in a plain shell
/// foreground (no known coding agent in `fg_argv` and no active non-shell OSC `status.app`).
pub fn is_plain_shell_foreground_parts(
    argv: &[String],
    fg_name: Option<&str>,
    fg_argv: &[String],
    status_state: Option<&str>,
    status_app: Option<&str>,
) -> bool {
    if is_attach_client_invocation(None, argv) {
        return true;
    }
    let Some(fg_name) = fg_name else {
        return false;
    };
    if is_attach_client_invocation(Some(fg_name), fg_argv) {
        return true;
    }
    if is_plain_shell(fg_name) && !fg_argv.iter().any(|arg| is_known_agent(arg)) {
        if let (Some(state), Some(app)) = (status_state, status_app) {
            let trimmed_app = app.trim();
            if is_active_status_state(state)
                && !trimmed_app.is_empty()
                && !is_plain_shell(trimmed_app)
                && !is_supervisor_program(trimmed_app)
            {
                return false;
            }
        }
        return true;
    }
    false
}

/// Classifies `AgentKind` from explicit `cockpit.agent` label or foreground/launch process names.
pub fn classify_agent_kind(term: &HcTerminal) -> AgentKind {
    if term.state != "finished" && term.is_plain_shell_foreground() {
        return AgentKind::PlainShell;
    }

    if let Some(lbl) = term.labels.get("cockpit.agent") {
        let trimmed = lbl.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("shell") {
            return AgentKind::PlainShell;
        }
        if let Some(kind) = agent_kind_from_token(trimmed) {
            return kind;
        }
        return AgentKind::CustomAgent;
    }

    let mut candidates: Vec<&str> = Vec::new();
    if let Some(fg) = &term.foreground {
        if let Some(name) = fg.name.as_deref() {
            candidates.push(name);
        }
        for arg in &fg.argv {
            candidates.push(arg.as_str());
        }
    }
    if let Some(agent) = term.agent.as_deref() {
        candidates.push(agent);
    }
    for arg in &term.argv {
        candidates.push(arg.as_str());
    }
    if let Some(st) = &term.status {
        if let Some(app) = st.app.as_deref() {
            candidates.push(app);
        }
    }

    for cand in candidates {
        if let Some(kind) = agent_kind_from_token(cand) {
            return kind;
        }
    }

    if term.name.as_deref().is_some_and(is_pi_title) {
        return AgentKind::Pi;
    }

    AgentKind::PlainShell
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{HcForeground, HcStatus};
    use std::collections::BTreeMap;

    #[test]
    fn test_normalize_program_token() {
        assert_eq!(normalize_program_token("-zsh"), "zsh");
        assert_eq!(normalize_program_token("/bin/bash"), "bash");
        assert_eq!(normalize_program_token("/usr/local/bin/-fish"), "fish");
        assert_eq!(normalize_program_token("C:\\tools\\Codex"), "codex");
        assert_eq!(normalize_program_token("  /usr/bin/ksh/  "), "ksh");
        assert_eq!(normalize_program_token("--claude"), "-claude");
        assert_eq!(normalize_program_token(""), "");
    }

    #[test]
    fn test_non_agents_not_matched_by_substring() {
        for non_agent in ["pip", "ampere", "opencodex", "champ", "cursor-helper"] {
            assert!(
                !is_known_agent(non_agent),
                "{non_agent} must not be classified as a known agent"
            );
            assert_eq!(agent_kind_from_token(non_agent), None);
        }
    }

    #[test]
    fn test_canonical_agents_and_kinds() {
        for &agent in KNOWN_AGENTS {
            assert!(
                agent_kind_from_token(agent).is_some(),
                "KNOWN_AGENTS entry `{agent}` must map to an AgentKind"
            );
        }
        assert_eq!(agent_kind_from_token("agy"), Some(AgentKind::Antigravity));
        assert_eq!(
            agent_kind_from_token("antigravity"),
            Some(AgentKind::Antigravity)
        );
        assert_eq!(agent_kind_from_token("claude"), Some(AgentKind::ClaudeCode));
        assert_eq!(
            agent_kind_from_token("claude-code"),
            Some(AgentKind::ClaudeCode)
        );
        assert_eq!(
            agent_kind_from_token("claudecode"),
            Some(AgentKind::ClaudeCode)
        );
        assert_eq!(agent_kind_from_token("codex"), Some(AgentKind::Codex));
        assert_eq!(agent_kind_from_token("opencode"), Some(AgentKind::OpenCode));
        assert_eq!(agent_kind_from_token("pi"), Some(AgentKind::Pi));
        assert_eq!(
            agent_kind_from_token("gemini"),
            Some(AgentKind::CustomAgent)
        );
        assert_eq!(
            agent_kind_from_token("cursor-agent"),
            Some(AgentKind::CustomAgent)
        );
        assert_eq!(agent_kind_from_token("amp"), Some(AgentKind::CustomAgent));
    }

    #[test]
    fn test_plain_shells() {
        for shell in [
            "-zsh",
            "/bin/bash",
            "ksh",
            "csh",
            "tcsh",
            "dash",
            "fish",
            "sh",
            "nu",
            "nushell",
            "tmux",
            "screen",
            "login",
            "herdr-core",
            "hc",
        ] {
            assert!(is_plain_shell(shell), "{shell} must be a plain shell");
        }
        assert!(!is_plain_shell("claude"));
        assert!(!is_plain_shell("pip"));
    }

    #[test]
    fn test_attach_client_invocation() {
        assert!(is_attach_client_invocation(
            None,
            &["herdr-core".into(), "attach".into(), "x".into()]
        ));
        assert!(is_attach_client_invocation(
            Some("hc"),
            &["hc".into(), "attach".into(), "claude".into()]
        ));
        assert!(is_attach_client_invocation(
            None,
            &[
                "/usr/local/bin/herdr-core".into(),
                "--socket".into(),
                "/tmp/hc.sock".into(),
                "attach".into(),
                "t1".into()
            ]
        ));
        assert!(!is_attach_client_invocation(
            Some("hc"),
            &["hc".into(), "run".into(), "--".into(), "attach".into()]
        ));
    }

    #[test]
    fn test_classify_agent_kind_cursor_gemini_and_pip() {
        let cursor_term = HcTerminal {
            address: "t1".into(),
            state: "live".into(),
            foreground: Some(HcForeground {
                name: Some("cursor".into()),
                argv: vec!["cursor".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify_agent_kind(&cursor_term), AgentKind::CustomAgent);

        let gemini_term = HcTerminal {
            address: "t2".into(),
            state: "live".into(),
            foreground: Some(HcForeground {
                name: Some("gemini".into()),
                argv: vec!["gemini".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify_agent_kind(&gemini_term), AgentKind::CustomAgent);

        let pip_in_shell = HcTerminal {
            address: "t3".into(),
            state: "live".into(),
            foreground: Some(HcForeground {
                name: Some("-zsh".into()),
                argv: vec!["-zsh".into(), "-c".into(), "pip".into()],
                ..Default::default()
            }),
            status: Some(HcStatus {
                state: "working".into(),
                app: Some("zsh".into()),
                ..Default::default()
            }),
            labels: BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(classify_agent_kind(&pip_in_shell), AgentKind::PlainShell);
    }
}
