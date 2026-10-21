//! Value types cho Schedule & Reminder (ADR-0004).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleActionKind {
    /// Chỉ bắn sự kiện nhắc nhở (Reminder) cho người dùng kèm `[[WikiLinks]]`.
    NotifyOnly,
    /// Tự động tạo và chạy một `Task` trên `TargetAgent` khi đến giờ.
    SpawnTask,
}

impl ScheduleActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotifyOnly => "notify_only",
            Self::SpawnTask => "spawn_task",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "spawn_task" => Self::SpawnTask,
            _ => Self::NotifyOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    Once,
    Cron,
}

impl ScheduleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Cron => "cron",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "cron" => Self::Cron,
            _ => Self::Once,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleSessionMode {
    NewSession,
    ReuseSession,
}

impl ScheduleSessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewSession => "new_session",
            Self::ReuseSession => "reuse_session",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "reuse_session" => Self::ReuseSession,
            _ => Self::NewSession,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "high" => Self::High,
            _ => Self::Medium,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Pending,
    Active,
    Paused,
    Completed,
    Cancelled,
}

impl ScheduleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateScheduleRequest {
    pub title: String,
    pub notes: Option<String>,
    pub action_kind: ScheduleActionKind,
    pub target_agent: Option<String>,
    pub session_mode: ScheduleSessionMode,
    pub priority: Priority,
    pub related_wiki_links: Vec<String>,
    /// Thời điểm chạy 1 lần (RFC3339 / ISO-8601 hoặc epoch ms).
    pub due_at_iso: Option<String>,
    /// Hoặc độ trễ tính bằng giây kể từ hiện tại (cho one-shot timer nhanh từ Voice).
    pub delay_seconds: Option<u64>,
    /// Biểu thức Cron (5 hoặc 6 trường, VD: `"0 7 * * *"` hoặc `"0 0 7 * * *"`).
    pub cron_expr: Option<String>,
    pub timezone: Option<String>,
}

impl CreateScheduleRequest {
    pub fn reminder(title: impl Into<String>, due_at_iso: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            notes: None,
            action_kind: ScheduleActionKind::NotifyOnly,
            target_agent: None,
            session_mode: ScheduleSessionMode::NewSession,
            priority: Priority::Medium,
            related_wiki_links: Vec::new(),
            due_at_iso: Some(due_at_iso.into()),
            delay_seconds: None,
            cron_expr: None,
            timezone: None,
        }
    }

    pub fn scheduled_task(
        title: impl Into<String>,
        prompt: impl Into<String>,
        target_agent: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            notes: Some(prompt.into()),
            action_kind: ScheduleActionKind::SpawnTask,
            target_agent: Some(target_agent.into()),
            session_mode: ScheduleSessionMode::NewSession,
            priority: Priority::Medium,
            related_wiki_links: Vec::new(),
            due_at_iso: None,
            delay_seconds: None,
            cron_expr: None,
            timezone: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleView {
    pub id: String,
    pub owner_id: String,
    pub workspace_id: String,
    pub title: String,
    pub notes: Option<String>,
    pub action_kind: ScheduleActionKind,
    pub flow_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_mode: ScheduleSessionMode,
    pub reused_session_id: Option<String>,
    pub priority: Priority,
    pub related_wiki_links: Vec<String>,
    pub schedule_kind: ScheduleKind,
    pub due_at: Option<i64>,
    pub cron_expr: Option<String>,
    pub timezone: String,
    pub status: ScheduleStatus,
    pub resolution_note: Option<String>,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_task_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleFilter {
    /// `"pending"`, `"active"`, `"completed"`, `"all"`, ...
    pub status: Option<String>,
    pub action_kind: Option<ScheduleActionKind>,
    pub from_timestamp_ms: Option<i64>,
    pub to_timestamp_ms: Option<i64>,
    pub limit: Option<u32>,
}
