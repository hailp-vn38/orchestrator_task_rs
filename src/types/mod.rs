//! Public value types của `orchestrator_task_rs`, tách theo miền nghiệp vụ.

mod event;
mod schedule;
mod scope;
mod task;

pub use event::{TaskEvent, TaskFeedItem, TaskSubscription};
pub use schedule::{
    CreateScheduleRequest, Priority, ScheduleActionKind, ScheduleFilter, ScheduleKind,
    ScheduleSessionMode, ScheduleStatus, ScheduleView,
};
pub use scope::TaskScope;
pub use task::{
    AgentCatalogEntry, CreateTaskRequest, FollowupStatus, FollowupView, TaskDetailView, TaskFilter,
    TaskPage, TaskSource, TaskStatus, TaskView,
};
