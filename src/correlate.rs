use crate::herdr::HcTerminal;
use crate::model::{
    sanitize_terminal_text, AgentKind, AgentRow, AttentionState, CorrelatedContext,
    CorrelationTier, ProvenanceEdge, RemoteRepo, WorkspaceInfo,
};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const TECHNICAL_PREFIX_DENYLIST: [&str; 11] = [
    "UTF", "SHA", "ISO", "RFC", "CVE", "HTTP", "TLS", "SSH", "OSC", "PR", "MR",
];

static TICKET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Z][A-Z0-9_]{1,9})-(\d+)\b").expect("valid ticket regex"));

static SLACK_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https://([a-zA-Z0-9\-_]+)\.slack\.com/archives/([A-Z0-9]+)/p(\d{10,})")
        .expect("valid slack url regex")
});

/// Classify `AgentKind` from explicit `cockpit.agent` label or foreground/launch process names.
pub fn classify_agent_kind(term: &HcTerminal) -> AgentKind {
    if term.state != "finished" && term.is_plain_shell_foreground() {
        return AgentKind::PlainShell;
    }

    if let Some(lbl) = term.labels.get("cockpit.agent") {
        let lower = lbl.trim().to_ascii_lowercase();
        return match lower.as_str() {
            "claude" | "claude-code" | "claudecode" => AgentKind::ClaudeCode,
            "opencode" => AgentKind::OpenCode,
            "pi" => AgentKind::Pi,
            "codex" => AgentKind::Codex,
            "shell" | "" => AgentKind::PlainShell,
            _ => AgentKind::CustomAgent,
        };
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
        let base = Path::new(cand)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(cand)
            .to_ascii_lowercase();
        match base.as_str() {
            "claude" | "claude-code" => return AgentKind::ClaudeCode,
            "opencode" => return AgentKind::OpenCode,
            "pi" => return AgentKind::Pi,
            "codex" => return AgentKind::Codex,
            "aider" | "goose" | "cursor-agent" | "amp" => return AgentKind::CustomAgent,
            _ => {}
        }
    }

    if term.name.as_deref().is_some_and(|n| n.contains("π -")) {
        return AgentKind::Pi;
    }

    AgentKind::PlainShell
}

fn is_plain_shell_name(name: &str) -> bool {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "zsh" | "bash" | "fish" | "sh" | "nu" | "dash" | "tmux" | "screen"
    )
}

/// Returns `true` if the terminal should be included in the fleet view (`include_all` bypasses the plain-shell exclusion filter).
pub fn should_include_terminal(term: &HcTerminal, include_all: bool) -> bool {
    if include_all {
        return true;
    }
    if term.state != "finished" && term.is_plain_shell_foreground() {
        return ["cockpit.issue", "cockpit.pr", "cockpit.slack", "cockpit.runbook"]
            .iter()
            .any(|k| term.labels.get(*k).is_some_and(|v| !v.trim().is_empty()));
    }
    let kind = classify_agent_kind(term);
    if kind != AgentKind::PlainShell {
        return true;
    }
    if term.labels.iter().any(|(k, v)| {
        if !k.starts_with("cockpit.") || v.trim().is_empty() {
            return false;
        }
        if kind == AgentKind::PlainShell {
            let ignored = matches!(
                k.as_str(),
                "cockpit.state"
                    | "cockpit.kind"
                    | "cockpit.msg"
                    | "cockpit.supervisor_state"
                    | "cockpit.supervisor_decision"
            ) || (k == "cockpit.agent" && v.trim().eq_ignore_ascii_case("shell"));
            return !ignored;
        }
        true
    }) {
        return true;
    }
    if let Some(st) = &term.status {
        let state = st.state.trim().to_ascii_lowercase();
        let active_state = matches!(state.as_str(), "working" | "blocked" | "done" | "error");
        let non_shell_app = st
            .app
            .as_deref()
            .map(str::trim)
            .is_some_and(|app| !app.is_empty() && !is_plain_shell_name(app));
        if active_state && non_shell_app {
            return true;
        }
    }
    false
}

/// Resolves `.git` directory or linked worktree `gitdir:` + `commondir` metadata directly from disk.
pub fn resolve_git_workspace(cwd_str: &str) -> WorkspaceInfo {
    let clean_cwd = sanitize_terminal_text(cwd_str);
    let mut info = WorkspaceInfo {
        cwd: clean_cwd.clone(),
        ..Default::default()
    };

    let start = Path::new(&clean_cwd);
    if !start.exists() {
        return info;
    }

    let mut cur = Some(start);
    let mut found_worktree: Option<(PathBuf, PathBuf, PathBuf)> = None;

    while let Some(dir) = cur {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            found_worktree = Some((dir.to_path_buf(), dot_git.clone(), dot_git));
            break;
        } else if dot_git.is_file() {
            if let Ok(content) = fs::read_to_string(&dot_git) {
                if let Some(rest) = content.trim().strip_prefix("gitdir:") {
                    let raw_gitdir = Path::new(rest.trim());
                    let gitdir = if raw_gitdir.is_absolute() {
                        raw_gitdir.to_path_buf()
                    } else {
                        dir.join(raw_gitdir)
                    };
                    let gitdir = fs::canonicalize(&gitdir).unwrap_or(gitdir);
                    let commondir = resolve_commondir(&gitdir);
                    found_worktree = Some((dir.to_path_buf(), gitdir, commondir));
                    break;
                }
            }
        }
        cur = dir.parent();
    }

    let Some((worktree_root, gitdir, commondir)) = found_worktree else {
        return info;
    };

    let repo_root = commondir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| worktree_root.clone());

    info.worktree_path = Some(worktree_root.to_string_lossy().to_string());
    info.repo_root = Some(repo_root.to_string_lossy().to_string());

    // Read <gitdir>/HEAD
    let head_path = gitdir.join("HEAD");
    if let Ok(head_raw) = fs::read_to_string(&head_path) {
        let head_trimmed = head_raw.trim();
        if let Some(ref_rel) = head_trimmed.strip_prefix("ref:") {
            let ref_name = ref_rel.trim();
            if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
                info.git_branch = Some(sanitize_terminal_text(branch));
            } else {
                info.git_branch = Some(sanitize_terminal_text(ref_name));
            }
            info.git_head_sha = resolve_ref_sha(&gitdir, &commondir, ref_name);
        } else if head_trimmed.len() >= 7 && head_trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            info.git_head_sha = Some(head_trimmed.to_string());
        }
    }

    // Read <commondir>/config for remote URLs
    let config_path = commondir.join("config");
    if let Ok(cfg_raw) = fs::read_to_string(&config_path) {
        info.remote_repo = parse_git_config_remote(&cfg_raw);
    }

    info
}

/// Resolves the actual `<gitdir>/HEAD` path for a working directory (used by the daemon 2s worktree watcher).
pub fn resolve_git_head_file(cwd_str: &str) -> Option<PathBuf> {
    let mut cur = Some(Path::new(cwd_str));
    while let Some(dir) = cur {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git.join("HEAD"));
        } else if dot_git.is_file() {
            if let Ok(content) = fs::read_to_string(&dot_git) {
                if let Some(rest) = content.trim().strip_prefix("gitdir:") {
                    let raw = Path::new(rest.trim());
                    let gitdir = if raw.is_absolute() {
                        raw.to_path_buf()
                    } else {
                        dir.join(raw)
                    };
                    return Some(gitdir.join("HEAD"));
                }
            }
        }
        cur = dir.parent();
    }
    None
}

fn resolve_commondir(gitdir: &Path) -> PathBuf {
    let commondir_file = gitdir.join("commondir");
    if let Ok(raw) = fs::read_to_string(&commondir_file) {
        let rel = raw.trim();
        if !rel.is_empty() {
            let candidate = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                gitdir.join(rel)
            };
            return fs::canonicalize(&candidate).unwrap_or(candidate);
        }
    }
    gitdir.to_path_buf()
}

fn resolve_ref_sha(gitdir: &Path, commondir: &Path, ref_name: &str) -> Option<String> {
    for base in [gitdir, commondir] {
        let direct = base.join(ref_name);
        if let Ok(sha) = fs::read_to_string(&direct) {
            let s = sha.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    let packed = commondir.join("packed-refs");
    if let Ok(content) = fs::read_to_string(&packed) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            if let (Some(sha), Some(rname)) = (parts.next(), parts.next()) {
                if rname == ref_name {
                    return Some(sha.to_string());
                }
            }
        }
    }
    None
}

pub fn parse_git_config_remote(config_text: &str) -> Option<RemoteRepo> {
    let mut remotes: BTreeMap<String, String> = BTreeMap::new();
    let mut current_remote: Option<String> = None;

    for raw_line in config_text.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            current_remote = None;
            let inner = &line[1..line.len() - 1];
            if let Some(rest) = inner.strip_prefix("remote ") {
                let name = rest.trim().trim_matches('"');
                if !name.is_empty() {
                    current_remote = Some(name.to_string());
                }
            }
        } else if let Some(rem_name) = &current_remote {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case("url") {
                    remotes.insert(rem_name.clone(), v.trim().to_string());
                }
            }
        }
    }

    let chosen_url = remotes
        .get("origin")
        .or_else(|| remotes.get("upstream"))
        .or_else(|| remotes.values().next())?;

    normalize_remote_url(chosen_url)
}

pub fn normalize_remote_url(raw_url: &str) -> Option<RemoteRepo> {
    let trimmed = raw_url.trim().trim_end_matches('/');
    let without_git = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    // Handle SCP-like SSH: git@github.com:owner/repo
    if let Some(after_at) = without_git.strip_prefix("git@") {
        if let Some((host, path)) = after_at.split_once(':') {
            return split_host_owner_repo(host, path);
        }
    }

    // Handle scheme URLs: https://github.com/owner/repo or ssh://git@gitlab.com/group/sub/repo
    if let Some((_, after_scheme)) = without_git.split_once("://") {
        let host_and_path = after_scheme
            .split_once('@')
            .map(|(_, hp)| hp)
            .unwrap_or(after_scheme);
        if let Some((host_port, path)) = host_and_path.split_once('/') {
            let host = host_port.split(':').next().unwrap_or(host_port);
            return split_host_owner_repo(host, path);
        }
    }

    None
}

fn split_host_owner_repo(host: &str, path: &str) -> Option<RemoteRepo> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let repo = parts.last()?.to_string();
    let owner = parts[..parts.len() - 1].join("/");
    Some(RemoteRepo {
        host: host.to_ascii_lowercase(),
        owner,
        repo,
    })
}

/// Extracts candidate ticket keys (`PROJ-123`) excluding `TECHNICAL_PREFIX_DENYLIST`.
pub fn extract_ticket_keys(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for cap in TICKET_RE.captures_iter(text) {
        let prefix = &cap[1];
        if TECHNICAL_PREFIX_DENYLIST.contains(&prefix) {
            continue;
        }
        let full = cap[0].to_string();
        if seen.insert(full.clone()) {
            out.push(full);
        }
    }
    out
}

/// Extracts explicit Slack thread URLs from labels or PR descriptions.
pub fn extract_slack_urls(text: &str) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for cap in SLACK_URL_RE.captures_iter(text) {
        let full = cap[0].to_string();
        if seen.insert(full.clone()) {
            let workspace = cap[1].to_string();
            let channel = cap[2].to_string();
            let raw_ts = &cap[3];
            let formatted_ts = if raw_ts.len() > 6 {
                let split_at = raw_ts.len() - 6;
                format!("{}.{}", &raw_ts[..split_at], &raw_ts[split_at..])
            } else {
                raw_ts.to_string()
            };
            out.push((workspace, channel, formatted_ts, full));
        }
    }
    out
}

pub fn build_initial_agent_row(
    term: &HcTerminal,
    attention_state: AttentionState,
    blocked_children: Vec<String>,
    now_sec: i64,
) -> AgentRow {
    let agent_kind = classify_agent_kind(term);
    let workspace = resolve_git_workspace(&term.effective_cwd());
    let mut provenance = Vec::new();
    let mut context = CorrelatedContext::default();
    let mut tier = CorrelationTier::LocalOnly;

    // 1. Check explicit cockpit.* labels (Tier 1: Exact)
    if let Some(issue_lbl) = term
        .labels
        .get("cockpit.issue")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        context
            .candidate_issues
            .push(sanitize_terminal_text(issue_lbl));
        tier = CorrelationTier::Exact;
        provenance.push(ProvenanceEdge {
            source: "hc_label:cockpit.issue".into(),
            target: sanitize_terminal_text(issue_lbl),
            confidence: CorrelationTier::Exact,
            detail: "Explicit terminal label override".into(),
        });
    }

    if let Some(pr_lbl) = term
        .labels
        .get("cockpit.pr")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        tier = CorrelationTier::Exact;
        provenance.push(ProvenanceEdge {
            source: "hc_label:cockpit.pr".into(),
            target: sanitize_terminal_text(pr_lbl),
            confidence: CorrelationTier::Exact,
            detail: "Explicit PR label override".into(),
        });
    }

    if let Some(slack_lbl) = term
        .labels
        .get("cockpit.slack")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        tier = CorrelationTier::Exact;
        provenance.push(ProvenanceEdge {
            source: "hc_label:cockpit.slack".into(),
            target: sanitize_terminal_text(slack_lbl),
            confidence: CorrelationTier::Exact,
            detail: "Explicit Slack thread label".into(),
        });
    }

    // 2. Branch & title ticket extraction (Tier 2: Inferred) if no explicit issue label
    if context.candidate_issues.is_empty() {
        let mut inferred_keys = Vec::new();
        if let Some(branch) = &workspace.git_branch {
            for k in extract_ticket_keys(branch) {
                provenance.push(ProvenanceEdge {
                    source: "git_branch".into(),
                    target: k.clone(),
                    confidence: CorrelationTier::Inferred,
                    detail: format!("Extracted from branch '{branch}'"),
                });
                inferred_keys.push(k);
            }
        }
        if let Some(subj) = &workspace.commit_subject {
            for k in extract_ticket_keys(subj) {
                if !inferred_keys.contains(&k) {
                    provenance.push(ProvenanceEdge {
                        source: "commit_subject".into(),
                        target: k.clone(),
                        confidence: CorrelationTier::Inferred,
                        detail: "Extracted from HEAD commit subject".into(),
                    });
                    inferred_keys.push(k);
                }
            }
        }
        if let Some(name) = &term.name {
            for k in extract_ticket_keys(name) {
                if !inferred_keys.contains(&k) {
                    provenance.push(ProvenanceEdge {
                        source: "terminal_name".into(),
                        target: k.clone(),
                        confidence: CorrelationTier::Inferred,
                        detail: format!("Extracted from terminal name '{name}'"),
                    });
                    inferred_keys.push(k);
                }
            }
        }

        if inferred_keys.len() > 1 {
            let distinct_projects: BTreeSet<&str> = inferred_keys
                .iter()
                .filter_map(|k| k.split('-').next())
                .collect();
            if distinct_projects.len() > 1 && tier != CorrelationTier::Exact {
                tier = CorrelationTier::Ambiguous;
            } else if tier == CorrelationTier::LocalOnly {
                tier = CorrelationTier::Inferred;
            }
        } else if !inferred_keys.is_empty() && tier == CorrelationTier::LocalOnly {
            tier = CorrelationTier::Inferred;
        }
        context.candidate_issues = inferred_keys;
    }

    // 3. Non-default branch on remote repo qualifies for PR lookup
    if tier == CorrelationTier::LocalOnly {
        if let (Some(branch), Some(_remote)) = (&workspace.git_branch, &workspace.remote_repo) {
            if !matches!(branch.as_str(), "main" | "master" | "trunk" | "develop") {
                provenance.push(ProvenanceEdge {
                    source: "git_worktree".into(),
                    target: branch.clone(),
                    confidence: CorrelationTier::Inferred,
                    detail: "Active feature branch in git worktree".into(),
                });
            }
        }
    }

    AgentRow {
        address: sanitize_terminal_text(&term.address),
        name: sanitize_terminal_text(&term.display_name()),
        parent: term.parent.as_deref().map(sanitize_terminal_text),
        agent_kind,
        lifecycle: term.lifecycle(),
        attention_state,
        blocked_children,
        turn: term.turn,
        reported: term.reported,
        labels: term.labels.clone(),
        workspace,
        correlation_tier: tier,
        provenance,
        context,
        updated_at: term
            .status_ms
            .or(term.finished_ms)
            .or(term.created_ms)
            .map(|ms| ms / 1000)
            .unwrap_or(now_sec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ticket_extraction_denylist_filters_utf_sha_osc() {
        let branch = "feat/ENG-42-fix-UTF-8-and-SHA-256-and-OSC-7501-PR-99";
        let keys = extract_ticket_keys(branch);
        assert_eq!(keys, vec!["ENG-42".to_string()]);
    }

    #[test]
    fn test_normalize_remote_urls_ssh_and_https() {
        let ssh = normalize_remote_url("git@github.com:Earendil-Works/cockpit.git").unwrap();
        assert_eq!(ssh.host, "github.com");
        assert_eq!(ssh.owner, "Earendil-Works");
        assert_eq!(ssh.repo, "cockpit");

        let https = normalize_remote_url("https://gitlab.com/org/sub/service.git/").unwrap();
        assert_eq!(https.host, "gitlab.com");
        assert_eq!(https.owner, "org/sub");
        assert_eq!(https.repo, "service");
    }

    #[test]
    fn test_resolve_git_workspace_supports_linked_worktree_commondir() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("main-repo");
        let main_git = main_repo.join(".git");
        let wt_gitdir = main_git.join("worktrees/wt-1");
        let wt_dir = tmp.path().join("wt-1");

        fs::create_dir_all(&wt_gitdir).unwrap();
        fs::create_dir_all(&wt_dir).unwrap();

        // Write <commondir>/config and <commondir>/packed-refs in main_git
        fs::write(
            main_git.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:acme/platform.git\n",
        )
        .unwrap();
        fs::write(
            main_git.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\n0123456789abcdef0123456789abcdef01234567 refs/heads/feat/CORE-101-login\n",
        )
        .unwrap();

        // Write worktree .git pointer, HEAD, and commondir
        fs::write(
            wt_dir.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();
        fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
        fs::write(
            wt_gitdir.join("HEAD"),
            "ref: refs/heads/feat/CORE-101-login\n",
        )
        .unwrap();

        let ws = resolve_git_workspace(wt_dir.to_str().unwrap());
        assert_eq!(ws.git_branch.as_deref(), Some("feat/CORE-101-login"));
        assert_eq!(
            ws.git_head_sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        let rem = ws.remote_repo.unwrap();
        assert_eq!(rem.host, "github.com");
        assert_eq!(rem.owner, "acme");
        assert_eq!(rem.repo, "platform");
    }

    #[test]
    fn test_should_include_terminal_filters_idle_or_reported_plain_shells() {
        use crate::herdr::HcStatus;

        let reported_zsh = HcTerminal {
            address: "t1".into(),
            name: Some("zsh".into()),
            state: "live".into(),
            reported: true,
            argv: vec!["/bin/zsh".into()],
            status: Some(HcStatus {
                state: "idle".into(),
                app: Some("zsh".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!should_include_terminal(&reported_zsh, false));
        assert!(should_include_terminal(&reported_zsh, true));

        let working_non_shell_app = HcTerminal {
            address: "t2".into(),
            name: Some("zsh".into()),
            state: "live".into(),
            argv: vec!["/bin/zsh".into()],
            status: Some(HcStatus {
                state: "working".into(),
                app: Some("my-custom-agent".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(should_include_terminal(&working_non_shell_app, false));
    }

    #[test]
    fn test_should_include_terminal_filters_plain_shell_with_stale_agent_labels() {
        use crate::herdr::HcForeground;

        let mut labels = BTreeMap::new();
        labels.insert("cockpit.agent".into(), "pi".into());
        labels.insert("cockpit.state".into(), "working".into());
        let term = HcTerminal {
            address: "t7".into(),
            name: Some("π - cockpit".into()),
            state: "live".into(),
            foreground: Some(HcForeground {
                name: Some("zsh".into()),
                argv: vec!["/usr/bin/zsh".into()],
                ..Default::default()
            }),
            status: None,
            labels,
            ..Default::default()
        };
        assert_eq!(classify_agent_kind(&term), AgentKind::PlainShell);
        assert!(!should_include_terminal(&term, false));
        assert!(should_include_terminal(&term, true));
    }
}
