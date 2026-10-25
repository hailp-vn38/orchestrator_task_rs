//! Các thao tác điều khiển Task (`create_task`, `send_followup`, `cancel_task`, `delete_task`)
//! và xóa tài liệu Wiki/Source (`delete_wiki_file`, `delete_source_file`).

use crate::error::TaskResult;
use crate::manager::TaskManager;
use crate::types::{
    CreateTaskRequest, FollowupView, TaskScope, TaskSource, TaskView, WikiDeleteOutcome,
};

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

    /// Gửi lệnh bổ sung (Followup) vào Task bất kể `TaskScope` (phục vụ Admin / Global View).
    pub async fn send_followup_any_scope(
        &self,
        task_id: &str,
        prompt: impl Into<String>,
        source: TaskSource,
    ) -> TaskResult<FollowupView> {
        let scope = self.core.resolve_task_scope(task_id).await?;
        self.send_followup_scoped(&scope, task_id, prompt, source)
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

    /// Hủy một Task đang chạy bất kể `TaskScope` (phục vụ Admin / Global View).
    pub async fn cancel_task_any_scope(&self, task_id: &str) -> TaskResult<TaskView> {
        let scope = self.core.resolve_task_scope(task_id).await?;
        self.cancel_task_scoped(&scope, task_id).await
    }

    /// Xóa vĩnh viễn một Task trong `default_scope`:
    /// tự động cancel nếu đang chạy, xóa bản ghi trong `tasks.db` + `task_followups`,
    /// xóa Root/Subagent session tương ứng trong `Orchestrator` và phát `TaskEvent::TaskDeleted`.
    pub async fn delete_task(&self, task_id: &str) -> TaskResult<()> {
        let scope = &self.core.config.default_scope;
        self.delete_task_scoped(scope, task_id).await
    }

    /// Xóa vĩnh viễn một Task trong `TaskScope` chỉ định.
    pub async fn delete_task_scoped(&self, scope: &TaskScope, task_id: &str) -> TaskResult<()> {
        self.core.delete_task_internal(scope, task_id).await
    }

    /// Xóa vĩnh viễn một Task bất kể `TaskScope` (phục vụ Admin / Global View).
    pub async fn delete_task_any_scope(&self, task_id: &str) -> TaskResult<()> {
        let scope = self.core.resolve_task_scope(task_id).await?;
        self.delete_task_scoped(&scope, task_id).await
    }

    /// Xóa một trang trong `wiki/` (VD: `"entities/rust/tokio.md"` hoặc `"wiki/entities/rust/tokio.md"`),
    /// tự động dọn thư mục cha rỗng, xóa khỏi `.wiki/index.sqlite` và đồng bộ lại `wiki/index.md`.
    pub fn delete_wiki_file(&self, rel_path: &str) -> TaskResult<WikiDeleteOutcome> {
        self.core.delete_wiki_file_internal(rel_path)
    }

    /// Xóa một file nguồn trong `sources/` (VD: `"docs/flow/lib.md"` hoặc `"sources/docs/flow/lib.md"`).
    /// Nếu `delete_wiki_mirror = true`, tự động xóa luôn trang tóm tắt đối ứng tại `wiki/sources/<rel_path>`
    /// và đồng bộ lại `wiki/index.md`.
    pub fn delete_source_file(
        &self,
        rel_path: &str,
        delete_wiki_mirror: bool,
    ) -> TaskResult<WikiDeleteOutcome> {
        self.core
            .delete_source_file_internal(rel_path, delete_wiki_mirror)
    }
}
