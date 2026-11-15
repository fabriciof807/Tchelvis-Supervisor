use crate::model::{
    sanitize_terminal_text, ProviderHealth, ProviderId, ProviderStatus, RunbookRef,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_RUNBOOK_BYTES: u64 = 256 * 1024; // 256 KiB

#[derive(Debug, Clone, Default)]
pub struct ParsedRunbook {
    pub path: PathBuf,
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub tickets: Vec<String>,
    pub service: Option<String>,
    pub ci_job: Option<String>,
    pub branch_pattern: Option<String>,
}

pub fn check_health(home_dir: &Path) -> ProviderHealth {
    let global_dir = home_dir.join(".config/tchelvis/runbooks");
    let detail = if global_dir.exists() {
        format!(
            "Local runbooks active (global dir {} present)",
            global_dir.display()
        )
    } else {
        "Local runbook discovery active (RUNBOOK.md, docs/runbooks/, .tchelvis/runbooks/)".into()
    };
    ProviderHealth {
        provider: ProviderId::Runbooks,
        status: ProviderStatus::Ready,
        detail: Some(detail),
        ..Default::default()
    }
}

pub fn discover_and_match_runbooks(
    repo_root: Option<&str>,
    cwd: &str,
    branch: Option<&str>,
    candidate_tickets: &[String],
    failing_checks: &[String],
    explicit_label: Option<&str>,
    home_dir: &Path,
) -> Vec<RunbookRef> {
    let mut candidate_files: Vec<PathBuf> = Vec::new();
    let mut gitignore_entries: Vec<String> = Vec::new();

    if let Some(root_str) = repo_root {
        let root = Path::new(root_str);
        gitignore_entries = load_gitignore_patterns(root);

        let root_rb = root.join("RUNBOOK.md");
        if root_rb.is_file() {
            candidate_files.push(root_rb);
        }
        collect_markdown_files(&root.join("docs/runbooks"), 0, 3, &mut candidate_files);
        collect_markdown_files(&root.join(".tchelvis/runbooks"), 0, 3, &mut candidate_files);
    } else {
        let cwd_path = Path::new(cwd);
        let cwd_rb = cwd_path.join("RUNBOOK.md");
        if cwd_rb.is_file() {
            candidate_files.push(cwd_rb);
        }
    }

    collect_markdown_files(
        &home_dir.join(".config/tchelvis/runbooks"),
        0,
        3,
        &mut candidate_files,
    );

    if let Some(lbl) = explicit_label.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let lbl_path = if Path::new(lbl).is_absolute() {
            PathBuf::from(lbl)
        } else if let Some(r) = repo_root {
            Path::new(r).join(lbl)
        } else {
            Path::new(cwd).join(lbl)
        };
        if lbl_path.is_file() && !candidate_files.contains(&lbl_path) {
            candidate_files.push(lbl_path);
        }
    }

    let mut seen = BTreeSet::new();
    let mut scored: Vec<(u32, RunbookRef)> = Vec::new();

    for file in candidate_files {
        if let Some(root_str) = repo_root {
            if is_gitignored(Path::new(root_str), &file, &gitignore_entries) {
                continue;
            }
        }
        let canon = fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        if !seen.insert(canon) {
            continue;
        }
        let Some(parsed) = parse_runbook_file(&file) else {
            continue;
        };

        let (score, reason) = score_runbook(
            &parsed,
            repo_root,
            cwd,
            branch,
            candidate_tickets,
            failing_checks,
            explicit_label,
        );
        if score > 0 {
            let display_path = if let Some(root_str) = repo_root {
                file.strip_prefix(root_str)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| file.to_string_lossy().to_string())
            } else {
                file.to_string_lossy().to_string()
            };
            scored.push((
                score,
                RunbookRef {
                    path: sanitize_terminal_text(&display_path),
                    title: parsed.title,
                    summary: parsed.summary,
                    matched_reason: reason,
                    tags: parsed.tags,
                    service: parsed.service,
                    ci_job: parsed.ci_job,
                },
            ));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    scored.into_iter().map(|(_, r)| r).collect()
}

fn collect_markdown_files(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
    if depth > max_depth || !dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_markdown_files(&path, depth + 1, max_depth, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn load_gitignore_patterns(repo_root: &Path) -> Vec<String> {
    let gi = repo_root.join(".gitignore");
    let Ok(raw) = fs::read_to_string(gi) else {
        return Vec::new();
    };
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .map(|l| l.trim_matches('/').to_string())
        .collect()
}

fn is_gitignored(repo_root: &Path, file: &Path, patterns: &[String]) -> bool {
    let Ok(rel) = file.strip_prefix(repo_root) else {
        return false;
    };
    let rel_str = rel.to_string_lossy();
    for pat in patterns {
        if pat.is_empty() {
            continue;
        }
        if rel_str == *pat || rel_str.starts_with(&format!("{pat}/")) {
            return true;
        }
    }
    false
}

pub fn parse_runbook_file(path: &Path) -> Option<ParsedRunbook> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_RUNBOOK_BYTES {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    Some(parse_runbook_str(path, &raw))
}

pub fn parse_runbook_str(path: &Path, raw: &str) -> ParsedRunbook {
    let mut rb = ParsedRunbook {
        path: path.to_path_buf(),
        ..Default::default()
    };

    let mut lines = raw.lines().peekable();
    // Parse optional YAML frontmatter between --- and ---
    if lines.peek().map(|l| l.trim()) == Some("---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim().trim_matches('"').trim_matches('\'');
                match key.as_str() {
                    "title" => rb.title = sanitize_terminal_text(val),
                    "service" => rb.service = Some(sanitize_terminal_text(val)),
                    "ci_job" => rb.ci_job = Some(sanitize_terminal_text(val)),
                    "branch_pattern" | "branches" => {
                        rb.branch_pattern = Some(sanitize_terminal_text(val))
                    }
                    "tags" => rb.tags = parse_yaml_inline_list(val),
                    "tickets" => rb.tickets = parse_yaml_inline_list(val),
                    _ => {}
                }
            }
        }
    }

    let mut summary_lines = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !summary_lines.is_empty() {
                break;
            }
            continue;
        }
        if let Some(h) = trimmed.strip_prefix('#') {
            if rb.title.is_empty() {
                rb.title = sanitize_terminal_text(h.trim_start_matches('#').trim());
            }
            continue;
        }
        summary_lines.push(sanitize_terminal_text(trimmed));
        if summary_lines.len() >= 2 {
            break;
        }
    }

    if rb.title.is_empty() {
        rb.title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(sanitize_terminal_text)
            .unwrap_or_else(|| "Runbook".into());
    }
    rb.summary = summary_lines.join(" ");
    rb
}

fn parse_yaml_inline_list(val: &str) -> Vec<String> {
    let inner = val.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| sanitize_terminal_text(s.trim().trim_matches('"').trim_matches('\'')))
        .filter(|s| !s.is_empty())
        .collect()
}

fn score_runbook(
    rb: &ParsedRunbook,
    repo_root: Option<&str>,
    cwd: &str,
    branch: Option<&str>,
    candidate_tickets: &[String],
    failing_checks: &[String],
    explicit_label: Option<&str>,
) -> (u32, String) {
    let file_name = rb
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let file_stem = rb
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    if let Some(lbl) = explicit_label.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if rb.path.to_string_lossy().ends_with(lbl) || file_name == lbl || file_stem == lbl {
            return (100, "Explicit cockpit.runbook label".into());
        }
    }

    for check in failing_checks {
        if rb.ci_job.as_deref() == Some(check.as_str())
            || file_stem.eq_ignore_ascii_case(check)
            || rb.tags.iter().any(|t| t.eq_ignore_ascii_case(check))
        {
            return (90, format!("Matches failing CI check '{check}'"));
        }
    }

    for ticket in candidate_tickets {
        if rb.tickets.iter().any(|t| t.eq_ignore_ascii_case(ticket))
            || rb.tags.iter().any(|t| t.eq_ignore_ascii_case(ticket))
        {
            return (80, format!("Matches ticket '{ticket}'"));
        }
    }

    if let (Some(pat), Some(br)) = (rb.branch_pattern.as_deref(), branch) {
        let pat_clean = pat.trim_end_matches('*');
        if !pat_clean.is_empty() && br.contains(pat_clean) {
            return (70, format!("Matches branch pattern '{pat}'"));
        }
    }

    if let Some(svc) = rb.service.as_deref() {
        if cwd.contains(svc) {
            return (60, format!("Matches service '{svc}' in working directory"));
        }
    }

    if file_name.eq_ignore_ascii_case("RUNBOOK.md") {
        if let Some(root) = repo_root {
            if rb.path.starts_with(root) {
                return (50, "Repository root RUNBOOK.md".into());
            }
        }
        return (40, "Workspace RUNBOOK.md".into());
    }

    // Any runbook inside the current repo's docs/runbooks or .tchelvis/runbooks is included with base relevance
    if let Some(root) = repo_root {
        if rb.path.starts_with(root) {
            return (30, "Repository runbook".into());
        }
    }

    (0, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_match_runbooks_by_ci_job_and_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let rb_dir = tmp.path().join("docs/runbooks");
        fs::create_dir_all(&rb_dir).unwrap();

        let rb_file = rb_dir.join("integration-tests.md");
        fs::write(
            &rb_file,
            "---\ntitle: Integration Tests Triage\nci_job: integration-tests\ntags: [ci, auth]\n---\n# Heading\nReset the local test database before retrying.\n",
        )
        .unwrap();

        let matched = discover_and_match_runbooks(
            Some(tmp.path().to_str().unwrap()),
            tmp.path().to_str().unwrap(),
            Some("feat/ENG-42"),
            &["ENG-42".into()],
            &["integration-tests".into()],
            None,
            tmp.path(),
        );
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].title, "Integration Tests Triage");
        assert!(matched[0]
            .matched_reason
            .contains("Matches failing CI check 'integration-tests'"));
    }

    #[test]
    fn runbooks_reads_tchelvis_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let rb_dir = tmp.path().join(".tchelvis/runbooks");
        fs::create_dir_all(&rb_dir).unwrap();
        fs::write(rb_dir.join("b.md"), "# Tchelvis Runbook\nCheck logs.\n").unwrap();

        let matched = discover_and_match_runbooks(
            Some(tmp.path().to_str().unwrap()),
            tmp.path().to_str().unwrap(),
            None,
            &[],
            &[],
            None,
            tmp.path(),
        );
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].path, ".tchelvis/runbooks/b.md");
    }
}
