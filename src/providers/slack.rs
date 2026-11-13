use crate::config::CockpitConfig;
use crate::model::{
    sanitize_terminal_text, ProviderHealth, ProviderId, ProviderStatus, SlackThreadRef,
};
use crate::providers::{ProviderCoordinator, DEFAULT_429_COOLDOWN_SEC, POSITIVE_CACHE_TTL_SEC};
use crate::store::{CacheStore, CachedEntity, QuotaRecord};
use serde_json::Value;

pub async fn check_health(
    _coord: &ProviderCoordinator,
    cfg: &CockpitConfig,
    store: &CacheStore,
) -> ProviderHealth {
    let token = cfg.resolve_token(ProviderId::Slack).await;
    if token.is_none() {
        return ProviderHealth {
            provider: ProviderId::Slack,
            status: ProviderStatus::Inactive,
            detail: Some(
                "No COCKPIT_SLACK_TOKEN configured (URL link parsing still active)".into(),
            ),
            ..Default::default()
        };
    }

    if let Ok(Some(q)) = store.get_quota(ProviderId::Slack) {
        if q.last_error_kind.as_deref() == Some("missing_scope") {
            return ProviderHealth {
                provider: ProviderId::Slack,
                status: ProviderStatus::MissingScope,
                detail: q.last_error_msg,
                ..Default::default()
            };
        }
    }

    store.apply_quota_to_health(ProviderHealth {
        provider: ProviderId::Slack,
        status: ProviderStatus::Ready,
        detail: Some("Configured via COCKPIT_SLACK_TOKEN".into()),
        ..Default::default()
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_thread(
    coord: &ProviderCoordinator,
    cfg: &CockpitConfig,
    store: &CacheStore,
    workspace: &str,
    channel_id: &str,
    thread_ts: &str,
    url: &str,
    force_refresh: bool,
) -> SlackThreadRef {
    let base_ref = SlackThreadRef {
        workspace: sanitize_terminal_text(workspace),
        channel_id: sanitize_terminal_text(channel_id),
        thread_ts: sanitize_terminal_text(thread_ts),
        url: sanitize_terminal_text(url),
        title_snippet: None,
        reply_count: None,
        latest_reply_at: None,
    };

    let cache_key = format!("slack:{workspace}:{channel_id}:{thread_ts}");
    let now_sec = chrono::Utc::now().timestamp();

    let cached = store
        .get_cached(ProviderId::Slack, &cache_key)
        .ok()
        .flatten();
    if let Some(ref ent) = cached {
        if !force_refresh && ent.expires_at > now_sec {
            if let Ok(parsed) = serde_json::from_str::<SlackThreadRef>(&ent.payload_json) {
                return parsed;
            }
        }
    }

    let Some(token) = cfg.resolve_token(ProviderId::Slack).await else {
        return base_ref;
    };

    if !coord.can_query_provider(ProviderId::Slack, store, force_refresh) {
        if let Some(ent) = cached {
            if let Ok(parsed) = serde_json::from_str::<SlackThreadRef>(&ent.payload_json) {
                return parsed;
            }
        }
        return base_ref;
    }

    let api_url = format!(
        "https://slack.com/api/conversations.replies?channel={channel_id}&ts={thread_ts}&limit=5"
    );
    let Ok(resp) = coord
        .http_client()
        .get(&api_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
    else {
        return base_ref;
    };

    let status = resp.status().as_u16();
    if status == 429 {
        let retry = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(DEFAULT_429_COOLDOWN_SEC);
        let _ = store.update_quota(&QuotaRecord {
            provider: ProviderId::Slack.as_str().into(),
            remaining: None,
            limit_total: None,
            reset_at: None,
            cooldown_until: Some(now_sec + retry),
            last_error_kind: Some("rate_limited".into()),
            last_error_msg: None,
            last_success_at: None,
        });
        return base_ref;
    }

    let Ok(val) = resp.json::<Value>().await else {
        return base_ref;
    };

    if val.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err_str = val
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("slack_error");
        let _ = store.update_quota(&QuotaRecord {
            provider: ProviderId::Slack.as_str().into(),
            remaining: None,
            limit_total: None,
            reset_at: None,
            cooldown_until: None,
            last_error_kind: Some(err_str.to_string()),
            last_error_msg: Some(format!("Slack API error: {err_str}")),
            last_success_at: None,
        });
        return base_ref;
    }

    let mut enriched = base_ref;
    if let Some(msgs) = val.get("messages").and_then(|m| m.as_array()) {
        if let Some(root) = msgs.first() {
            enriched.title_snippet = root
                .get("text")
                .and_then(|t| t.as_str())
                .map(|s| sanitize_terminal_text(&s.chars().take(160).collect::<String>()));
            enriched.reply_count = root
                .get("reply_count")
                .and_then(|c| c.as_u64())
                .map(|c| c as u32);
            enriched.latest_reply_at = root
                .get("latest_reply")
                .and_then(|t| t.as_str())
                .map(sanitize_terminal_text);
        }
    }

    if let Ok(payload_json) = serde_json::to_string(&enriched) {
        let _ = store.put_cached(&CachedEntity {
            provider: ProviderId::Slack.as_str().into(),
            resource_key: cache_key,
            payload_json,
            etag: None,
            fetched_at: now_sec,
            expires_at: now_sec + POSITIVE_CACHE_TTL_SEC,
            is_negative: false,
        });
    }

    enriched
}
