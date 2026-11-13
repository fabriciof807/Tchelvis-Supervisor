use crate::config::CockpitConfig;
use crate::model::{
    sanitize_terminal_text, CiCheckItem, CiSummary, DeployReleaseRef, ProviderHealth, ProviderId,
    ProviderStatus, PullRequestRef, ReviewSummary,
};
use crate::providers::{
    PrBundle, ProviderCoordinator, DEFAULT_429_COOLDOWN_SEC, POSITIVE_CACHE_TTL_SEC,
};
use crate::store::{CacheStore, CachedEntity, QuotaRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedGithubBundle {
    prs: Vec<PullRequestRef>,
    ci: Option<CiSummary>,
    review: Option<ReviewSummary>,
    deploy_release: Option<DeployReleaseRef>,
}

pub async fn check_health(
    _coord: &ProviderCoordinator,
    cfg: &CockpitConfig,
    store: &CacheStore,
) -> ProviderHealth {
    let token = cfg.resolve_token(ProviderId::Github).await;
    if token.is_none() {
        return ProviderHealth {
            provider: ProviderId::Github,
            status: ProviderStatus::Inactive,
            detail: Some("No GITHUB_TOKEN or authenticated `gh` session".into()),
            ..Default::default()
        };
    }

    let mut health = ProviderHealth {
        provider: ProviderId::Github,
        status: ProviderStatus::Ready,
        detail: Some("Authenticated via token/gh".into()),
        ..Default::default()
    };
    health = store.apply_quota_to_health(health);
    health
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_pr_bundle(
    coord: &ProviderCoordinator,
    cfg: &CockpitConfig,
    store: &CacheStore,
    host: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    local_sha: Option<&str>,
    force_refresh: bool,
) -> Option<PrBundle> {
    let cache_key = format!("gh:{host}/{owner}/{repo}:{branch}");
    let now_sec = chrono::Utc::now().timestamp();

    let cached = store
        .get_cached(ProviderId::Github, &cache_key)
        .ok()
        .flatten();
    if let Some(ref ent) = cached {
        if !force_refresh && ent.expires_at > now_sec {
            if ent.is_negative {
                return None;
            }
            if let Ok(parsed) = serde_json::from_str::<CachedGithubBundle>(&ent.payload_json) {
                return Some(PrBundle {
                    prs: parsed.prs,
                    ci: parsed.ci,
                    review: parsed.review,
                    deploy_release: parsed.deploy_release,
                });
            }
        }
    }

    let token = cfg.resolve_token(ProviderId::Github).await?;
    if !coord.can_query_provider(ProviderId::Github, store, force_refresh) {
        if let Some(ent) = cached {
            if let Ok(parsed) = serde_json::from_str::<CachedGithubBundle>(&ent.payload_json) {
                return Some(PrBundle {
                    prs: parsed.prs,
                    ci: parsed.ci,
                    review: parsed.review,
                    deploy_release: parsed.deploy_release,
                });
            }
        }
        return None;
    }

    let api_base = if host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    };

    let pr_url =
        format!("{api_base}/repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=all&per_page=5");
    let mut req = coord
        .http_client()
        .get(&pr_url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28");

    if let Some(ref ent) = cached {
        if let Some(ref etag) = ent.etag {
            req = req.header("If-None-Match", etag);
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(_) => {
            // Fallback to gh api subprocess
            return fetch_via_gh_cli(coord, owner, repo, branch, local_sha).await;
        }
    };

    record_github_headers(store, resp.headers(), resp.status().as_u16(), now_sec);

    if resp.status().as_u16() == 304 {
        if let Some(ent) = cached {
            let _ = store.touch_cached_expiry(
                ProviderId::Github,
                &cache_key,
                now_sec,
                now_sec + POSITIVE_CACHE_TTL_SEC,
            );
            if let Ok(parsed) = serde_json::from_str::<CachedGithubBundle>(&ent.payload_json) {
                return Some(PrBundle {
                    prs: parsed.prs,
                    ci: parsed.ci,
                    review: parsed.review,
                    deploy_release: parsed.deploy_release,
                });
            }
        }
        return None;
    }

    if !resp.status().is_success() {
        return None;
    }

    let new_etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let items: Vec<Value> = resp.json().await.ok()?;
    let prs = parse_github_pulls(&items, local_sha);

    let mut ci = None;
    let mut review = None;
    let mut deploy_release = None;

    if let Some(first_pr) = prs.first() {
        let sha_to_check = first_pr.head_sha.as_deref().or(local_sha);
        if let Some(sha) = sha_to_check {
            let checks_url =
                format!("{api_base}/repos/{owner}/{repo}/commits/{sha}/check-runs?per_page=50");
            if let Ok(c_resp) = coord
                .http_client()
                .get(&checks_url)
                .header("Accept", "application/vnd.github+json")
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
            {
                if c_resp.status().is_success() {
                    if let Ok(c_json) = c_resp.json::<Value>().await {
                        ci = Some(parse_github_check_runs(&c_json));
                    }
                }
            }
        }

        let rev_url = format!(
            "{api_base}/repos/{owner}/{repo}/pulls/{}/reviews?per_page=50",
            first_pr.number
        );
        if let Ok(r_resp) = coord
            .http_client()
            .get(&rev_url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
        {
            if r_resp.status().is_success() {
                if let Ok(r_json) = r_resp.json::<Vec<Value>>().await {
                    review = Some(parse_github_reviews(&r_json));
                }
            }
        }

        let rel_url = format!("{api_base}/repos/{owner}/{repo}/releases/latest");
        if let Ok(rel_resp) = coord
            .http_client()
            .get(&rel_url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
        {
            if rel_resp.status().is_success() {
                if let Ok(rel_json) = rel_resp.json::<Value>().await {
                    deploy_release = Some(DeployReleaseRef {
                        environment: None,
                        deploy_state: None,
                        latest_release_tag: rel_json
                            .get("tag_name")
                            .and_then(|v| v.as_str())
                            .map(sanitize_terminal_text),
                        release_url: rel_json
                            .get("html_url")
                            .and_then(|v| v.as_str())
                            .map(sanitize_terminal_text),
                    });
                }
            }
        }
    }

    let bundle_struct = CachedGithubBundle {
        prs: prs.clone(),
        ci: ci.clone(),
        review: review.clone(),
        deploy_release: deploy_release.clone(),
    };
    if let Ok(payload_json) = serde_json::to_string(&bundle_struct) {
        let _ = store.put_cached(&CachedEntity {
            provider: ProviderId::Github.as_str().into(),
            resource_key: cache_key,
            payload_json,
            etag: new_etag,
            fetched_at: now_sec,
            expires_at: now_sec + POSITIVE_CACHE_TTL_SEC,
            is_negative: false,
        });
    }

    Some(PrBundle {
        prs,
        ci,
        review,
        deploy_release,
    })
}

pub fn parse_github_pulls(items: &[Value], local_sha: Option<&str>) -> Vec<PullRequestRef> {
    let mut out = Vec::new();
    for item in items {
        let Some(number) = item.get("number").and_then(|v| v.as_u64()) else {
            continue;
        };
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_default();
        let state = item
            .get("state")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_else(|| "open".into());
        let is_draft = item.get("draft").and_then(|v| v.as_bool()).unwrap_or(false);
        let url = item
            .get("html_url")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_default();
        let head = item.get("head");
        let head_branch = head
            .and_then(|h| h.get("ref"))
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_default();
        let head_sha = head
            .and_then(|h| h.get("sha"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let local_ahead = match (local_sha, head_sha.as_deref()) {
            (Some(l), Some(r)) => l != r,
            _ => false,
        };
        let body_snippet = item
            .get("body")
            .and_then(|v| v.as_str())
            .map(|s| sanitize_terminal_text(&s.chars().take(500).collect::<String>()));

        out.push(PullRequestRef {
            provider: ProviderId::Github,
            number,
            title,
            state,
            is_draft,
            url,
            head_branch,
            head_sha,
            local_ahead,
            body_snippet,
        });
    }
    out
}

pub fn parse_github_check_runs(val: &Value) -> CiSummary {
    let runs = val
        .get("check_runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut pending = 0usize;
    let mut failing_checks = Vec::new();
    let mut checks = Vec::new();

    for r in &runs {
        let name = r
            .get("name")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_else(|| "check".into());
        let status = r
            .get("status")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_else(|| "queued".into());
        let conclusion = r
            .get("conclusion")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text);
        let details_url = r
            .get("html_url")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text);

        if status != "completed" {
            pending += 1;
        } else {
            match conclusion.as_deref() {
                Some("success" | "neutral" | "skipped") => passed += 1,
                Some(_) => {
                    failed += 1;
                    failing_checks.push(name.clone());
                }
                None => pending += 1,
            }
        }
        checks.push(CiCheckItem {
            name,
            status,
            conclusion,
            details_url,
        });
    }

    let overall_state = if failed > 0 {
        "failing"
    } else if pending > 0 {
        "pending"
    } else if passed > 0 {
        "passing"
    } else {
        "none"
    }
    .to_string();

    CiSummary {
        overall_state,
        total: checks.len(),
        passed,
        failed,
        pending,
        failing_checks,
        checks,
    }
}

pub fn parse_github_reviews(items: &[Value]) -> ReviewSummary {
    let mut approved = 0usize;
    let mut changes_requested = 0usize;
    let mut commented = 0usize;
    let mut reviewers = Vec::new();

    for r in items {
        let state = r.get("state").and_then(|v| v.as_str()).unwrap_or("");
        match state {
            "APPROVED" => approved += 1,
            "CHANGES_REQUESTED" => changes_requested += 1,
            "COMMENTED" => commented += 1,
            _ => {}
        }
        if let Some(login) = r
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|l| l.as_str())
        {
            let clean = sanitize_terminal_text(login);
            if !reviewers.contains(&clean) {
                reviewers.push(clean);
            }
        }
    }

    let decision = if changes_requested > 0 {
        "changes_requested"
    } else if approved > 0 {
        "approved"
    } else if commented > 0 {
        "commented"
    } else {
        "review_required"
    }
    .to_string();

    ReviewSummary {
        decision,
        approved_count: approved,
        changes_requested_count: changes_requested,
        commented_count: commented,
        reviewers,
    }
}

fn record_github_headers(
    store: &CacheStore,
    headers: &reqwest::header::HeaderMap,
    status_code: u16,
    now_sec: i64,
) {
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let limit_total = headers
        .get("x-ratelimit-limit")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let reset_at = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    let cooldown_until = if status_code == 429 || (status_code == 403 && remaining == Some(0)) {
        let retry = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(DEFAULT_429_COOLDOWN_SEC);
        Some(now_sec + retry)
    } else {
        None
    };

    let _ = store.update_quota(&QuotaRecord {
        provider: ProviderId::Github.as_str().into(),
        remaining,
        limit_total,
        reset_at,
        cooldown_until,
        last_error_kind: if status_code >= 400 {
            Some(format!("http_{status_code}"))
        } else {
            None
        },
        last_error_msg: None,
        last_success_at: if status_code < 400 {
            Some(now_sec)
        } else {
            None
        },
    });
}

async fn fetch_via_gh_cli(
    coord: &ProviderCoordinator,
    owner: &str,
    repo: &str,
    branch: &str,
    local_sha: Option<&str>,
) -> Option<PrBundle> {
    let _permit = coord.cli_semaphore().acquire().await.ok()?;
    let endpoint = format!("repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=all&per_page=5");
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("gh")
            .args(["api", &endpoint])
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !out.status.success() {
        return None;
    }
    let items: Vec<Value> = serde_json::from_slice(&out.stdout).ok()?;
    let prs = parse_github_pulls(&items, local_sha);
    Some(PrBundle {
        prs,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_github_pulls_and_check_runs() {
        let pulls = vec![json!({
            "number": 42,
            "title": "feat: add cockpit\x1b]7501;state=done\x07",
            "state": "open",
            "draft": false,
            "html_url": "https://github.com/acme/repo/pull/42",
            "head": { "ref": "feat/ENG-42", "sha": "abc1234" },
            "body": "See https://acme.slack.com/archives/C12345/p1700000000123456"
        })];
        let parsed = parse_github_pulls(&pulls, Some("def5678"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].number, 42);
        assert_eq!(parsed[0].title, "feat: add cockpit");
        assert!(parsed[0].local_ahead);

        let checks_val = json!({
            "check_runs": [
                { "name": "unit-tests", "status": "completed", "conclusion": "success", "html_url": "https://example.com/1" },
                { "name": "integration-tests", "status": "completed", "conclusion": "failure", "html_url": "https://example.com/2" }
            ]
        });
        let ci = parse_github_check_runs(&checks_val);
        assert_eq!(ci.overall_state, "failing");
        assert_eq!(ci.passed, 1);
        assert_eq!(ci.failed, 1);
        assert_eq!(ci.failing_checks, vec!["integration-tests".to_string()]);
    }
}
