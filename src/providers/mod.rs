pub mod github;
pub mod gitlab;
pub mod jira;
pub mod linear;
pub mod runbooks;
pub mod slack;

use crate::config::CockpitConfig;
use crate::correlate::extract_slack_urls;
use crate::model::{
    AgentRow, CiSummary, CorrelationTier, DeployReleaseRef, IssueRef, ProvenanceEdge,
    ProviderHealth, ProviderId, PullRequestRef, ReviewSummary, RunbookRef, SlackThreadRef,
};
use crate::store::CacheStore;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;

pub const POSITIVE_CACHE_TTL_SEC: i64 = 60;
pub const NEGATIVE_CACHE_TTL_SEC: i64 = 900; // 15 minutes
pub const DEFAULT_429_COOLDOWN_SEC: i64 = 60;
pub const TOKEN_BUCKET_CAPACITY: f64 = 10.0;
pub const TOKEN_BUCKET_REFILL_PER_SEC: f64 = 0.5; // 1 req / 2s
pub const MAX_CLI_SUBPROCESSES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKey {
    RepoBranch {
        host: String,
        owner: String,
        repo: String,
        branch: String,
        head_sha: Option<String>,
    },
    PullRequest {
        host: String,
        owner: String,
        repo: String,
        number: u64,
        head_sha: Option<String>,
    },
    Issue {
        key: String,
    },
    SlackThread {
        workspace: String,
        channel_id: String,
        thread_ts: String,
        url: String,
    },
    RunbookContext {
        repo_root: Option<String>,
        cwd: String,
        branch: Option<String>,
        tickets: Vec<String>,
        failing_checks: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderErrorKind {
    AuthExpired,
    MissingScope,
    RateLimited { retry_after_sec: i64 },
    NotFound,
    BinaryMissing,
    NetworkOffline,
    SchemaError,
}

#[derive(Debug, Clone, Default)]
pub struct PrBundle {
    pub prs: Vec<PullRequestRef>,
    pub ci: Option<CiSummary>,
    pub review: Option<ReviewSummary>,
    pub deploy_release: Option<DeployReleaseRef>,
}

#[derive(Debug, Clone)]
pub struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new(TOKEN_BUCKET_CAPACITY, TOKEN_BUCKET_REFILL_PER_SEC)
    }
}

impl TokenBucket {
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            tokens: capacity,
            capacity,
            refill_per_sec,
            last_refill: Instant::now(),
        }
    }

    pub fn try_acquire(&mut self, cost: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

/// Returns `true` if remaining quota is below the 25% reserve floor (`remaining * 4 < limit_total`).
pub fn is_below_quota_reserve_floor(remaining: Option<i64>, limit_total: Option<i64>) -> bool {
    match (remaining, limit_total) {
        (Some(rem), Some(lim)) if lim > 0 => rem * 4 < lim,
        _ => false,
    }
}

/// Computes exponential backoff TTL (`30s -> 60s -> 180s -> 600s`) based on idle cycles without state change.
pub fn adaptive_refresh_ttl_sec(idle_cycles: u32) -> i64 {
    match idle_cycles {
        0 => 30,
        1 => 60,
        2 => 180,
        _ => 600,
    }
}

#[derive(Clone)]
pub struct ProviderCoordinator {
    buckets: Arc<Mutex<BTreeMap<ProviderId, TokenBucket>>>,
    cli_semaphore: Arc<Semaphore>,
    http_client: reqwest::Client,
}

impl Default for ProviderCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderCoordinator {
    pub fn new() -> Self {
        let mut map = BTreeMap::new();
        for p in ProviderId::ALL {
            map.insert(p, TokenBucket::default());
        }
        let http_client = reqwest::Client::builder()
            .user_agent(concat!("tchelvis/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self {
            buckets: Arc::new(Mutex::new(map)),
            cli_semaphore: Arc::new(Semaphore::new(MAX_CLI_SUBPROCESSES)),
            http_client,
        }
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    pub fn cli_semaphore(&self) -> &Arc<Semaphore> {
        &self.cli_semaphore
    }

    pub fn can_query_provider(
        &self,
        provider: ProviderId,
        store: &CacheStore,
        force_refresh: bool,
    ) -> bool {
        if provider == ProviderId::Runbooks {
            return true;
        }
        let now_sec = chrono::Utc::now().timestamp();
        if let Ok(Some(q)) = store.get_quota(provider) {
            if let Some(cd) = q.cooldown_until {
                if cd > now_sec {
                    return false;
                }
            }
            if !force_refresh && is_below_quota_reserve_floor(q.remaining, q.limit_total) {
                return false;
            }
        }
        let mut buckets = self.buckets.lock().expect("bucket mutex");
        buckets.entry(provider).or_default().try_acquire(1.0)
    }

    pub async fn check_all_health(
        &self,
        cfg: &CockpitConfig,
        store: &CacheStore,
        home_dir: &std::path::Path,
    ) -> BTreeMap<ProviderId, ProviderHealth> {
        let mut out = BTreeMap::new();
        let gh = github::check_health(self, cfg, store).await;
        out.insert(ProviderId::Github, store.apply_quota_to_health(gh));

        let gl = gitlab::check_health(self, cfg, store).await;
        out.insert(ProviderId::Gitlab, store.apply_quota_to_health(gl));

        let jr = jira::check_health(self, cfg, store).await;
        out.insert(ProviderId::Jira, store.apply_quota_to_health(jr));

        let ln = linear::check_health(self, cfg, store).await;
        out.insert(ProviderId::Linear, store.apply_quota_to_health(ln));

        let sl = slack::check_health(self, cfg, store).await;
        out.insert(ProviderId::Slack, store.apply_quota_to_health(sl));

        let rb = runbooks::check_health(home_dir);
        out.insert(ProviderId::Runbooks, rb);
        out
    }

    /// Enriches `agents` with deduplicated external context and local runbooks.
    pub async fn enrich_agents(
        &self,
        agents: &mut [AgentRow],
        cfg: &CockpitConfig,
        store: &CacheStore,
        home_dir: &std::path::Path,
        force_refresh: bool,
    ) {
        // 1. Collect deduplicated keys for Live agents (Finished agents only read cached state, freezing external polling)
        let mut gh_keys: BTreeSet<(String, String, String, String, Option<String>)> =
            BTreeSet::new();
        let mut gl_keys: BTreeSet<(String, String, String, String, Option<String>)> =
            BTreeSet::new();
        let mut issue_keys: BTreeSet<String> = BTreeSet::new();
        let mut slack_keys: BTreeSet<(String, String, String, String)> = BTreeSet::new();

        for row in agents.iter() {
            let is_live = row.lifecycle.is_live();
            for k in &row.context.candidate_issues {
                if is_live || force_refresh {
                    issue_keys.insert(k.clone());
                }
            }
            if let Some(slack_lbl) = row.labels.get("cockpit.slack") {
                for s in extract_slack_urls(slack_lbl) {
                    slack_keys.insert(s);
                }
            }
            if let (Some(rem), Some(branch)) =
                (&row.workspace.remote_repo, &row.workspace.git_branch)
            {
                if matches!(branch.as_str(), "main" | "master" | "trunk")
                    && !row.labels.contains_key("cockpit.pr")
                {
                    continue;
                }
                if is_live || force_refresh {
                    let tuple = (
                        rem.host.clone(),
                        rem.owner.clone(),
                        rem.repo.clone(),
                        branch.clone(),
                        row.workspace.git_head_sha.clone(),
                    );
                    if rem.host.contains("gitlab") {
                        gl_keys.insert(tuple);
                    } else {
                        gh_keys.insert(tuple);
                    }
                }
            }
        }

        // 2. Fetch GitHub PR bundles
        let mut gh_results: BTreeMap<(String, String, String), PrBundle> = BTreeMap::new();
        for (host, owner, repo, branch, sha) in gh_keys {
            if let Some(bundle) = github::fetch_pr_bundle(
                self,
                cfg,
                store,
                &host,
                &owner,
                &repo,
                &branch,
                sha.as_deref(),
                force_refresh,
            )
            .await
            {
                for pr in &bundle.prs {
                    if let Some(body) = &pr.body_snippet {
                        for s in extract_slack_urls(body) {
                            slack_keys.insert(s);
                        }
                    }
                }
                gh_results.insert((owner, repo, branch), bundle);
            }
        }

        // 3. Fetch GitLab MR bundles
        let mut gl_results: BTreeMap<(String, String, String), PrBundle> = BTreeMap::new();
        for (host, owner, repo, branch, sha) in gl_keys {
            if let Some(bundle) = gitlab::fetch_mr_bundle(
                self,
                cfg,
                store,
                &host,
                &owner,
                &repo,
                &branch,
                sha.as_deref(),
                force_refresh,
            )
            .await
            {
                gl_results.insert((owner, repo, branch), bundle);
            }
        }

        // 4. Fetch Issues (Jira / Linear)
        let mut resolved_issues: BTreeMap<String, IssueRef> = BTreeMap::new();
        for key in issue_keys {
            if let Some(iss) = jira::fetch_issue(self, cfg, store, &key, force_refresh).await {
                resolved_issues.insert(key, iss);
            } else if let Some(iss) =
                linear::fetch_issue(self, cfg, store, &key, force_refresh).await
            {
                resolved_issues.insert(key, iss);
            }
        }

        // 5. Fetch Slack threads
        let mut resolved_slack: BTreeMap<String, SlackThreadRef> = BTreeMap::new();
        for (ws, ch, ts, url) in slack_keys {
            let thread =
                slack::fetch_thread(self, cfg, store, &ws, &ch, &ts, &url, force_refresh).await;
            resolved_slack.insert(url, thread);
        }

        // 6. Attach results + discover local runbooks per agent row
        for row in agents.iter_mut() {
            if let (Some(rem), Some(branch)) =
                (&row.workspace.remote_repo, &row.workspace.git_branch)
            {
                let key = (rem.owner.clone(), rem.repo.clone(), branch.clone());
                let maybe_bundle = gh_results.get(&key).or_else(|| gl_results.get(&key));
                if let Some(bundle) = maybe_bundle {
                    if bundle.prs.len() == 1 {
                        let pr = bundle.prs[0].clone();
                        row.provenance.push(ProvenanceEdge {
                            source: "provider_pr".into(),
                            target: format!("#{}", pr.number),
                            confidence: CorrelationTier::Exact,
                            detail: format!(
                                "Unique PR matching {}/{}:{}",
                                rem.owner, rem.repo, branch
                            ),
                        });
                        row.context.pull_request = Some(pr);
                        row.correlation_tier = CorrelationTier::Exact;
                    } else if bundle.prs.len() > 1 {
                        row.context.candidate_prs = bundle.prs.clone();
                        row.correlation_tier = CorrelationTier::Ambiguous;
                    }
                    row.context.ci = bundle.ci.clone();
                    row.context.review = bundle.review.clone();
                    row.context.deploy_release = bundle.deploy_release.clone();
                }
            }

            if let Some(first_key) = row.context.candidate_issues.first() {
                if let Some(iss) = resolved_issues.get(first_key) {
                    row.context.issue = Some(iss.clone());
                }
            }

            if let Some(slack_lbl) = row.labels.get("cockpit.slack") {
                for (_, _, _, url) in extract_slack_urls(slack_lbl) {
                    if let Some(st) = resolved_slack.get(&url) {
                        row.context.slack_threads.push(st.clone());
                    }
                }
            }
            if let Some(pr) = &row.context.pull_request {
                if let Some(body) = &pr.body_snippet {
                    for (_, _, _, url) in extract_slack_urls(body) {
                        if let Some(st) = resolved_slack.get(&url) {
                            if !row.context.slack_threads.iter().any(|x| x.url == st.url) {
                                row.context.slack_threads.push(st.clone());
                            }
                        }
                    }
                }
            }

            let failing_checks = row
                .context
                .ci
                .as_ref()
                .map(|c| c.failing_checks.clone())
                .unwrap_or_default();

            let matched_rbs: Vec<RunbookRef> = runbooks::discover_and_match_runbooks(
                row.workspace.repo_root.as_deref(),
                &row.workspace.cwd,
                row.workspace.git_branch.as_deref(),
                &row.context.candidate_issues,
                &failing_checks,
                row.labels.get("cockpit.runbook").map(|s| s.as_str()),
                home_dir,
            );
            if !matched_rbs.is_empty() && row.correlation_tier == CorrelationTier::LocalOnly {
                row.correlation_tier = CorrelationTier::Inferred;
            }
            row.context.runbooks = matched_rbs;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quota_reserve_floor_and_token_bucket() {
        assert!(is_below_quota_reserve_floor(Some(1200), Some(5000))); // 24% < 25%
        assert!(!is_below_quota_reserve_floor(Some(1300), Some(5000))); // 26% >= 25%

        let mut bucket = TokenBucket::new(2.0, 0.5);
        assert!(bucket.try_acquire(1.0));
        assert!(bucket.try_acquire(1.0));
        assert!(!bucket.try_acquire(1.0));
    }
}
