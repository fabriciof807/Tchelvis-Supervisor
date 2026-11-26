use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const MAX_DOC_SNAPSHOT_BYTES: usize = 262_144;
pub const MAX_DOC_FINISHED_AGENTS: usize = 50;

static ANSI_CSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid CSI regex"));
static ANSI_OSC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").expect("valid OSC regex"));

/// Strips ANSI CSI sequences, OSC sequences (including OSC 7501), and C0/C1 control characters
/// (preserving `\t` and `\n` only when `allow_newlines` is false -> replaced with space).
pub fn sanitize_terminal_text(s: &str) -> String {
    let without_osc = ANSI_OSC_RE.replace_all(s, "");
    let without_csi = ANSI_CSI_RE.replace_all(&without_osc, "");
    let mut out = String::with_capacity(without_csi.len());
    for ch in without_csi.chars() {
        let code = ch as u32;
        if ch == '\t' {
            out.push(' ');
        } else if code <= 0x1F || (0x7F..=0x9F).contains(&code) {
            // Strip C0 and C1 control characters
            continue;
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    #[default]
    Github,
    Gitlab,
    Jira,
    Linear,
    Slack,
    Runbooks,
}

impl ProviderId {
    pub const ALL: [ProviderId; 6] = [
        ProviderId::Github,
        ProviderId::Gitlab,
        ProviderId::Jira,
        ProviderId::Linear,
        ProviderId::Slack,
        ProviderId::Runbooks,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderId::Github => "github",
            ProviderId::Gitlab => "gitlab",
            ProviderId::Jira => "jira",
            ProviderId::Linear => "linear",
            ProviderId::Slack => "slack",
            ProviderId::Runbooks => "runbooks",
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Ready,
    #[default]
    Inactive,
    AuthExpired,
    MissingScope,
    RateLimited,
    Offline,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderHealth {
    pub provider: ProviderId,
    pub status: ProviderStatus,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub quota_remaining: Option<i64>,
    #[serde(default)]
    pub quota_limit: Option<i64>,
    #[serde(default)]
    pub quota_reset_at: Option<i64>,
    #[serde(default)]
    pub cooldown_until: Option<i64>,
    #[serde(default)]
    pub last_success_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentLifecycle {
    #[default]
    Live,
    Finished {
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        finished_ms: Option<i64>,
        #[serde(default)]
        reason: Option<String>,
    },
}

impl AgentLifecycle {
    pub fn is_live(&self) -> bool {
        matches!(self, AgentLifecycle::Live)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    OpenCode,
    Pi,
    Codex,
    Antigravity,
    CustomAgent,
    #[default]
    PlainShell,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude",
            AgentKind::OpenCode => "opencode",
            AgentKind::Pi => "pi",
            AgentKind::Codex => "codex",
            AgentKind::Antigravity => "antigravity",
            AgentKind::CustomAgent => "custom",
            AgentKind::PlainShell => "shell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    Auth,
    Permission,
    #[default]
    Question,
    #[serde(other)]
    Unknown,
}

impl BlockedKind {
    pub fn priority_rank(&self) -> u8 {
        match self {
            BlockedKind::Auth => 0,
            BlockedKind::Permission => 1,
            BlockedKind::Question => 2,
            BlockedKind::Unknown => 3,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            BlockedKind::Auth => "auth",
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttentionState {
    Blocked {
        #[serde(default)]
        kind: BlockedKind,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        since_ms: i64,
    },
    Working {
        #[serde(default)]
        message: Option<String>,
    },
    Done {
        #[serde(default)]
        message: Option<String>,
    },
    #[default]
    Idle,
    Error {
        #[serde(default)]
        message: Option<String>,
    },
}

impl AttentionState {
    pub fn sort_bucket(&self, lifecycle: &AgentLifecycle) -> u8 {
        if !lifecycle.is_live() {
            return 5;
        }
        match self {
            AttentionState::Blocked { .. } => 0,
            AttentionState::Error { .. } => 1,
            AttentionState::Working { .. } => 2,
            AttentionState::Done { .. } => 3,
            AttentionState::Idle => 4,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AttentionState::Blocked { .. } => "blocked",
            AttentionState::Working { .. } => "working",
            AttentionState::Done { .. } => "done",
            AttentionState::Idle => "idle",
            AttentionState::Error { .. } => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CorrelationTier {
    Exact,
    Inferred,
    Ambiguous,
    #[default]
    LocalOnly,
}

impl CorrelationTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            CorrelationTier::Exact => "exact",
            CorrelationTier::Inferred => "inferred",
            CorrelationTier::Ambiguous => "ambiguous",
            CorrelationTier::LocalOnly => "local_only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceEdge {
    pub source: String,
    pub target: String,
    pub confidence: CorrelationTier,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RemoteRepo {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl RemoteRepo {
    pub fn canonical_slug(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.repo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceInfo {
    pub cwd: String,
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub git_head_sha: Option<String>,
    #[serde(default)]
    pub commit_subject: Option<String>,
    #[serde(default)]
    pub remote_repo: Option<RemoteRepo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiCheckItem {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CiSummary {
    pub overall_state: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    #[serde(default)]
    pub failing_checks: Vec<String>,
    #[serde(default)]
    pub checks: Vec<CiCheckItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewSummary {
    pub decision: String,
    pub approved_count: usize,
    pub changes_requested_count: usize,
    pub commented_count: usize,
    #[serde(default)]
    pub reviewers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployReleaseRef {
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub deploy_state: Option<String>,
    #[serde(default)]
    pub latest_release_tag: Option<String>,
    #[serde(default)]
    pub release_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub provider: ProviderId,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub is_draft: bool,
    pub url: String,
    pub head_branch: String,
    #[serde(default)]
    pub head_sha: Option<String>,
    #[serde(default)]
    pub local_ahead: bool,
    #[serde(default)]
    pub body_snippet: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    pub provider: ProviderId,
    pub key: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackThreadRef {
    pub workspace: String,
    pub channel_id: String,
    pub thread_ts: String,
    pub url: String,
    #[serde(default)]
    pub title_snippet: Option<String>,
    #[serde(default)]
    pub reply_count: Option<u32>,
    #[serde(default)]
    pub latest_reply_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookRef {
    pub path: String,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub matched_reason: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub ci_job: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CorrelatedContext {
    #[serde(default)]
    pub pull_request: Option<PullRequestRef>,
    #[serde(default)]
    pub candidate_prs: Vec<PullRequestRef>,
    #[serde(default)]
    pub ci: Option<CiSummary>,
    #[serde(default)]
    pub review: Option<ReviewSummary>,
    #[serde(default)]
    pub deploy_release: Option<DeployReleaseRef>,
    #[serde(default)]
    pub issue: Option<IssueRef>,
    #[serde(default)]
    pub candidate_issues: Vec<String>,
    #[serde(default)]
    pub slack_threads: Vec<SlackThreadRef>,
    #[serde(default)]
    pub runbooks: Vec<RunbookRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopProgress {
    pub status: String,
    pub cycle: u32,
    pub max_cycles: u32,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

impl LoopProgress {
    /// The card title of a loop child: `worker: pi · 1/3`, `loop · 2/3` or `PASS · 2/3`.
    pub fn short_label(&self) -> String {
        let cycle = format!("{}/{}", self.cycle, self.max_cycles);
        if self.status != "RUNNING" {
            format!("{} · {cycle}", self.status)
        } else {
            match (self.role.as_deref(), self.provider.as_deref()) {
                (Some(role), Some(provider)) => format!("{role}: {provider} · {cycle}"),
                _ => format!("loop · {cycle}"),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentRow {
    pub address: String,
    pub name: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub agent_kind: AgentKind,
    pub lifecycle: AgentLifecycle,
    pub attention_state: AttentionState,
    #[serde(default)]
    pub prompt_destructive: bool,
    #[serde(default)]
    pub blocked_children: Vec<String>,
    #[serde(default)]
    pub active_children: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_progress: Option<LoopProgress>,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub reported: bool,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub workspace: WorkspaceInfo,
    pub correlation_tier: CorrelationTier,
    #[serde(default)]
    pub provenance: Vec<ProvenanceEdge>,
    #[serde(default)]
    pub context: CorrelatedContext,
    pub updated_at: i64,
}

impl AgentRow {
    pub fn sort_for_attention_queue(rows: &mut [AgentRow]) {
        rows.sort_by(|a, b| {
            let bucket_a = a.attention_state.sort_bucket(&a.lifecycle);
            let bucket_b = b.attention_state.sort_bucket(&b.lifecycle);
            if bucket_a != bucket_b {
                return bucket_a.cmp(&bucket_b);
            }
            if let (
                AttentionState::Blocked {
                    kind: ka,
                    since_ms: sa,
                    ..
                },
                AttentionState::Blocked {
                    kind: kb,
                    since_ms: sb,
                    ..
                },
            ) = (&a.attention_state, &b.attention_state)
            {
                let rank_cmp = ka.priority_rank().cmp(&kb.priority_rank());
                if rank_cmp != std::cmp::Ordering::Equal {
                    return rank_cmp;
                }
                return sa.cmp(sb);
            }
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.address.cmp(&b.address))
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FleetSummary {
    pub total_live: usize,
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
    pub error: usize,
    pub recent_finished: usize,
}

impl FleetSummary {
    pub fn from_agents(agents: &[AgentRow]) -> Self {
        let mut s = FleetSummary::default();
        for a in agents {
            if !a.lifecycle.is_live() {
                s.recent_finished += 1;
                continue;
            }
            s.total_live += 1;
            match a.attention_state {
                AttentionState::Blocked { .. } => s.blocked += 1,
                AttentionState::Working { .. } => s.working += 1,
                AttentionState::Done { .. } => s.done += 1,
                AttentionState::Idle => s.idle += 1,
                AttentionState::Error { .. } => s.error += 1,
            }
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CockpitSnapshot {
    pub schema_version: u32,
    pub cockpit_version: String,
    pub generated_at: i64,
    pub published_at: i64,
    #[serde(default)]
    pub daemon_pid: Option<u32>,
    pub herdr_connected: bool,
    pub summary: FleetSummary,
    pub agents: Vec<AgentRow>,
    pub providers: BTreeMap<ProviderId, ProviderHealth>,
}

impl Default for CockpitSnapshot {
    fn default() -> Self {
        Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            cockpit_version: env!("CARGO_PKG_VERSION").to_string(),
            generated_at: 0,
            published_at: 0,
            daemon_pid: None,
            herdr_connected: false,
            summary: FleetSummary::default(),
            agents: Vec::new(),
            providers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummaryRow {
    pub address: String,
    pub name: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub agent_kind: AgentKind,
    pub lifecycle: AgentLifecycle,
    pub attention_state: AttentionState,
    #[serde(default)]
    pub prompt_destructive: bool,
    #[serde(default)]
    pub blocked_children: Vec<String>,
    #[serde(default)]
    pub active_children: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub git_branch: Option<String>,
    pub correlation_tier: CorrelationTier,
    #[serde(default)]
    pub ticket_key: Option<String>,
    #[serde(default)]
    pub ticket_status: Option<String>,
    #[serde(default)]
    pub pr_number: Option<u64>,
    #[serde(default)]
    pub pr_state: Option<String>,
    #[serde(default)]
    pub ci_state: Option<String>,
    #[serde(default)]
    pub review_state: Option<String>,
    pub runbook_count: usize,
    pub updated_at: i64,
}

impl AgentSummaryRow {
    pub fn from_agent_row(row: &AgentRow) -> Self {
        Self {
            address: row.address.clone(),
            name: row.name.clone(),
            parent: row.parent.clone(),
            agent_kind: row.agent_kind,
            lifecycle: row.lifecycle.clone(),
            attention_state: row.attention_state.clone(),
            prompt_destructive: row.prompt_destructive,
            blocked_children: row.blocked_children.clone(),
            active_children: row.active_children.clone(),
            cwd: row.workspace.cwd.clone(),
            git_branch: row.workspace.git_branch.clone(),
            correlation_tier: row.correlation_tier,
            ticket_key: row
                .context
                .issue
                .as_ref()
                .map(|i| i.key.clone())
                .or_else(|| row.context.candidate_issues.first().cloned()),
            ticket_status: row.context.issue.as_ref().map(|i| i.status.clone()),
            pr_number: row.context.pull_request.as_ref().map(|p| p.number),
            pr_state: row.context.pull_request.as_ref().map(|p| p.state.clone()),
            ci_state: row.context.ci.as_ref().map(|c| c.overall_state.clone()),
            review_state: row.context.review.as_ref().map(|r| r.decision.clone()),
            runbook_count: row.context.runbooks.len(),
            updated_at: row.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSummaryProjection {
    pub schema_version: u32,
    pub cockpit_version: String,
    pub published_at: i64,
    #[serde(default)]
    pub daemon_pid: Option<u32>,
    pub herdr_connected: bool,
    pub summary: FleetSummary,
    pub agents: Vec<AgentSummaryRow>,
    pub providers: BTreeMap<ProviderId, ProviderHealth>,
}

impl FleetSummaryProjection {
    pub fn from_snapshot(snap: &CockpitSnapshot) -> Self {
        let mut live_rows = Vec::new();
        let mut finished_rows = Vec::new();
        for row in &snap.agents {
            let summary_row = AgentSummaryRow::from_agent_row(row);
            if row.lifecycle.is_live() {
                live_rows.push(summary_row);
            } else if finished_rows.len() < MAX_DOC_FINISHED_AGENTS {
                finished_rows.push(summary_row);
            }
        }

        let mut agents = Vec::with_capacity(live_rows.len() + finished_rows.len());
        agents.extend(live_rows);
        agents.extend(finished_rows);

        let mut proj = Self {
            schema_version: snap.schema_version,
            cockpit_version: snap.cockpit_version.clone(),
            published_at: snap.published_at,
            daemon_pid: snap.daemon_pid,
            herdr_connected: snap.herdr_connected,
            summary: snap.summary.clone(),
            agents,
            providers: snap.providers.clone(),
        };

        // Enforce <256 KiB cap: drop oldest finished rows first, then truncate messages
        while serde_json::to_vec(&proj)
            .map(|v| v.len() > MAX_DOC_SNAPSHOT_BYTES)
            .unwrap_or(false)
        {
            if let Some(pos) = proj.agents.iter().rposition(|a| !a.lifecycle.is_live()) {
                proj.agents.remove(pos);
                continue;
            }
            // Truncate long messages on live rows
            let mut truncated_any = false;
            for agent in &mut proj.agents {
                if let AttentionState::Blocked {
                    message: Some(msg), ..
                } = &mut agent.attention_state
                {
                    if msg.len() > 120 {
                        msg.truncate(120);
                        truncated_any = true;
                    }
                }
            }
            if !truncated_any && proj.agents.len() > 1 {
                proj.agents.pop();
            } else if !truncated_any {
                break;
            }
        }

        proj
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcOp {
    Status,
    Watch,
    Inspect,
    Refresh,
    #[serde(rename = "orch_wake")]
    OrchWake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcRequest {
    pub op: IpcOp,
    #[serde(default)]
    pub terminal: Option<String>,
    #[serde(default)]
    pub include_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectResponse {
    pub agent: AgentRow,
    pub providers: BTreeMap<ProviderId, ProviderHealth>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_progress_short_label() {
        let p = |status: &str, cycle, role: Option<&str>, provider: Option<&str>| LoopProgress {
            status: status.to_string(),
            cycle,
            max_cycles: 3,
            role: role.map(str::to_string),
            provider: provider.map(str::to_string),
        };
        assert_eq!(
            p("RUNNING", 1, Some("worker"), Some("pi")).short_label(),
            "worker: pi · 1/3"
        );
        assert_eq!(p("RUNNING", 2, None, None).short_label(), "loop · 2/3");
        assert_eq!(p("PASS", 2, None, None).short_label(), "PASS · 2/3");
        assert_eq!(
            p("ESCALATE", 3, Some("worker"), Some("pi")).short_label(),
            "ESCALATE · 3/3"
        );
    }

    #[test]
    fn agent_row_without_loop_progress_serializes_as_before() {
        let json = serde_json::to_string(&AgentRow::default()).unwrap();
        assert!(!json.contains("loop_progress"), "{json}");
        let back: AgentRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.loop_progress, None);
    }

    #[test]
    fn test_sanitize_terminal_text_strips_osc_and_csi() {
        let dirty = "Fix bug \x1b]7501;state=blocked:kind=permission\x07 and \x1b[31mred\x1b[0m\x00\x08 text";
        assert_eq!(sanitize_terminal_text(dirty), "Fix bug  and red text");
    }

    #[test]
    fn test_attention_queue_sorting_pins_blocked_by_priority() {
        let mut rows = vec![
            AgentRow {
                address: "t1".into(),
                name: "worker".into(),
                attention_state: AttentionState::Working { message: None },
                updated_at: 100,
                ..Default::default()
            },
            AgentRow {
                address: "t2".into(),
                name: "blocked-q".into(),
                attention_state: AttentionState::Blocked {
                    kind: BlockedKind::Question,
                    message: Some("Which DB?".into()),
                    since_ms: 50,
                },
                updated_at: 90,
                ..Default::default()
            },
            AgentRow {
                address: "t3".into(),
                name: "blocked-auth".into(),
                attention_state: AttentionState::Blocked {
                    kind: BlockedKind::Auth,
                    message: Some("Need SSO".into()),
                    since_ms: 80,
                },
                updated_at: 80,
                ..Default::default()
            },
        ];
        AgentRow::sort_for_attention_queue(&mut rows);
        assert_eq!(rows[0].address, "t3");
        assert_eq!(rows[1].address, "t2");
        assert_eq!(rows[2].address, "t1");
    }

    #[test]
    fn test_fleet_summary_projection_stays_under_256kib() {
        let mut snap = CockpitSnapshot::default();
        for i in 0..80 {
            snap.agents.push(AgentRow {
                address: format!("t{i}"),
                name: format!("agent-{i}"),
                lifecycle: AgentLifecycle::Finished {
                    exit_code: Some(0),
                    finished_ms: Some(1000 + i),
                    reason: None,
                },
                ..Default::default()
            });
        }
        let proj = FleetSummaryProjection::from_snapshot(&snap);
        assert!(proj.agents.len() <= MAX_DOC_FINISHED_AGENTS);
        let bytes = serde_json::to_vec(&proj).unwrap();
        assert!(bytes.len() <= MAX_DOC_SNAPSHOT_BYTES);
    }
}
