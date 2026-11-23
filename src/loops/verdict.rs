use crate::loops::state::sanitize_single_line;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_SUMMARY_CHARS: usize = 140;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    Pass,
    Retry,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Retry => "RETRY",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn parse_verdict(text: &str) -> Option<Verdict> {
    let last = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let cleaned: String = last
        .trim()
        .chars()
        .filter(|&c| c != '*' && c != '_' && c != '`')
        .collect();
    let (left, right) = cleaned.split_once(':')?;
    if !left.trim().eq_ignore_ascii_case("VERDICT") {
        return None;
    }
    match right.trim().to_ascii_uppercase().as_str() {
        "PASS" => Some(Verdict::Pass),
        "RETRY" => Some(Verdict::Retry),
        _ => None,
    }
}

pub fn summary_line(text: &str) -> String {
    let non_empty: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if non_empty.is_empty() {
        return String::new();
    }
    let last_idx = non_empty.len() - 1;
    let target = if parse_verdict(non_empty[last_idx]).is_some() {
        if last_idx == 0 {
            return String::new();
        }
        non_empty[last_idx - 1]
    } else {
        non_empty[last_idx]
    };
    sanitize_single_line(target, Some(MAX_SUMMARY_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_cases() {
        assert_eq!(parse_verdict("VERDICT: PASS"), Some(Verdict::Pass));
        assert_eq!(parse_verdict(" **VERDICT: RETRY** "), Some(Verdict::Retry));
        assert_eq!(parse_verdict("`verdict: pass`"), Some(Verdict::Pass));
        assert_eq!(
            parse_verdict("Tudo conferido.\nVERDICT: PASS\n\n   \n"),
            Some(Verdict::Pass)
        );

        assert_eq!(parse_verdict("VERDICT: PASS\nMais uma linha"), None);
        assert_eq!(parse_verdict("VERDICT: MAYBE"), None);
        assert_eq!(parse_verdict("VERDICT: PASS RETRY"), None);
        assert_eq!(parse_verdict(""), None);
        assert_eq!(parse_verdict("   \n\t\n "), None);
    }

    #[test]
    fn summary_line_extracts_and_sanitizes_before_verdict() {
        let text = "Detalhes\n\x1b[32mResumo\tcom   controle\x1b[0m\n\nVERDICT: PASS\n\n";
        let line = summary_line(text);
        assert_eq!(line, "Resumo com   controle");

        let long = format!("{}\nVERDICT: RETRY\n", "x".repeat(300));
        let truncated = summary_line(&long);
        assert_eq!(truncated.chars().count(), 140);

        assert_eq!(summary_line("VERDICT: PASS"), "");
    }
}
