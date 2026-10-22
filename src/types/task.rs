//! Value types cho Task, Followup, và Agent Catalog.

use orchestrator_agent_rs::{FamilySnapshot, RunStatus};
use serde::{Deserialize, Serialize};

use crate::config::TargetAgent;
use crate::types::execution::{TaskAgentSessionView, TaskExecutionSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => Self::Failed,
        }
    }

    pub fn from_run_status(status: RunStatus) -> Self {
        match status {
            RunStatus::Running => Self::Running,
            RunStatus::Completed => Self::Completed,
            RunStatus::Cancelled => Self::Cancelled,
            RunStatus::Interrupted => Self::Interrupted,
            RunStatus::Failed | RunStatus::TimedOut => Self::Failed,
            _ => Self::Failed,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    Voice,
    Api,
    Chat,
    Schedule,
    Autoflow,
}

impl TaskSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Api => "api",
            Self::Chat => "chat",
            Self::Schedule => "schedule",
            Self::Autoflow => "autoflow",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "voice" => Self::Voice,
            "chat" => Self::Chat,
            "schedule" => Self::Schedule,
            "autoflow" => Self::Autoflow,
            _ => Self::Api,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub prompt: String,
    /// Có thể truyền `"daily/daily_root"`, `"wiki/wiki_root"`, hoặc chỉ `"daily_root"` / `"wiki_root"`.
    /// Nếu `None`, `TaskManager` dùng `default_target` hoặc Primary agent đầu tiên đã đăng ký.
    pub target_agent: Option<String>,
    pub source: TaskSource,
    pub schedule_id: Option<String>,
    /// Nếu truyền `session_id` có sẵn, Task sẽ submit turn mới vào session đó.
    pub existing_session_id: Option<String>,
    pub related_wiki_links: Vec<String>,
}

impl CreateTaskRequest {
    pub fn new(title: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            prompt: prompt.into(),
            target_agent: None,
            source: TaskSource::Api,
            schedule_id: None,
            existing_session_id: None,
            related_wiki_links: Vec::new(),
        }
    }

    pub fn with_target(mut self, target_agent: impl Into<String>) -> Self {
        self.target_agent = Some(target_agent.into());
        self
    }

    pub fn with_source(mut self, source: TaskSource) -> Self {
        self.source = source;
        self
    }

    pub fn with_wiki_links(mut self, links: Vec<String>) -> Self {
        self.related_wiki_links = links;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub owner_id: String,
    pub workspace_id: String,
    pub title: String,
    pub prompt: String,
    pub flow_id: String,
    pub agent_id: String,
    pub source: TaskSource,
    pub schedule_id: Option<String>,
    pub session_id: String,
    pub latest_turn_id: Option<String>,
    pub status: TaskStatus,
    pub active_subagent: Option<String>,
    pub active_tool: Option<String>,
    pub progress_summary: Option<String>,
    pub output_preview: Option<String>,
    pub related_wiki_links: Vec<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowupStatus {
    Pending,
    Submitted,
    Cancelled,
}

impl FollowupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitted => "submitted",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "submitted" => Self::Submitted,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowupView {
    pub id: String,
    pub task_id: String,
    pub prompt: String,
    pub source: TaskSource,
    pub status: FollowupStatus,
    pub submitted_turn_id: Option<String>,
    pub created_at: i64,
    pub submitted_at: Option<i64>,
}

/// Chi tiết đầy đủ của 1 Task trên UI View Task (gồm bản ghi Task, danh sách Followup,
/// tổng hợp số liệu `execution_summary`, chi tiết phiên của `root_agent` + `subagents`,
/// và `FamilySnapshot` gốc từ `Orchestrator`).
#[derive(Debug, Clone, Serialize)]
pub struct TaskDetailView {
    pub task: TaskView,
    pub followups: Vec<FollowupView>,
    pub execution_summary: Option<TaskExecutionSummary>,
    pub root_agent: Option<TaskAgentSessionView>,
    pub subagents: Vec<TaskAgentSessionView>,
    #[serde(skip)]
    pub family_snapshot: Option<FamilySnapshot>,
}

impl TaskDetailView {
    pub fn from_parts(
        task: TaskView,
        followups: Vec<FollowupView>,
        family_snapshot: Option<FamilySnapshot>,
    ) -> Self {
        let (execution_summary, root_agent, subagents) = match family_snapshot {
            Some(ref snap) => {
                let summary = TaskExecutionSummary::from_family_snapshot(snap);
                let root = TaskAgentSessionView::from_session_snapshot(&snap.root);
                let children = snap
                    .children
                    .iter()
                    .map(TaskAgentSessionView::from_session_snapshot)
                    .collect();
                (Some(summary), Some(root), children)
            }
            None => (None, None, Vec::new()),
        };

        Self {
            task,
            followups,
            execution_summary,
            root_agent,
            subagents,
            family_snapshot,
        }
    }

    /// Trả về danh sách mọi phiên Agent trong Task (Root Agent đứng đầu, tiếp theo là các Subagent).
    pub fn all_agents(&self) -> Vec<&TaskAgentSessionView> {
        let mut out = Vec::with_capacity(1 + self.subagents.len());
        if let Some(ref root) = self.root_agent {
            out.push(root);
        }
        out.extend(self.subagents.iter());
        out
    }

    /// Tìm phiên thực thi của một Agent theo `session_id` hoặc `agent_id`.
    pub fn find_agent_session(&self, session_or_agent_id: &str) -> Option<&TaskAgentSessionView> {
        let key = session_or_agent_id.trim();
        self.all_agents()
            .into_iter()
            .find(|s| s.session_id == key || s.agent_id == key)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskFilter {
    pub status: Option<TaskStatus>,
    pub source: Option<TaskSource>,
    pub flow_id: Option<String>,
    pub agent_id: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPage<T> {
    pub items: Vec<T>,
    pub total: u64,
}

/// Thông tin mô tả một Target Agent khả dụng (trích xuất động từ các `Orchestrator`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCatalogEntry {
    pub target: TargetAgent,
    pub description: String,
}
