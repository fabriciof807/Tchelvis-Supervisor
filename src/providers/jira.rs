use crate::config::{CockpitConfig, JiraAuthMode};
use crate::model::{sanitize_terminal_text, IssueRef, ProviderHealth, ProviderId, ProviderStatus};
use crate::providers::{
    ProviderCoordinator, DEFAULT_429_COOLDOWN_SEC, NEGATIVE_CACHE_TTL_SEC, POSITIVE_CACHE_TTL_SEC,
};
use crate::store::{CacheStore, CachedEntity, QuotaRecord};
use base64::Engine;
use serde_json::Value;

pub async fn check_health(
    _coord: &ProviderCoordinator,
    cfg: &CockpitConfig,
    store: &CacheStore,
) -> ProviderHealth {
    let Some(base_url) = cfg.jira.base_url.as_deref().filter(|s| !s.is_empty()) else {
        return ProviderHealth {
            provider: ProviderId::Jira,
            status: ProviderStatus::Inactive,
            detail: Some("Jira base_url not configured".into()),
            ..Default::default()
        };
    };

    let token = cfg.resolve_token(ProviderId::Jira).await;
    if token.is_none() {
        return ProviderHealth {
            provider: ProviderId::Jira,
            status: ProviderStatus::Inactive,
            detail: Some(format!("No TCHELVIS_JIRA_TOKEN for {base_url}")),
            ..Default::default()
        };
    }

    store.apply_quota_to_health(ProviderHealth {
        provider: ProviderId::Jira,
        status: ProviderStatus::Ready,
        detail: Some(format!("Configured for {base_url}")),
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
    let base_url = cfg
        .jira
        .base_url
        .as_deref()
        .filter(|s| !s.is_empty())?
        .trim_end_matches('/');

    let cache_key = format!("jira:{key}");
    let now_sec = chrono::Utc::now().timestamp();

    let cached = store
        .get_cached(ProviderId::Jira, &cache_key)
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

    let token = cfg.resolve_token(ProviderId::Jira).await?;
    if !coord.can_query_provider(ProviderId::Jira, store, force_refresh) {
        if let Some(ent) = cached {
            if !ent.is_negative {
                return serde_json::from_str::<IssueRef>(&ent.payload_json).ok();
            }
        }
        return None;
    }

    let email = crate::envvars::var("JIRA_EMAIL").or_else(|| cfg.jira.email.clone());

    let auth_header = match (cfg.jira.auth_mode, email.as_deref()) {
        (JiraAuthMode::Basic, Some(em)) if !em.is_empty() => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{em}:{token}"));
            format!("Basic {b64}")
        }
        _ => format!("Bearer {token}"),
    };

    let url_v3 = format!(
        "{base_url}/rest/api/3/issue/{key}?fields=summary,status,assignee,priority,updated"
    );
    let mut req = coord
        .http_client()
        .get(&url_v3)
        .header("Accept", "application/json")
        .header("Authorization", &auth_header);

    if let Some(ref ent) = cached {
        if let Some(ref etag) = ent.etag {
            req = req.header("If-None-Match", etag);
        }
    }

    let resp = req.send().await.ok()?;
    let status = resp.status().as_u16();

    if status == 429 {
        let retry = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(DEFAULT_429_COOLDOWN_SEC);
        let _ = store.update_quota(&QuotaRecord {
            provider: ProviderId::Jira.as_str().into(),
            remaining: None,
            limit_total: None,
            reset_at: None,
            cooldown_until: Some(now_sec + retry),
            last_error_kind: Some("rate_limited".into()),
            last_error_msg: None,
            last_success_at: None,
        });
        return None;
    }

    if status == 304 {
        if let Some(ent) = cached {
            let _ = store.touch_cached_expiry(
                ProviderId::Jira,
                &cache_key,
                now_sec,
                now_sec + POSITIVE_CACHE_TTL_SEC,
            );
            return serde_json::from_str::<IssueRef>(&ent.payload_json).ok();
        }
        return None;
    }

    if status == 404 {
        // Cache negative lookup for 15 minutes
        let _ = store.put_cached(&CachedEntity {
            provider: ProviderId::Jira.as_str().into(),
            resource_key: cache_key,
            payload_json: "null".into(),
            etag: None,
            fetched_at: now_sec,
            expires_at: now_sec + NEGATIVE_CACHE_TTL_SEC,
            is_negative: true,
        });
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
    let val: Value = resp.json().await.ok()?;
    let issue = parse_jira_issue(&val, base_url, key)?;

    if let Ok(payload_json) = serde_json::to_string(&issue) {
        let _ = store.put_cached(&CachedEntity {
            provider: ProviderId::Jira.as_str().into(),
            resource_key: cache_key,
            payload_json,
            etag: new_etag,
            fetched_at: now_sec,
            expires_at: now_sec + POSITIVE_CACHE_TTL_SEC,
            is_negative: false,
        });
    }

    Some(issue)
}

pub fn parse_jira_issue(val: &Value, base_url: &str, fallback_key: &str) -> Option<IssueRef> {
    let key = val
        .get("key")
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text)
        .unwrap_or_else(|| fallback_key.to_string());
    let fields = val.get("fields")?;
    let title = fields
        .get("summary")
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text)
        .unwrap_or_default();
    let status = fields
        .get("status")
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text)
        .unwrap_or_else(|| "Unknown".into());
    let assignee = fields
        .get("assignee")
        .and_then(|a| a.get("displayName"))
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text);
    let priority = fields
        .get("priority")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .map(sanitize_terminal_text);

    Some(IssueRef {
        provider: ProviderId::Jira,
        url: format!("{}/browse/{key}", base_url.trim_end_matches('/')),
        key,
        title,
        status,
        assignee,
        priority,
    })
}
