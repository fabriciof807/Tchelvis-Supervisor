use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Allowed Lifecycle States.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LifecycleState {
    #[default]
    Working,
    WaitingConfirmation,
    StuckLoop,
    Error,
    Completed,
}

impl LifecycleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "WORKING",
            Self::WaitingConfirmation => "WAITING_CONFIRMATION",
            Self::StuckLoop => "STUCK_LOOP",
            Self::Error => "ERROR",
            Self::Completed => "COMPLETED",
        }
    }
}

/// Urgency level emitted by the cognitive layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Critical => "critical",
        }
    }

    pub fn display_badge(&self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Normal => "Normal",
            Self::Critical => "Critical",
        }
    }
}

/// Supervisor operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SupervisorMode {
    #[default]
    SemiAuto,
    AlertOnly,
    Paused,
}

impl SupervisorMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::SemiAuto => "Semi-Auto (Whitelist)",
            Self::AlertOnly => "Alert Only (Read-Only)",
            Self::Paused => "Paused",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::SemiAuto => Self::AlertOnly,
            Self::AlertOnly => Self::Paused,
            Self::Paused => Self::SemiAuto,
        }
    }
}

/// Audit entry for an executed `AUTO_REPLY`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoReplyAuditEntry {
    pub timestamp: u64,
    pub payload: String,
    pub summary: String,
}

/// Details of an active Operator Alert Banner / Action Card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOperatorAction {
    pub urgency: Urgency,
    pub detected_state: LifecycleState,
    pub reason_for_operator: String,
    pub updated_summary: String,
    pub hc_command_payload: String,
    pub triggered_at: u64,
}

/// Per-Agent Durable State Schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDurableState {
    pub agent_id: String,
    pub session_name: String,
    pub last_output_hash: String,
    pub last_change_timestamp: u64,
    pub last_evaluated_hash: String,
    pub lifecycle_state: LifecycleState,
    pub consecutive_auto_replies: u32,
    pub max_auto_replies: u32,
    pub rolling_summary: String,
    pub last_auto_action: Option<String>,
    pub pending_operator_action: Option<PendingOperatorAction>,
    #[serde(default)]
    pub last_hc_status: Option<String>,
    #[serde(default)]
    pub auto_reply_log: Vec<AutoReplyAuditEntry>,
    #[serde(default)]
    pub last_output_preview: String,
    #[serde(default)]
    pub last_eval_error: Option<String>,
    #[serde(default)]
    pub last_eval_error_at: Option<u64>,
}

impl AgentDurableState {
    pub fn new(
        agent_id: impl Into<String>,
        session_name: impl Into<String>,
        max_auto_replies: u32,
        now: u64,
    ) -> Self {
        let empty_hash = compute_sha256("");
        Self {
            agent_id: agent_id.into(),
            session_name: session_name.into(),
            last_output_hash: empty_hash.clone(),
            last_change_timestamp: now,
            last_evaluated_hash: empty_hash,
            lifecycle_state: LifecycleState::Working,
            consecutive_auto_replies: 0,
            max_auto_replies,
            rolling_summary: "Monitorando sessão ativa do agente.".to_string(),
            last_auto_action: None,
            pending_operator_action: None,
            last_hc_status: None,
            auto_reply_log: Vec::new(),
            last_output_preview: String::new(),
            last_eval_error: None,
            last_eval_error_at: None,
        }
    }

    /// Resets `consecutive_auto_replies` when the human operator approves an Action Card or manually intervenes.
    pub fn reset_operator_counter(&mut self) {
        self.consecutive_auto_replies = 0;
        self.pending_operator_action = None;
        self.last_evaluated_hash = self.last_output_hash.clone();
    }

    /// Ignores the current alert and marks `last_evaluated_hash = current_hash`.
    pub fn ignore_alert(&mut self) {
        self.pending_operator_action = None;
        self.last_evaluated_hash = self.last_output_hash.clone();
    }
}

/// Top-level persisted supervisor store (`~/.local/state/tchelvis/supervisor_state.json`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DurableStateStore {
    pub mode: SupervisorMode,
    pub agents: BTreeMap<String, AgentDurableState>,
    #[serde(default)]
    pub zero_active_since: Option<u64>,
    #[serde(default)]
    pub model_loaded: bool,
    #[serde(skip)]
    file_path: PathBuf,
}

impl DurableStateStore {
    /// Loads existing state from disk or initializes a fresh store at `path`.
    pub fn load_or_init(path: impl AsRef<Path>) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        Self::load_from_source(&path_buf, path_buf.clone())
    }

    /// Loads from `path` if it exists, otherwise falls back to reading `legacy_path`
    /// while keeping `path` as the target save path.
    pub fn load_or_init_with_fallback(
        path: impl AsRef<Path>,
        legacy_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let primary = path.as_ref().to_path_buf();
        if primary.exists() {
            return Self::load_from_source(&primary, primary.clone());
        }
        let legacy = legacy_path.as_ref();
        if legacy.exists() {
            return Self::load_from_source(legacy, primary);
        }
        Ok(Self {
            mode: SupervisorMode::SemiAuto,
            agents: BTreeMap::new(),
            file_path: primary,
            ..Default::default()
        })
    }

    fn load_from_source(read_path: &Path, save_path: PathBuf) -> Result<Self> {
        if read_path.exists() {
            let content = fs::read_to_string(read_path)
                .with_context(|| format!("Failed to read state file {:?}", read_path))?;
            if !content.trim().is_empty() {
                if let Ok(mut store) = serde_json::from_str::<DurableStateStore>(&content) {
                    store.file_path = save_path;
                    return Ok(store);
                }
                // Also support loading a single AgentDurableState if created externally
                if let Ok(single) = serde_json::from_str::<AgentDurableState>(&content) {
                    let mut agents = BTreeMap::new();
                    agents.insert(single.agent_id.clone(), single);
                    return Ok(Self {
                        mode: SupervisorMode::SemiAuto,
                        agents,
                        file_path: save_path,
                        ..Default::default()
                    });
                }
            }
        }
        Ok(Self {
            mode: SupervisorMode::SemiAuto,
            agents: BTreeMap::new(),
            file_path: save_path,
            ..Default::default()
        })
    }

    /// Atomically persists the state store to disk.
    pub fn save(&self) -> Result<()> {
        if self.file_path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create state directory {:?}", parent))?;
        }
        let tmp_path = self.file_path.with_extension("json.tmp");
        let serialized =
            serde_json::to_string_pretty(self).context("Failed to serialize DurableStateStore")?;
        fs::write(&tmp_path, serialized)
            .with_context(|| format!("Failed to write temporary state file {:?}", tmp_path))?;
        fs::rename(&tmp_path, &self.file_path)
            .with_context(|| format!("Failed to rename {:?} to {:?}", tmp_path, self.file_path))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.file_path
    }
}

/// Computes the hex-encoded SHA-256 hash of a string.
pub fn compute_sha256(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Helper returning current Unix epoch timestamp in seconds.
pub fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_sha256_empty_vector_matches_spec() {
        assert_eq!(
            compute_sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_state_store_roundtrip_persistence() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("supervisor_state.json");
        let mut store = DurableStateStore::load_or_init(&file_path).unwrap();

        let agent = AgentDurableState::new("worker-01", "refactor-auth", 2, 1_700_000_000);
        store.agents.insert("worker-01".to_string(), agent);
        store.save().unwrap();

        let loaded = DurableStateStore::load_or_init(&file_path).unwrap();
        assert_eq!(loaded.agents.len(), 1);
        let a = loaded.agents.get("worker-01").unwrap();
        assert_eq!(a.session_name, "refactor-auth");
        assert_eq!(a.lifecycle_state, LifecycleState::Working);
        assert_eq!(a.max_auto_replies, 2);
    }
}
