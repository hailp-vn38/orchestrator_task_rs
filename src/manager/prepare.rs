//! Pha 1 (`prepare`) và Pha 2 (`start` kèm startup recovery) của `TaskManager`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use orchestrator_agent_rs::{HostTool, Orchestrator};
use tokio_util::sync::CancellationToken;

use crate::config::{TaskConfig, DEFAULT_FLOW_ID};
use crate::error::{TaskError, TaskResult};
use crate::manager::{TaskManager, TaskManagerInit};
use crate::owner_lock::OwnerLock;
use crate::runtime::{
    autoflow, scheduler, watcher, FlowRegistry, SharedTaskCore, WikiAutoFlowBinding,
};
use crate::store::{TaskStore, TerminalUpdate, DB_FILE, OWNER_LOCK_FILE};
use crate::tools::host_adapter::{build_all_host_tools, build_reminder_host_tools};
use crate::types::TaskStatus;

impl TaskManager {
    /// Pha 1: Validate `TaskConfig`, tạo thư mục `store_dir`, lấy `OwnerLock`, và mở/migrate `tasks.db`.
    pub async fn prepare(config: TaskConfig) -> TaskResult<TaskManagerInit> {
        config.validate_local()?;
        tokio::fs::create_dir_all(&config.store_dir)
            .await
            .map_err(|e| TaskError::storage("create task store_dir", e))?;

        let lock_path = config.store_dir.join(OWNER_LOCK_FILE);
        let owner_lock = OwnerLock::acquire(&lock_path)?;

        let db_path = config.store_dir.join(DB_FILE);
        let store = TaskStore::open(&db_path).await?;
        let shutdown = CancellationToken::new();
        let core = Arc::new(SharedTaskCore::new(config, store, shutdown));

        Ok(TaskManagerInit {
            core,
            owner_lock: Some(owner_lock),
            flows: BTreeMap::new(),
            autoflow: None,
        })
    }
}

impl TaskManagerInit {
    /// Xuất 4 `HostTool` (`reminder_create`, `reminder_list`, `reminder_complete`, `reminder_delete`)
    /// để đăng ký vào `daily_orch` (Flow 2) trước khi gọi `OrchestratorBuilder::build()`.
    pub fn reminder_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        build_reminder_host_tools(Arc::clone(&self.core))
    }

    /// Xuất toàn bộ 9 `HostTool` (`task_*` + `reminder_*`).
    pub fn all_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        build_all_host_tools(Arc::clone(&self.core))
    }

    /// Gắn một instance `Orchestrator` hợp nhất duy nhất (`flow_id = "default"`).
    pub fn with_orchestrator(self, orchestrator: Arc<Orchestrator>) -> Self {
        self.register_flow(DEFAULT_FLOW_ID, orchestrator)
    }

    /// Gắn một instance `Orchestrator` với định danh `flow_id` (VD: `"daily"`, `"wiki"`).
    pub fn register_flow(
        mut self,
        flow_id: impl Into<String>,
        orchestrator: Arc<Orchestrator>,
    ) -> Self {
        self.flows.insert(flow_id.into(), orchestrator);
        self
    }

    /// Gắn cầu nối `WikiAutoFlowBinding` để tự động chuyển `autoflow_notify` thành các Task trên Flow `wiki`.
    pub fn with_wiki_autoflow(mut self, binding: WikiAutoFlowBinding) -> Self {
        self.autoflow = Some(binding);
        self
    }

    /// Pha 2: Kiểm tra danh mục Flow/Agent, đồng bộ (reconcile) các Task đang chạy dở trong `tasks.db`,
    /// và khởi chạy `Scheduler` + `AutoFlow` worker.
    pub async fn start(mut self) -> TaskResult<Arc<TaskManager>> {
        let registry = FlowRegistry::build(self.flows, self.core.config.default_target.clone())?;

        if let Some(ref af) = self.autoflow {
            let target_str = format!("{}/{}", af.flow_id, af.agent_id);
            registry.resolve_target(Some(&target_str))?;
        }

        let _ = self.core.flows.set(registry);

        // Reconcile các Task đang ở trạng thái 'queued' hoặc 'running' từ lần chạy trước
        reconcile_active_tasks(&self.core).await?;

        let mut bg_tasks = Vec::with_capacity(2);
        bg_tasks.push(scheduler::spawn_scheduler_loop(Arc::clone(&self.core)));

        if let Some(binding) = self.autoflow.take() {
            bg_tasks.push(autoflow::spawn_autoflow_loop(
                Arc::clone(&self.core),
                binding,
            ));
        }

        Ok(Arc::new(TaskManager {
            core: self.core,
            bg_tasks: Mutex::new(bg_tasks),
            owner_lock: Mutex::new(self.owner_lock.take()),
        }))
    }
}

async fn reconcile_active_tasks(core: &Arc<SharedTaskCore>) -> TaskResult<()> {
    let active_tasks = core.store.list_active_tasks_unscoped().await?;
    for task in active_tasks {
        match core.settle_or_advance_task(&task).await {
            Ok(true) => {
                // Task đã kết thúc trong Orchestrator và đã được cập nhật sang terminal state
            }
            Ok(false) => {
                // Task vẫn đang chạy (hoặc vừa được submit followup): gắn lại Watcher
                if let Ok(Some(refreshed)) = core.store.get_task_unscoped(&task.id).await {
                    if !refreshed.status.is_terminal() {
                        watcher::ensure_task_watcher(Arc::clone(core), refreshed);
                    }
                }
            }
            Err(_) => {
                // Flow hoặc session không còn tồn tại -> đánh dấu Interrupted
                let _ = core
                    .store
                    .finish_task(
                        &task.id,
                        TerminalUpdate {
                            status: TaskStatus::Interrupted,
                            output_preview: None,
                            error_code: Some("RECOVERY_FAILED"),
                            error_message: Some("Task session could not be reconciled on startup"),
                        },
                    )
                    .await;
            }
        }
    }
    Ok(())
}
