use crate::config::CockpitConfig;
use crate::model::{sanitize_terminal_text, IssueRef, ProviderHealth, ProviderId, ProviderStatus};
use crate::providers::{
    ProviderCoordinator, DEFAULT_429_COOLDOWN_SEC, NEGATIVE_CACHE_TTL_SEC, POSITIVE_CACHE_TTL_SEC,
};
use crate::store::{CacheStore, CachedEntity, QuotaRecord};
use serde_json::{json, Value};

pub async fn check_health(
    _coord: &ProviderCoordinator,
    cfg: &CockpitConfig,
    store: &CacheStore,
) -> ProviderHealth {
    let token = cfg.resolve_token(ProviderId::Linear).await;
    if token.is_none() {
        return ProviderHealth {
            provider: ProviderId::Linear,
            status: ProviderStatus::Inactive,
            detail: Some("No COCKPIT_LINEAR_TOKEN configured".into()),
            ..Default::default()
        };
    }

    store.apply_quota_to_health(ProviderHealth {
        provider: ProviderId::Linear,
        status: ProviderStatus::Ready,
        detail: Some("Configured via COCKPIT_LINEAR_TOKEN".into()),
        ..Default::default()
    })
}

pub async fn fetch_issue(
    coord: &ProviderCoordinator,
    cfg: &CockpitConfig,
    store: &CacheStore,
    key: &str,
    force_refresh: bool,
) -> Option<IssueRef> {
    let cache_key = format!("linear:{key}");
    let now_sec = chrono::Utc::now().timestamp();

    let cached = store
        .get_cached(ProviderId::Linear, &cache_key)
        .ok()
        .flatten();
    if let Some(ref ent) = cached {
        if !force_refresh && ent.expires_at > now_sec {
            if ent.is_negative {
                return None;
            }
            if let Ok(parsed) = serde_json::from_str::<IssueRef>(&ent.payload_json) {
                return Some(parsed);
            }
        }
    }

    let token = cfg.resolve_token(ProviderId::Linear).await?;
    if !coord.can_query_provider(ProviderId::Linear, store, force_refresh) {
        if let Some(ent) = cached {
            if !ent.is_negative {
                return serde_json::from_str::<IssueRef>(&ent.payload_json).ok();
            }
        }
        return None;
    }

    let query = r#"
        query CockpitIssue($id: String!) {
            issue(id: $id) {
                identifier
                title
                url
                priorityLabel
                state { name type }
                assignee { name }
            }
        }
    "#;

    let resp = coord
        .http_client()
        .post("https://api.linear.app/graphql")
        .header("Content-Type", "application/json")
        .header("Authorization", &token)
        .json(&json!({
            "query": query,
            "variables": { "id": key }
        }))
        .send()
        .await
        .ok()?;

    let status = resp.status().as_u16();
    let remaining = resp
        .headers()
        .get("x-ratelimit-requests-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let limit_total = resp
        .headers()
        .get("x-ratelimit-requests-limit")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    let cooldown_until = if status == 429 {
        Some(now_sec + DEFAULT_429_COOLDOWN_SEC)
    } else {
        None
    };

    let _ = store.update_quota(&QuotaRecord {
        provider: ProviderId::Linear.as_str().into(),
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

    if !resp.status().is_success() {
        return None;
    }

    let body: Value = resp.json().await.ok()?;
    let Some(iss_node) = body.get("data").and_then(|d| d.get("issue")) else {
        let _ = store.put_cached(&CachedEntity {
            provider: ProviderId::Linear.as_str().into(),
            resource_key: cache_key,
            payload_json: "null".into(),
            etag: None,
            fetched_at: now_sec,
            expires_at: now_sec + NEGATIVE_CACHE_TTL_SEC,
            is_negative: true,
        });
        return None;
    };
    if iss_node.is_null() {
        return None;
    }

    let issue = parse_linear_issue(iss_node, key)?;
    if let Ok(payload_json) = serde_json::to_string(&issue) {
        let _ = store.put_cached(&CachedEntity {
            provider: ProviderId::Linear.as_str().into(),
            resource_key: cache_key,
            payload_json,
            etag: None,
            fetched_at: now_sec,
            expires_at: now_sec + POSITIVE_CACHE_TTL_SEC,
            is_negative: false,
        });
    }
    Some(issue)
}

pub fn parse_linear_issue(iss_node: &Value, fallback_key: &str) -> Option<IssueRef> {
    if iss_node.is_null() {
        return None;
    }
    let key = iss_node
        .get("identifier")
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text)
        .unwrap_or_else(|| fallback_key.to_string());
    let title = iss_node
        .get("title")
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text)
        .unwrap_or_default();
    let url = iss_node
        .get("url")
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text)
        .unwrap_or_default();
    let status = iss_node
        .get("state")
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text)
        .unwrap_or_else(|| "Unknown".into());
    let assignee = iss_node
        .get("assignee")
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text);
    let priority = iss_node
        .get("priorityLabel")
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text);

    Some(IssueRef {
        provider: ProviderId::Linear,
        key,
        title,
        status,
        assignee,
        priority,
        url,
    })
}
