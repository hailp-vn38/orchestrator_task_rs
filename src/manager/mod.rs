//! `TaskManager` & `TaskManagerInit` — Deep Module điều phối các Task, Followup,
//! Schedule/Reminder và Auto-Flow trên nhiều instance `Orchestrator`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use llm_wiki::WikiEngine;
use orchestrator_agent_rs::Orchestrator;
use tokio::task::JoinHandle;

use crate::owner_lock::OwnerLock;
use crate::runtime::{SharedTaskCore, WikiAutoFlowBinding};

mod dispatch;
mod lifecycle;
mod prepare;
mod query_api;
mod schedule_api;

/// Pha 1 của quá trình khởi tạo `TaskManager` (ADR-0002).
/// Cho phép lấy các `HostTool` (`reminder_*`, `task_*`) để đăng ký vào `OrchestratorBuilder`
/// trước khi gắn các `Arc<Orchestrator>` đã build và gọi [`Self::start`].
pub struct TaskManagerInit {
    pub(super) core: Arc<SharedTaskCore>,
    pub(super) owner_lock: Option<OwnerLock>,
    pub(super) flows: BTreeMap<String, Arc<Orchestrator>>,
    pub(super) wiki_engine: Option<Arc<WikiEngine>>,
    pub(super) autoflow: Option<WikiAutoFlowBinding>,
}

/// Deep Module quản lý vòng đời Tác vụ Nền, Hàng đợi Followup, Lịch hẹn/Reminder và Wiki Auto-Flow.
pub struct TaskManager {
    pub(super) core: Arc<SharedTaskCore>,
    pub(super) bg_tasks: Mutex<Vec<JoinHandle<()>>>,
    pub(super) owner_lock: Mutex<Option<OwnerLock>>,
}
