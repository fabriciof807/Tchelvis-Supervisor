use crate::model::{ProviderHealth, ProviderId, ProviderStatus};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const DB_USER_VERSION: i32 = 1;

#[derive(Debug, Clone)]
pub struct CachedEntity {
    pub provider: String,
    pub resource_key: String,
    pub payload_json: String,
    pub etag: Option<String>,
    pub fetched_at: i64,
    pub expires_at: i64,
    pub is_negative: bool,
}

#[derive(Debug, Clone)]
pub struct QuotaRecord {
    pub provider: String,
    pub remaining: Option<i64>,
    pub limit_total: Option<i64>,
    pub reset_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub last_error_kind: Option<String>,
    pub last_error_msg: Option<String>,
    pub last_success_at: Option<i64>,
}

#[derive(Clone)]
pub struct CacheStore {
    db_path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl CacheStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("failed opening SQLite cache at {}", db_path.display()))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            db_path: db_path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            db_path: PathBuf::from(":memory:"),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 3000;
            PRAGMA user_version = 1;

            CREATE TABLE IF NOT EXISTS cached_entities (
                provider TEXT NOT NULL,
                resource_key TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                etag TEXT,
                fetched_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                is_negative INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (provider, resource_key)
            );

            CREATE TABLE IF NOT EXISTS provider_quotas (
                provider TEXT PRIMARY KEY,
                remaining INTEGER,
                limit_total INTEGER,
                reset_at INTEGER,
                cooldown_until INTEGER,
                last_error_kind TEXT,
                last_error_msg TEXT,
                last_success_at INTEGER
            );
            ",
        )?;
        Ok(())
    }

    pub fn get_cached(
        &self,
        provider: ProviderId,
        resource_key: &str,
    ) -> Result<Option<CachedEntity>> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        let row = conn
            .query_row(
                "SELECT provider, resource_key, payload_json, etag, fetched_at, expires_at, is_negative
                 FROM cached_entities
                 WHERE provider = ?1 AND resource_key = ?2",
                params![provider.as_str(), resource_key],
                |r| {
                    Ok(CachedEntity {
                        provider: r.get(0)?,
                        resource_key: r.get(1)?,
                        payload_json: r.get(2)?,
                        etag: r.get(3)?,
                        fetched_at: r.get(4)?,
                        expires_at: r.get(5)?,
                        is_negative: r.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn put_cached(&self, entity: &CachedEntity) -> Result<()> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        conn.execute(
            "INSERT INTO cached_entities (provider, resource_key, payload_json, etag, fetched_at, expires_at, is_negative)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(provider, resource_key) DO UPDATE SET
                 payload_json = excluded.payload_json,
                 etag = excluded.etag,
                 fetched_at = excluded.fetched_at,
                 expires_at = excluded.expires_at,
                 is_negative = excluded.is_negative",
            params![
                entity.provider,
                entity.resource_key,
                entity.payload_json,
                entity.etag,
                entity.fetched_at,
                entity.expires_at,
                if entity.is_negative { 1i64 } else { 0i64 },
            ],
        )?;
        Ok(())
    }

    pub fn touch_cached_expiry(
        &self,
        provider: ProviderId,
        resource_key: &str,
        now_sec: i64,
        new_expires_at: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        conn.execute(
            "UPDATE cached_entities
             SET fetched_at = ?3, expires_at = ?4
             WHERE provider = ?1 AND resource_key = ?2",
            params![provider.as_str(), resource_key, now_sec, new_expires_at],
        )?;
        Ok(())
    }

    pub fn get_quota(&self, provider: ProviderId) -> Result<Option<QuotaRecord>> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        let row = conn
            .query_row(
                "SELECT provider, remaining, limit_total, reset_at, cooldown_until, last_error_kind, last_error_msg, last_success_at
                 FROM provider_quotas
                 WHERE provider = ?1",
                params![provider.as_str()],
                |r| {
                    Ok(QuotaRecord {
                        provider: r.get(0)?,
                        remaining: r.get(1)?,
                        limit_total: r.get(2)?,
                        reset_at: r.get(3)?,
                        cooldown_until: r.get(4)?,
                        last_error_kind: r.get(5)?,
                        last_error_msg: r.get(6)?,
                        last_success_at: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn update_quota(&self, rec: &QuotaRecord) -> Result<()> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        conn.execute(
            "INSERT INTO provider_quotas (provider, remaining, limit_total, reset_at, cooldown_until, last_error_kind, last_error_msg, last_success_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(provider) DO UPDATE SET
                 remaining = COALESCE(excluded.remaining, provider_quotas.remaining),
                 limit_total = COALESCE(excluded.limit_total, provider_quotas.limit_total),
                 reset_at = COALESCE(excluded.reset_at, provider_quotas.reset_at),
                 cooldown_until = excluded.cooldown_until,
                 last_error_kind = excluded.last_error_kind,
                 last_error_msg = excluded.last_error_msg,
                 last_success_at = COALESCE(excluded.last_success_at, provider_quotas.last_success_at)",
            params![
                rec.provider,
                rec.remaining,
                rec.limit_total,
                rec.reset_at,
                rec.cooldown_until,
                rec.last_error_kind,
                rec.last_error_msg,
                rec.last_success_at,
            ],
        )?;
        Ok(())
    }

    pub fn prune_expired(&self, now_sec: i64, max_age_hours: u64) -> Result<usize> {
        let cutoff = now_sec - (max_age_hours as i64) * 3600;
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        let deleted = conn.execute(
            "DELETE FROM cached_entities WHERE expires_at < ?1 AND fetched_at < ?2",
            params![now_sec, cutoff],
        )?;
        Ok(deleted)
    }

    pub fn apply_quota_to_health(&self, mut health: ProviderHealth) -> ProviderHealth {
        if let Ok(Some(q)) = self.get_quota(health.provider) {
            health.quota_remaining = q.remaining;
            health.quota_limit = q.limit_total;
            health.quota_reset_at = q.reset_at;
            health.cooldown_until = q.cooldown_until;
            health.last_success_at = q.last_success_at;
            let now_sec = chrono::Utc::now().timestamp();
            if let Some(cd) = q.cooldown_until {
                if cd > now_sec {
                    health.status = ProviderStatus::RateLimited;
                    health.detail = Some(format!("Rate-limited until unix ts {cd}"));
                }
            }
        }
        health
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_put_get_and_prune() {
        let store = CacheStore::open_in_memory().unwrap();
        let ent = CachedEntity {
            provider: "github".into(),
            resource_key: "repo:acme/app:main".into(),
            payload_json: "{\"number\":42}".into(),
            etag: Some("W/\"123\"".into()),
            fetched_at: 100,
            expires_at: 160,
            is_negative: false,
        };
        store.put_cached(&ent).unwrap();
        let loaded = store
            .get_cached(ProviderId::Github, "repo:acme/app:main")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.etag.as_deref(), Some("W/\"123\""));

        let pruned = store.prune_expired(300_000, 72).unwrap();
        assert_eq!(pruned, 1);
        assert!(store
            .get_cached(ProviderId::Github, "repo:acme/app:main")
            .unwrap()
            .is_none());
    }
}
