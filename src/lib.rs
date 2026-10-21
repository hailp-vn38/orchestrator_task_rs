//! `orchestrator_task_rs` — Module quản lý Tác vụ Nền (Task), Hàng đợi Followup,
//! Lịch hẹn/Reminder (Cron & Timer) và Wiki Auto-Flow cho hệ thống Đa Agent (`Orchestrator`).

mod config;
mod error;
mod manager;
mod owner_lock;
mod runtime;
mod store;
mod tools;
mod types;

pub use config::{TargetAgent, TaskConfig, TaskRuntimeConfig, WorkspaceLayout};
pub use error::{ErrorCode, TaskError, TaskResult};
pub use manager::{TaskManager, TaskManagerInit};
pub use runtime::WikiAutoFlowBinding;
pub use tools::{TaskToolSpec, TaskToolSuite};
pub use types::{
    AgentCatalogEntry, CreateScheduleRequest, CreateTaskRequest, FollowupStatus, FollowupView,
    Priority, ScheduleActionKind, ScheduleFilter, ScheduleKind, ScheduleSessionMode,
    ScheduleStatus, ScheduleView, TaskDetailView, TaskEvent, TaskFeedItem, TaskFilter, TaskPage,
    TaskScope, TaskSource, TaskStatus, TaskSubscription, TaskView,
};
