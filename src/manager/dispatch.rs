//! Các thao tác điều khiển Task (`create_task`, `send_followup`, `cancel_task`, `delete_task`).

use crate::error::TaskResult;
use crate::manager::TaskManager;
use crate::types::{CreateTaskRequest, FollowupView, TaskScope, TaskSource, TaskView};

impl TaskManager {
    /// Tạo và chạy ngay một Task với `default_scope` (`"personal"`, `"homelab"`).
    pub async fn create_task(&self, req: CreateTaskRequest) -> TaskResult<TaskView> {
        let scope = &self.core.config.default_scope;
        self.core.create_and_start_task(scope, req).await
    }

    /// Tạo và chạy ngay một Task với `TaskScope` chỉ định.
    pub async fn create_task_scoped(
        &self,
        scope: &TaskScope,
        req: CreateTaskRequest,
    ) -> TaskResult<TaskView> {
        self.core.create_and_start_task(scope, req).await
    }

    /// Gửi lệnh bổ sung (Followup) vào Task trong `default_scope`.
    pub async fn send_followup(
        &self,
        task_id: &str,
        prompt: impl Into<String>,
        source: TaskSource,
    ) -> TaskResult<FollowupView> {
        let scope = &self.core.config.default_scope;
        self.core
            .submit_or_enqueue_followup(scope, task_id, prompt.into(), source)
            .await
    }

    /// Gửi lệnh bổ sung (Followup) vào Task trong `TaskScope` chỉ định.
    pub async fn send_followup_scoped(
        &self,
        scope: &TaskScope,
        task_id: &str,
        prompt: impl Into<String>,
        source: TaskSource,
    ) -> TaskResult<FollowupView> {
        self.core
            .submit_or_enqueue_followup(scope, task_id, prompt.into(), source)
            .await
    }

    /// Hủy một Task đang chạy trong `default_scope`.
    pub async fn cancel_task(&self, task_id: &str) -> TaskResult<TaskView> {
        let scope = &self.core.config.default_scope;
        self.core.cancel_task_internal(scope, task_id).await
    }

    /// Hủy một Task đang chạy trong `TaskScope` chỉ định.
    pub async fn cancel_task_scoped(
        &self,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<TaskView> {
        self.core.cancel_task_internal(scope, task_id).await
    }

    /// Xóa một bản ghi Task trong `default_scope` (nếu đang chạy sẽ tự động cancel trước).
    pub async fn delete_task(&self, task_id: &str) -> TaskResult<()> {
        let scope = &self.core.config.default_scope;
        self.delete_task_scoped(scope, task_id).await
    }

    /// Xóa một bản ghi Task trong `TaskScope` chỉ định.
    pub async fn delete_task_scoped(&self, scope: &TaskScope, task_id: &str) -> TaskResult<()> {
        self.core.ensure_open()?;
        let _ = self.core.cancel_task_internal(scope, task_id).await?;
        self.core.store.delete_task(scope, task_id).await
    }
}
