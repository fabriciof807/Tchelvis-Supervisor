use crate::config::CockpitConfig;
use crate::model::{
    sanitize_terminal_text, CiSummary, DeployReleaseRef, ProviderHealth, ProviderId,
    ProviderStatus, PullRequestRef, ReviewSummary,
};
use crate::providers::{
    PrBundle, ProviderCoordinator, DEFAULT_429_COOLDOWN_SEC, POSITIVE_CACHE_TTL_SEC,
};
use crate::store::{CacheStore, CachedEntity, QuotaRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedGitlabBundle {
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
    let token = cfg.resolve_token(ProviderId::Gitlab).await;
    if token.is_none() {
        return ProviderHealth {
            provider: ProviderId::Gitlab,
            status: ProviderStatus::Inactive,
            detail: Some("No GITLAB_TOKEN or authenticated `glab` session".into()),
            ..Default::default()
        };
    }
    store.apply_quota_to_health(ProviderHealth {
        provider: ProviderId::Gitlab,
        status: ProviderStatus::Ready,
        detail: Some("Authenticated via GITLAB_TOKEN/glab".into()),
        ..Default::default()
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_mr_bundle(
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
    let cache_key = format!("gl:{host}/{owner}/{repo}:{branch}");
    let now_sec = chrono::Utc::now().timestamp();

    let cached = store
        .get_cached(ProviderId::Gitlab, &cache_key)
        .ok()
        .flatten();
    if let Some(ref ent) = cached {
        if !force_refresh && ent.expires_at > now_sec {
            if ent.is_negative {
                return None;
            }
            if let Ok(parsed) = serde_json::from_str::<CachedGitlabBundle>(&ent.payload_json) {
                return Some(PrBundle {
                    prs: parsed.prs,
                    ci: parsed.ci,
                    review: parsed.review,
                    deploy_release: parsed.deploy_release,
                });
            }
        }
    }

    let token = cfg.resolve_token(ProviderId::Gitlab).await?;
    if !coord.can_query_provider(ProviderId::Gitlab, store, force_refresh) {
        return None;
    }

    let encoded_project = format!("{owner}/{repo}").replace('/', "%2F");
    let base = if host.is_empty() || host == "gitlab.com" {
        cfg.gitlab.base_url.trim_end_matches('/').to_string()
    } else {
        format!("https://{host}")
    };

    let mr_url = format!(
        "{base}/api/v4/projects/{encoded_project}/merge_requests?source_branch={branch}&per_page=5"
    );
    let mut req = coord
        .http_client()
        .get(&mr_url)
        .header("PRIVATE-TOKEN", &token)
        .header("Authorization", format!("Bearer {token}"));

    if let Some(ref ent) = cached {
        if let Some(ref etag) = ent.etag {
            req = req.header("If-None-Match", etag);
        }
    }

    let resp = req.send().await.ok()?;
    let status = resp.status().as_u16();

    let remaining = resp
        .headers()
        .get("ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let limit_total = resp
        .headers()
        .get("ratelimit-limit")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let cooldown_until = if status == 429 {
        Some(now_sec + DEFAULT_429_COOLDOWN_SEC)
    } else {
        None
    };

    let _ = store.update_quota(&QuotaRecord {
        provider: ProviderId::Gitlab.as_str().into(),
        remaining,
        limit_total,
        reset_at: None,
        cooldown_until,
        last_error_kind: if status >= 400 {
            Some(format!("http_{status}"))
        } else {
            None
        },
        last_error_msg: None,
        last_success_at: if status < 400 { Some(now_sec) } else { None },
    });

    if status == 304 {
        if let Some(ent) = cached {
            let _ = store.touch_cached_expiry(
                ProviderId::Gitlab,
                &cache_key,
                now_sec,
                now_sec + POSITIVE_CACHE_TTL_SEC,
            );
            if let Ok(parsed) = serde_json::from_str::<CachedGitlabBundle>(&ent.payload_json) {
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
    let prs = parse_gitlab_mrs(&items, local_sha);

    let bundle_struct = CachedGitlabBundle {
        prs: prs.clone(),
        ci: None,
        review: None,
        deploy_release: None,
    };
    if let Ok(payload_json) = serde_json::to_string(&bundle_struct) {
        let _ = store.put_cached(&CachedEntity {
            provider: ProviderId::Gitlab.as_str().into(),
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
        ..Default::default()
    })
}

pub fn parse_gitlab_mrs(items: &[Value], local_sha: Option<&str>) -> Vec<PullRequestRef> {
    let mut out = Vec::new();
    for item in items {
        let Some(iid) = item.get("iid").and_then(|v| v.as_u64()) else {
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
            .unwrap_or_else(|| "opened".into());
        let is_draft = item.get("draft").and_then(|v| v.as_bool()).unwrap_or(false);
        let url = item
            .get("web_url")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_default();
        let head_branch = item
            .get("source_branch")
            .and_then(|v| v.as_str())
            .map(sanitize_terminal_text)
            .unwrap_or_default();
        let head_sha = item
            .get("sha")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let local_ahead = match (local_sha, head_sha.as_deref()) {
            (Some(l), Some(r)) => l != r,
            _ => false,
        };
        let body_snippet = item
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| sanitize_terminal_text(&s.chars().take(500).collect::<String>()));

        out.push(PullRequestRef {
            provider: ProviderId::Gitlab,
            number: iid,
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
