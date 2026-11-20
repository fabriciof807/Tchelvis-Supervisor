use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod cli;
pub mod delivery;
pub mod preamble;
pub mod reconciler;
pub mod store;

pub const WORKER_DONE_QUIET_MS: i64 = 1_500;
pub const BLOCKED_SUPERVISOR_GRACE_MS: i64 = 3_000;
pub const TERMINAL_APPEAR_GRACE_MS: i64 = 6_000;
pub const POINTER_RETRY_BACKOFF_MS: i64 = 30_000;
pub const MAX_DELIVERY_ATTEMPTS: i32 = 3;
pub const MAX_DISPATCH_FAILURES: i32 = 3;
pub const MAX_ORCH_DEPTH: i32 = 2;

static ORCH_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn generate_orch_id(prefix: &str) -> String {
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let pid = std::process::id();
    let seq = ORCH_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    hasher.update(ts_ms.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    let digest = hasher.finalize();
    let hash_prefix_4 = format!("{:02x}{:02x}", digest[0], digest[1]);
    format!("{prefix}_{ts_ms:x}_{hash_prefix_4}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Active,
    Completed,
    Archived,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Archived => "archived",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "completed" => Ok(Self::Completed),
            "archived" => Ok(Self::Archived),
            other => anyhow::bail!("invalid RunStatus: '{other}'"),
        }
    }
}

impl TryFrom<&str> for RunStatus {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchRun {
    pub id: String,
    pub title: String,
    pub coordinator_terminal: String,
    pub coordinator_created_ms: i64,
    pub status: RunStatus,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Ready,
    Dispatched,
    Completed,
    Failed,
    Blocked,
    CircuitBroken,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Dispatched => "dispatched",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::CircuitBroken => "circuit_broken",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "dispatched" => Ok(Self::Dispatched),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "blocked" => Ok(Self::Blocked),
            "circuit_broken" => Ok(Self::CircuitBroken),
            other => anyhow::bail!("invalid TaskStatus: '{other}'"),
        }
    }
}

impl TryFrom<&str> for TaskStatus {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchTask {
    pub id: String,
    pub run_id: String,
    pub title: String,
    pub prompt: String,
    pub deps: Vec<String>,
    pub failure_count: i32,
    pub status: TaskStatus,
    pub result_summary: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStatus {
    Starting,
    Dispatched,
    WaitingForReply,
    NeedsReview,
    Completed,
    Failed,
    Stopped,
    Superseded,
}

impl DispatchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Dispatched => "dispatched",
            Self::WaitingForReply => "waiting_for_reply",
            Self::NeedsReview => "needs_review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Superseded => "superseded",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Dispatched | Self::WaitingForReply
        )
    }

    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Completed | Self::Superseded)
    }
}

impl fmt::Display for DispatchStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DispatchStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "starting" => Ok(Self::Starting),
            "dispatched" => Ok(Self::Dispatched),
            "waiting_for_reply" => Ok(Self::WaitingForReply),
            "needs_review" => Ok(Self::NeedsReview),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "stopped" => Ok(Self::Stopped),
            "superseded" => Ok(Self::Superseded),
            other => anyhow::bail!("invalid DispatchStatus: '{other}'"),
        }
    }
}

impl TryFrom<&str> for DispatchStatus {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchContext {
    pub id: String,
    pub run_id: String,
    pub task_id: String,
    pub parent_terminal: String,
    pub assignee_terminal: String,
    pub terminal_created_ms: i64,
    pub turn_at_dispatch: u64,
    pub agent_kind: String,
    pub worktree_mode: String,
    pub worktree_path: Option<String>,
    pub depth: i32,
    pub status: DispatchStatus,
    pub idle_since_ms: Option<i64>,
    pub blocked_since_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub fn send_to_recovery_hint(disp: &DispatchContext) -> String {
    if disp.status.is_retryable() {
        format!(
            "use `tchelvis orch retry {}` to launch a new attempt",
            disp.id
        )
    } else {
        let agent_arg: std::borrow::Cow<'_, str> = if disp.agent_kind == "attached" {
            std::borrow::Cow::Borrowed("<agent>")
        } else {
            shlex::try_quote(&disp.agent_kind)
                .unwrap_or(std::borrow::Cow::Borrowed(disp.agent_kind.as_str()))
        };
        if disp.status == DispatchStatus::Completed {
            format!(
                "task '{}' already finished; launch a new task instead with `tchelvis orch launch --run {} --agent {} --prompt \"...\"`",
                disp.task_id, disp.run_id, agent_arg
            )
        } else {
            format!(
                "this dispatch was superseded by a newer attempt; check `tchelvis orch status --run {}` or launch a new task with `tchelvis orch launch --run {} --agent {} --prompt \"...\"`",
                disp.run_id, disp.run_id, agent_arg
            )
        }
    }
}

pub fn send_to_status_error(disp: &DispatchContext) -> String {
    format!(
        "cannot send message to dispatch '{}' because its status is '{}'; {}",
        disp.id,
        disp.status,
        send_to_recovery_hint(disp)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    WorkerDone,
    Question,
    Reply,
    Directive,
    Escalation,
    Status,
}

impl MessageType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WorkerDone => "worker_done",
            Self::Question => "question",
            Self::Reply => "reply",
            Self::Directive => "directive",
            Self::Escalation => "escalation",
            Self::Status => "status",
        }
    }
}

impl fmt::Display for MessageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MessageType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "worker_done" => Ok(Self::WorkerDone),
            "question" => Ok(Self::Question),
            "reply" => Ok(Self::Reply),
            "directive" => Ok(Self::Directive),
            "escalation" => Ok(Self::Escalation),
            "status" => Ok(Self::Status),
            other => anyhow::bail!("invalid MessageType: '{other}'"),
        }
    }
}

impl TryFrom<&str> for MessageType {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerState {
    Pending,
    Delivered,
    Acknowledged,
    DeliveryStalled,
}

impl PointerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Acknowledged => "acknowledged",
            Self::DeliveryStalled => "delivery_stalled",
        }
    }
}

impl fmt::Display for PointerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PointerState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "delivered" => Ok(Self::Delivered),
            "acknowledged" => Ok(Self::Acknowledged),
            "delivery_stalled" => Ok(Self::DeliveryStalled),
            other => anyhow::bail!("invalid PointerState: '{other}'"),
        }
    }
}

impl TryFrom<&str> for PointerState {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchMessage {
    pub id: String,
    pub run_id: String,
    pub task_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub from_handle: String,
    pub to_handle: String,
    pub target_terminal: String,
    pub msg_type: MessageType,
    pub outcome: Option<String>,
    pub body: String,
    pub pointer_state: PointerState,
    pub pointer_terminal: Option<String>,
    pub delivery_attempts: i32,
    pub next_retry_at_ms: Option<i64>,
    pub delivered_at_ms: Option<i64>,
    pub acknowledged_at_ms: Option<i64>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionThread {
    pub id: String,
    pub run_id: String,
    pub dispatch_id: String,
    pub question_msg_id: String,
    pub reply_msg_id: Option<String>,
    pub status: String,
    pub created_at_ms: i64,
    pub answered_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPointerTarget {
    pub target_terminal: String,
    pub run_id: String,
    pub pending_count: usize,
    pub has_active_unexpired_pointer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxCheckResult {
    pub target_terminal: String,
    pub run_id_filter: Option<String>,
    pub messages: Vec<OrchMessage>,
    pub ready_tasks: Vec<OrchTask>,
    pub blocked_tasks: Vec<OrchTask>,
    pub active_dispatches: Vec<DispatchContext>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryOutcome {
    pub delivered_count: usize,
    pub deferred_count: usize,
    pub stalled_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileOutcome {
    pub stop_escalations: usize,
    pub block_escalations: usize,
    pub state_changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_orch_id_format_and_uniqueness() {
        let id1 = generate_orch_id("run");
        let id2 = generate_orch_id("run");
        assert!(id1.starts_with("run_"));
        assert!(id2.starts_with("run_"));
        assert_ne!(id1, id2);
        let parts: Vec<&str> = id1.split('_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "run");
        assert_eq!(parts[2].len(), 4);
    }

    #[test]
    fn test_enum_roundtrips() {
        for status in [RunStatus::Active, RunStatus::Completed, RunStatus::Archived] {
            assert_eq!(RunStatus::from_str(status.as_str()).unwrap(), status);
        }
        for status in [
            TaskStatus::Pending,
            TaskStatus::Ready,
            TaskStatus::Dispatched,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Blocked,
            TaskStatus::CircuitBroken,
        ] {
            assert_eq!(TaskStatus::from_str(status.as_str()).unwrap(), status);
        }
        for status in [
            DispatchStatus::Starting,
            DispatchStatus::Dispatched,
            DispatchStatus::WaitingForReply,
            DispatchStatus::NeedsReview,
            DispatchStatus::Completed,
            DispatchStatus::Failed,
            DispatchStatus::Stopped,
            DispatchStatus::Superseded,
        ] {
            assert_eq!(DispatchStatus::from_str(status.as_str()).unwrap(), status);
            assert_eq!(
                status.is_active(),
                matches!(
                    status,
                    DispatchStatus::Starting
                        | DispatchStatus::Dispatched
                        | DispatchStatus::WaitingForReply
                )
            );
        }
        for msg_type in [
            MessageType::WorkerDone,
            MessageType::Question,
            MessageType::Reply,
            MessageType::Directive,
            MessageType::Escalation,
            MessageType::Status,
        ] {
            assert_eq!(MessageType::from_str(msg_type.as_str()).unwrap(), msg_type);
        }
        for p_state in [
            PointerState::Pending,
            PointerState::Delivered,
            PointerState::Acknowledged,
            PointerState::DeliveryStalled,
        ] {
            assert_eq!(PointerState::from_str(p_state.as_str()).unwrap(), p_state);
        }
    }

    #[test]
    fn lot_l_send_to_recovery_hint_quotes_spaced_agent_for_round_trip() {
        let base_disp = DispatchContext {
            id: "disp_1".into(),
            run_id: "run_1".into(),
            task_id: "task_1".into(),
            parent_terminal: "t1".into(),
            assignee_terminal: "t2".into(),
            terminal_created_ms: 1000,
            turn_at_dispatch: 0,
            agent_kind: "agy --model 'GPT-OSS 120B (Medium)'".into(),
            worktree_mode: "current".into(),
            worktree_path: None,
            depth: 1,
            status: DispatchStatus::Completed,
            idle_since_ms: None,
            blocked_since_ms: None,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };

        for status in [DispatchStatus::Completed, DispatchStatus::Superseded] {
            let mut disp = base_disp.clone();
            disp.status = status;
            let hint = send_to_recovery_hint(&disp);
            let cmd = hint
                .split('`')
                .find(|s| s.starts_with("tchelvis orch launch"))
                .expect("hint should contain backticked launch command");
            let parsed = shlex::split(cmd).expect("launch command in hint should be valid shell");
            let agent_pos = parsed
                .iter()
                .position(|t| t == "--agent")
                .expect("launch command must contain --agent");
            assert_eq!(parsed[agent_pos + 1], disp.agent_kind);
        }

        // Single safe token remains unquoted
        let mut simple = base_disp;
        simple.agent_kind = "claude".into();
        let simple_hint = send_to_recovery_hint(&simple);
        assert!(simple_hint.contains("--agent claude --prompt"));
    }
}
