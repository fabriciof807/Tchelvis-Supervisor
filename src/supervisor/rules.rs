use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleWarning {
    pub key: String,
    pub entry: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuardrailRules {
    pub invalid: Option<String>,
    pub warnings: Vec<RuleWarning>,
    pub allow_commands: Vec<String>,
    pub deny_commands: Vec<String>,
    pub deny_paths: Vec<String>,
    pub allow_read_tool: bool,
    pub builtin_read_only: bool,
    pub builtin_build_test: bool,
    pub llm_auto_reply: bool,
}

impl Default for GuardrailRules {
    fn default() -> Self {
        Self {
            invalid: None,
            warnings: Vec::new(),
            allow_commands: Vec::new(),
            deny_commands: Vec::new(),
            deny_paths: Vec::new(),
            allow_read_tool: true,
            builtin_read_only: true,
            builtin_build_test: true,
            llm_auto_reply: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawGuardrails {
    allow_commands: Vec<String>,
    deny_commands: Vec<String>,
    deny_paths: Vec<String>,
    allow_tools: Vec<String>,
    builtin_read_only: bool,
    builtin_build_test: bool,
    llm_auto_reply: bool,
}

impl Default for RawGuardrails {
    fn default() -> Self {
        Self {
            allow_commands: Vec::new(),
            deny_commands: Vec::new(),
            deny_paths: Vec::new(),
            allow_tools: vec!["Read".to_string()],
            builtin_read_only: true,
            builtin_build_test: true,
            llm_auto_reply: true,
        }
    }
}

fn is_deny_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '{' | '}' | '$' | '`')
}

/// One way of splitting text into words. A request is denied when any reading matches.
#[derive(Clone, Copy)]
struct DenyView {
    /// Quotes vanish inside a word (`cargo pub''lish`) instead of ending it
    /// (`bash -c "cargo publish"`).
    drop_quotes: bool,
    /// Punctuation ends a word (`cargo publish?`, `a,prod/x`) instead of belonging to it
    /// (`build:prod`).
    split_punctuation: bool,
}

const PLAIN_VIEW: DenyView = DenyView {
    drop_quotes: false,
    split_punctuation: false,
};

fn deny_views() -> impl Iterator<Item = DenyView> {
    [false, true].into_iter().flat_map(|drop_quotes| {
        [false, true].map(|split_punctuation| DenyView {
            drop_quotes,
            split_punctuation,
        })
    })
}

fn contains_run<T: PartialEq>(haystack: &[T], needle: &[T]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_deny_punctuation(c: char) -> bool {
    !c.is_alphanumeric() && !matches!(c, '_' | '-' | '.' | '/' | '~' | '+' | '%')
}

fn tokenize_deny(s: &str, view: DenyView) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for c in s.chars().chain([' ']) {
        let is_quote = matches!(c, '"' | '\'');
        if c == '\\' || (is_quote && view.drop_quotes) {
            continue;
        }
        if is_quote || is_deny_sep(c) || (view.split_punctuation && is_deny_punctuation(c)) {
            if view.split_punctuation {
                // End of a sentence: `cargo publish.`
                cur.truncate(cur.trim_end_matches('.').len());
            }
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.extend(c.to_lowercase());
        }
    }
    tokens
}

fn path_segments_of_token(tok: &str) -> Vec<&str> {
    tok.split(['/', '=', ':'])
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect()
}

fn is_disallowed_star_executor(token: &str) -> bool {
    let base = token
        .rsplit('/')
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    base.starts_with("python")
        || matches!(
            base.as_str(),
            "bash"
                | "sh"
                | "zsh"
                | "fish"
                | "env"
                | "xargs"
                | "eval"
                | "exec"
                | "sudo"
                | "ssh"
                | "nohup"
                | "timeout"
                | "time"
                | "node"
                | "bun"
                | "deno"
                | "perl"
                | "ruby"
                | "npx"
                | "uvx"
        )
}

fn validate_allow_entry(entry: &str, rules: &GuardrailRules) -> Result<String, String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err("entrada vazia".to_string());
    }
    let Some(tokens) = shlex::split(trimmed) else {
        return Err("aspas abertas ou sintaxe inválida".to_string());
    };
    if tokens.is_empty() {
        return Err("entrada vazia".to_string());
    }

    let has_trailing_star = tokens.last().is_some_and(|t| t == "*") && trimmed.ends_with('*');
    let prefix_tokens = if has_trailing_star {
        &tokens[..tokens.len() - 1]
    } else {
        &tokens[..]
    };
    if prefix_tokens.is_empty() {
        return Err("curinga '*' sozinho".to_string());
    }
    if prefix_tokens.iter().any(|t| t.contains('*')) {
        return Err("'*' só é permitido como último token isolado".to_string());
    }

    let prefix_raw = if has_trailing_star {
        trimmed
            .strip_suffix('*')
            .expect("checked ends_with('*')")
            .trim_end()
    } else {
        trimmed
    };

    if let Some(reason) = super::guardrails::floor_violation(prefix_raw) {
        return Err(format!("bloqueada pelo piso fixo ({reason})"));
    }

    if has_trailing_star {
        if let Some(last) = prefix_tokens.last() {
            if is_disallowed_star_executor(last) {
                return Err("executor ou interpretador seguido de '*'".to_string());
            }
        }
    }

    if rules.match_deny_command(prefix_raw).is_some() {
        return Err("coberta por deny_commands".to_string());
    }
    if rules.match_deny_path(prefix_raw).is_some() {
        return Err("coberta por deny_paths".to_string());
    }

    Ok(trimmed.to_string())
}

impl GuardrailRules {
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self {
            invalid: Some(reason.into()),
            allow_read_tool: false,
            builtin_read_only: false,
            builtin_build_test: false,
            llm_auto_reply: false,
            ..Default::default()
        }
    }

    pub fn match_allow(&self, cmd: &str) -> Option<&str> {
        if self.invalid.is_some() || self.allow_commands.is_empty() {
            return None;
        }
        let cmd_tokens = shlex::split(cmd)?;
        if cmd_tokens.is_empty() {
            return None;
        }
        for entry in &self.allow_commands {
            let Some(entry_tokens) = shlex::split(entry) else {
                continue;
            };
            let has_star =
                entry_tokens.last().is_some_and(|t| t == "*") && entry.trim_end().ends_with('*');
            let matched = if has_star {
                let prefix = &entry_tokens[..entry_tokens.len() - 1];
                !prefix.is_empty()
                    && cmd_tokens.len() >= prefix.len()
                    && cmd_tokens[..prefix.len()] == *prefix
            } else {
                cmd_tokens == entry_tokens
            };
            if matched {
                return Some(entry.as_str());
            }
        }
        None
    }

    pub fn match_deny_command(&self, text: &str) -> Option<&str> {
        if self.deny_commands.is_empty() {
            return None;
        }
        self.deny_commands
            .iter()
            .find(|entry| {
                deny_views().any(|view| {
                    contains_run(&tokenize_deny(text, view), &tokenize_deny(entry, view))
                })
            })
            .map(String::as_str)
    }

    pub fn match_deny_path(&self, text: &str) -> Option<&str> {
        self.deny_paths
            .iter()
            .find(|entry| {
                deny_views().any(|view| {
                    // A path is one word; an entry that this view splits cannot match in it.
                    let entry_tokens = tokenize_deny(entry, view);
                    let [entry] = entry_tokens.as_slice() else {
                        return false;
                    };
                    let entry_segs = path_segments_of_token(entry);
                    tokenize_deny(text, view)
                        .iter()
                        .any(|tok| contains_run(&path_segments_of_token(tok), &entry_segs))
                })
            })
            .map(String::as_str)
    }
}

/// The parser's message and line number, never the source line: it may hold an API key.
fn toml_error_reason(raw: &str, err: &toml::de::Error) -> String {
    let line = err
        .span()
        .and_then(|s| raw.get(..s.start))
        .map_or(1, |before| before.matches('\n').count() + 1);
    format!("TOML inválido na linha {line}: {}", err.message().trim())
}

pub fn parse_guardrails(raw_toml: &str) -> GuardrailRules {
    let table: toml::Table = match toml::from_str(raw_toml) {
        Ok(t) => t,
        Err(err) => return GuardrailRules::invalid(toml_error_reason(raw_toml, &err)),
    };
    let Some(val) = table.get("guardrails") else {
        return GuardrailRules::default();
    };
    let raw: RawGuardrails = match val.clone().try_into() {
        Ok(r) => r,
        Err(err) => {
            return GuardrailRules::invalid(format!(
                "[guardrails] inválido: {}",
                err.message().trim()
            ))
        }
    };

    let mut deny_commands = Vec::with_capacity(raw.deny_commands.len());
    for entry in raw.deny_commands {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return GuardrailRules::invalid("entrada vazia em deny_commands");
        }
        if trimmed.contains('*') {
            return GuardrailRules::invalid(format!(
                "'*' não é permitido em deny_commands: {trimmed}"
            ));
        }
        if shlex::split(trimmed).is_none() {
            return GuardrailRules::invalid(format!("aspas abertas em deny_commands: {trimmed}"));
        }
        if tokenize_deny(trimmed, PLAIN_VIEW).is_empty() {
            return GuardrailRules::invalid(format!(
                "entrada sem tokens válidos em deny_commands: {trimmed}"
            ));
        }
        deny_commands.push(trimmed.to_string());
    }

    let mut deny_paths = Vec::with_capacity(raw.deny_paths.len());
    for entry in raw.deny_paths {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return GuardrailRules::invalid("entrada vazia em deny_paths");
        }
        if trimmed.contains('*') {
            return GuardrailRules::invalid(format!(
                "'*' não é permitido em deny_paths: {trimmed}"
            ));
        }
        if shlex::split(trimmed).is_none() {
            return GuardrailRules::invalid(format!("aspas abertas em deny_paths: {trimmed}"));
        }
        let tokens = tokenize_deny(trimmed, PLAIN_VIEW);
        if tokens.len() != 1 || path_segments_of_token(&tokens[0]).is_empty() {
            return GuardrailRules::invalid(format!("caminho inválido em deny_paths: {trimmed}"));
        }
        deny_paths.push(trimmed.to_string());
    }

    let mut rules = GuardrailRules {
        invalid: None,
        warnings: Vec::new(),
        allow_commands: Vec::new(),
        deny_commands,
        deny_paths,
        allow_read_tool: false,
        builtin_read_only: raw.builtin_read_only,
        builtin_build_test: raw.builtin_build_test,
        llm_auto_reply: raw.llm_auto_reply,
    };

    for tool in raw.allow_tools {
        match tool.as_str() {
            "Read" => rules.allow_read_tool = true,
            "Glob" | "Grep" | "LS" => rules.warnings.push(RuleWarning {
                key: "allow_tools".to_string(),
                entry: tool,
                reason: "sem formato real capturado".to_string(),
            }),
            _ => rules.warnings.push(RuleWarning {
                key: "allow_tools".to_string(),
                entry: tool,
                reason: "ferramenta não permitida".to_string(),
            }),
        }
    }

    for entry in raw.allow_commands {
        match validate_allow_entry(&entry, &rules) {
            Ok(valid) => rules.allow_commands.push(valid),
            Err(reason) => rules.warnings.push(RuleWarning {
                key: "allow_commands".to_string(),
                entry,
                reason,
            }),
        }
    }

    rules
}

pub fn load_guardrail_rules(path: &std::path::Path) -> GuardrailRules {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return GuardrailRules::default();
        }
        Err(err) => {
            return GuardrailRules::invalid(format!(
                "não foi possível acessar {}: {err}",
                path.display()
            ));
        }
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => parse_guardrails(&raw),
        Err(err) => {
            GuardrailRules::invalid(format!("não foi possível ler {}: {err}", path.display()))
        }
    }
}

pub fn load_default_guardrail_rules() -> GuardrailRules {
    load_guardrail_rules_for(super::config::resolve_home_dir().as_deref())
}

fn load_guardrail_rules_for(home: Option<&std::path::Path>) -> GuardrailRules {
    home.map_or_else(
        || GuardrailRules::invalid("HOME não definido"),
        load_guardrail_rules_in,
    )
}

/// Anything at the current path wins, even when unreadable: rules must not fall back to the legacy
/// file (or to none) because the current one is broken.
pub fn load_guardrail_rules_in(home: &std::path::Path) -> GuardrailRules {
    let config = home.join(".config");
    let current = config.join("tchelvis").join("supervisor.toml");
    match std::fs::symlink_metadata(&current) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            load_guardrail_rules(&config.join("herdr").join("supervisor.toml"))
        }
        _ => load_guardrail_rules(&current),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(body: &str) -> GuardrailRules {
        parse_guardrails(&format!("[guardrails]\n{body}"))
    }

    #[test]
    fn absent_table_is_default() {
        assert_eq!(
            parse_guardrails("poll_interval_secs = 5"),
            GuardrailRules::default()
        );
        assert_eq!(parse_guardrails(""), GuardrailRules::default());
        let d = GuardrailRules::default();
        assert!(
            d.invalid.is_none()
                && d.allow_read_tool
                && d.builtin_read_only
                && d.builtin_build_test
                && d.llm_auto_reply
        );
    }

    #[test]
    fn allow_is_exact_unless_trailing_star() {
        let r = rules(r#"allow_commands = ["git branch", "cargo test *"]"#);
        assert_eq!(r.match_allow("git  branch"), Some("git branch"));
        for c in ["cargo test", "cargo test --lib x"] {
            assert_eq!(r.match_allow(c), Some("cargo test *"));
        }
        for c in [
            "git branch -D main",
            "/usr/bin/git branch",
            "FOO=1 cargo test",
            "env cargo test",
            "cargo testx",
            "git 'branch",
        ] {
            assert_eq!(r.match_allow(c), None, "{c}");
        }
    }

    #[test]
    fn bad_allow_entries_are_ignored_with_a_warning() {
        for e in [
            "",
            "*",
            "cargo * test",
            "cargo te*",
            "git 'x",
            "rm x",
            "a | b",
            "cat .ssh/x",
            "bash *",
            "python3.12 *",
            "/usr/bin/env *",
            "uv run python *",
            "npx *",
        ] {
            let r = rules(&format!("allow_commands = [{e:?}, \"just test\"]"));
            assert!(r.invalid.is_none(), "{e}");
            assert_eq!(r.allow_commands, ["just test"], "{e}");
            assert_eq!(r.warnings.len(), 1, "{e}");
        }
        let r = rules("allow_commands = [\"git commit\"]\ndeny_commands = [\"git commit\"]");
        assert!(r.allow_commands.is_empty() && r.warnings.len() == 1);
        assert_eq!(
            rules(r#"allow_commands = ["python scripts/check.py"]"#).warnings,
            []
        );
    }

    #[test]
    fn bad_deny_unknown_key_wrong_type_or_broken_toml_is_invalid() {
        for b in [
            "deny_commands = [\"\"]",
            "deny_commands = [\"git *\"]",
            "deny_commands = [\"a 'b\"]",
            "deny_paths = [\"\"]",
            "deny_paths = [\"a/*\"]",
            "deny_command = [\"x\"]",
            "allow_commands = \"x\"",
            "llm_auto_reply = 1",
            "allow_commands = [",
        ] {
            assert!(rules(b).invalid.is_some(), "{b}");
        }
        assert!(parse_guardrails("guardrails = 3").invalid.is_some());
    }

    #[test]
    fn deny_command_matches_anywhere() {
        let r = rules(r#"deny_commands = ["cargo publish"]"#);
        for t in [
            "cargo publish --dry-run",
            "cd x && cargo publish",
            "bash -c \"cargo publish\"",
            "CARGO  Publish",
            "Run: (cargo publish)",
            "cargo\npublish",
            "`cargo publish`",
        ] {
            assert_eq!(r.match_deny_command(t), Some("cargo publish"), "{t}");
        }
        for t in [
            "cargo publish-helper",
            "cargo test publish",
            "mycargo publish",
        ] {
            assert_eq!(r.match_deny_command(t), None, "{t}");
        }
    }

    #[test]
    fn deny_path_matches_whole_segments() {
        let r = rules(r#"deny_paths = [".kube", "prod/secrets"]"#);
        for t in [
            "cat ~/.kube/config",
            "cat a/.KUBE",
            "x --f=prod/secrets/a",
            "cat ./prod/./secrets",
            "host:prod/secrets",
            "cat '.kube/c'",
        ] {
            assert!(r.match_deny_path(t).is_some(), "{t}");
        }
        for t in [
            "cat reprod/secrets",
            "cat prod/x/secrets",
            "cat .kubeconfig",
        ] {
            assert!(r.match_deny_path(t).is_none(), "{t}");
        }
    }

    #[test]
    fn quoting_inside_a_word_does_not_hide_a_denied_command_or_path() {
        let r = rules("deny_commands = [\"cargo publish\"]\ndeny_paths = [\"prod\"]");
        for t in [
            "cargo pub''lish",
            "cargo \"pub\"lish",
            "car\"\"go pu\\blish",
            "cargo 'publish'",
        ] {
            assert_eq!(r.match_deny_command(t), Some("cargo publish"), "{t}");
        }
        for t in ["cat pr''od/x", "cat \"pr\"od/x", "cat 'prod'/x"] {
            assert_eq!(r.match_deny_path(t), Some("prod"), "{t}");
        }
    }

    #[test]
    fn invalid_reason_never_shows_the_source_line() {
        for raw in [
            "api_key = \"sk-hunter2\" oops\n",
            "api_key = \"sk-hunter2\n",
        ] {
            let reason = parse_guardrails(raw).invalid.unwrap();
            assert!(!reason.contains("hunter2"), "{reason}");
            assert!(reason.contains("linha 1"), "{reason}");
        }
    }

    #[test]
    fn allow_tools_is_a_closed_list() {
        assert!(!rules("allow_tools = []").allow_read_tool);
        let r = rules(r#"allow_tools = ["Read", "Grep", "Bash", "Write", "mcp__x"]"#);
        assert!(r.allow_read_tool && r.invalid.is_none());
        assert_eq!(r.warnings.len(), 4);
    }

    #[test]
    fn non_ascii_and_long_input_do_not_panic() {
        let r = rules(
            "allow_commands = [\"just ação\"]\ndeny_commands = [\"é ü\"]\ndeny_paths = [\"ção\"]",
        );
        let long = "ação…".repeat(20_000);
        assert!(r.match_allow(&long).is_none());
        assert_eq!(r.match_allow("just ação"), Some("just ação"));
        let _ = (r.match_deny_command(&long), r.match_deny_path(&long));
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_guardrail_rules(&dir.path().join("missing.toml")),
            GuardrailRules::default()
        );
    }

    #[test]
    fn directory_or_broken_symlink_or_bad_utf8_is_invalid() {
        let dir = tempfile::tempdir().unwrap();

        let as_dir = dir.path().join("dir.toml");
        std::fs::create_dir(&as_dir).unwrap();
        assert!(load_guardrail_rules(&as_dir).invalid.is_some());

        #[cfg(unix)]
        {
            let broken = dir.path().join("broken.toml");
            std::os::unix::fs::symlink(dir.path().join("target_missing.toml"), &broken).unwrap();
            assert!(load_guardrail_rules(&broken).invalid.is_some());
        }

        let bad_utf8 = dir.path().join("bad_utf8.toml");
        std::fs::write(&bad_utf8, [0xff_u8, 0xfe, 0xfd]).unwrap();
        assert!(load_guardrail_rules(&bad_utf8).invalid.is_some());
    }

    #[test]
    fn missing_home_is_invalid() {
        assert!(load_guardrail_rules_for(None).invalid.is_some());
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            load_guardrail_rules_for(Some(home.path())),
            GuardrailRules::default()
        );
    }

    #[test]
    fn deny_entries_with_punctuation_still_match_exactly() {
        let r = rules("deny_commands = [\"npm run build:prod\"]\ndeny_paths = [\"a,b\", \"c@d\"]");
        assert!(r.invalid.is_none());
        assert!(r.match_deny_command("npm run build:prod").is_some());
        assert!(r.match_deny_command("npm run build:prod?").is_some());
        assert!(r.match_deny_command("npm run build").is_none());
        assert_eq!(r.match_deny_path("cat x/a,b/c"), Some("a,b"));
        assert!(r.match_deny_path("cat a b").is_none());
        assert!(r.match_deny_path("cat c@d/x").is_some());
    }

    #[test]
    fn legacy_file_is_used_only_when_the_current_one_is_absent() {
        let home = tempfile::tempdir().unwrap();
        let (cur, old) = (
            home.path().join(".config/tchelvis"),
            home.path().join(".config/herdr"),
        );
        assert_eq!(
            load_guardrail_rules_in(home.path()),
            GuardrailRules::default()
        );
        std::fs::create_dir_all(&cur).unwrap();
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(
            old.join("supervisor.toml"),
            "[guardrails]\nallow_commands = [\"just test\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_guardrail_rules_in(home.path()).allow_commands,
            ["just test"]
        );
        std::os::unix::fs::symlink(home.path().join("gone"), cur.join("supervisor.toml")).unwrap();
        assert!(load_guardrail_rules_in(home.path()).invalid.is_some());
    }

    #[test]
    fn unrelated_bad_setting_keeps_deny_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        std::fs::write(
            &path,
            "poll_interval_secs = \"x\"\n\n[guardrails]\ndeny_commands = [\"npm publish\"]\n",
        )
        .unwrap();
        let r = load_guardrail_rules(&path);
        assert!(r.invalid.is_none());
        assert_eq!(r.deny_commands, ["npm publish"]);
    }
}
