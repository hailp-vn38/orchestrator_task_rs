//! Sự kiện real-time cho UI View Task & Notification Bus.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::types::{ScheduleView, TaskScope, TaskStatus, TaskView};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskEvent {
    TaskCreated {
        task: TaskView,
    },
    TaskProgress {
        task_id: String,
        owner_id: String,
        workspace_id: String,
        status: TaskStatus,
        active_subagent: Option<String>,
        active_tool: Option<String>,
        progress_summary: Option<String>,
        timestamp_ms: i64,
    },
    /// Sự kiện thực thi chi tiết từ `Orchestrator` (`text.delta`, `tool.started`, `tool.completed`, ...)
    /// gắn kèm `task_id` và `flow_id` để stream trực tiếp lên UI View Task hoặc CLI.
    TaskExecutionStream {
        task_id: String,
        flow_id: String,
        owner_id: String,
        workspace_id: String,
        event_type: String,
        session_id: String,
        root_session_id: String,
        payload: serde_json::Value,
        timestamp_ms: i64,
    },
    TaskTerminal {
        task: TaskView,
    },
    FollowupQueued {
        task_id: String,
        followup_id: String,
        owner_id: String,
        workspace_id: String,
    },
    ReminderDue {
        schedule: ScheduleView,
    },
    ScheduleTriggered {
        schedule: ScheduleView,
        spawned_task_id: String,
    },
}

impl TaskEvent {
    pub fn matches_scope(&self, scope: &TaskScope) -> bool {
        match self {
            Self::TaskCreated { task } | Self::TaskTerminal { task } => {
                task.owner_id == scope.owner_id && task.workspace_id == scope.workspace_id
            }
            Self::TaskProgress {
                owner_id,
                workspace_id,
                ..
            }
            | Self::TaskExecutionStream {
                owner_id,
                workspace_id,
                ..
            }
            | Self::FollowupQueued {
                owner_id,
                workspace_id,
                ..
            } => owner_id == &scope.owner_id && workspace_id == &scope.workspace_id,
            Self::ReminderDue { schedule } | Self::ScheduleTriggered { schedule, .. } => {
                schedule.owner_id == scope.owner_id && schedule.workspace_id == scope.workspace_id
            }
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum TaskFeedItem {
    Event(TaskEvent),
    ResyncRequired,
}

/// Stream sự kiện theo `TaskScope` cho UI View Task (SSE/WebSocket).
pub struct TaskSubscription {
    rx: broadcast::Receiver<TaskEvent>,
    scope: TaskScope,
    closed: CancellationToken,
    finished: bool,
}

impl TaskSubscription {
    pub(crate) fn new(
        rx: broadcast::Receiver<TaskEvent>,
        scope: TaskScope,
        closed: CancellationToken,
    ) -> Self {
        Self {
            rx,
            scope,
            closed,
            finished: false,
        }
    }

    pub async fn recv(&mut self) -> Option<TaskFeedItem> {
        if self.finished {
            return None;
        }
        loop {
            tokio::select! {
                biased;
                item = self.rx.recv() => match item {
                    Ok(ev) if ev.matches_scope(&self.scope) => {
                        return Some(TaskFeedItem::Event(ev));
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        self.finished = true;
                        return Some(TaskFeedItem::ResyncRequired);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        self.finished = true;
                        return None;
                    }
                },
                () = self.closed.cancelled() => {
                    self.finished = true;
                    return None;
                }
            }
        }
    }
}
