//! Các thao tác điều khiển Task (`create_task`, `send_followup`, `cancel_task`, `delete_task`)
//! và xóa tài liệu Wiki/Source (`delete_wiki_file`, `delete_source_file`).

use crate::error::TaskResult;
use crate::manager::TaskManager;
use crate::types::{
    CreateTaskRequest, FollowupView, ResolveStuckTool, TaskScope, TaskSource, TaskView,
    WikiDeleteOutcome,
};

impl TaskManager {
    /// Tạo và chạy ngay một Task với `default_scope` (`"personal"`, `"homelab"`).
    pub async fn create_task(&self, req: CreateTaskRequest) -> TaskResult<TaskView> {
        let scope = &self.core.config.default_scope;
        self.core
            .create_and_start_task(scope, self.prepare_recipe_task(req)?)
            .await
    }

    /// Tạo và chạy ngay một Task với `TaskScope` chỉ định.
    pub async fn create_task_scoped(
        &self,
        scope: &TaskScope,
        req: CreateTaskRequest,
    ) -> TaskResult<TaskView> {
        self.core
            .create_and_start_task(scope, self.prepare_recipe_task(req)?)
            .await
    }

    fn prepare_recipe_task(&self, mut req: CreateTaskRequest) -> TaskResult<CreateTaskRequest> {
        let Some(recipe_request) = req.recipe.take() else {
            return Ok(req);
        };
        let recipe = self.recipes.get(&recipe_request.id).ok_or_else(|| {
            crate::error::TaskError::invalid_input(format!(
                "recipe '{}' is not registered",
                recipe_request.id
            ))
        })?;
        let metadata = recipe.metadata();
        let params = recipe
            .validate_params(&recipe_request.params)
            .map_err(|err| {
                crate::error::TaskError::invalid_input(format!(
                    "validation_path={}: {}",
                    err.validation_path, err.message
                ))
            })?;
        if req.title.trim().is_empty() {
            req.title = recipe
                .title_for_params(&params)
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| format!("{} v{}", metadata.title, metadata.version));
        }
        req.prompt = serde_json::to_string(&params).map_err(|e| {
            crate::error::TaskError::invalid_input(format!("serialize recipe parameters: {e}"))
        })?;
        req.target_agent = Some(format!("recipe_{}", metadata.id));
        Ok(req)
    }

    /// Gửi lệnh bổ sung (Followup) vào Task trong `default_scope`.
    pub async fn send_followup(
        &self,
        task_id: &str,
        prompt: impl Into<String>,
        source: TaskSource,
    ) -> TaskResult<FollowupView> {
        let scope = &self.core.config.default_scope;
        self.send_followup_scoped(scope, task_id, prompt, source)
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
        let prompt = prompt.into();
        if let Some(recipe_context) = self.recipe_followup_prompt(scope, task_id, &prompt).await? {
            return self
                .core
                .submit_or_enqueue_followup(
                    scope,
                    task_id,
                    prompt,
                    source,
                    Some("daily_root"),
                    Some(recipe_context),
                )
                .await;
        }
        self.core
            .submit_or_enqueue_followup(scope, task_id, prompt, source, None, None)
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

    async fn recipe_followup_prompt(
        &self,
        scope: &TaskScope,
        task_id: &str,
        prompt: &str,
    ) -> TaskResult<Option<String>> {
        let task = self.get_task_scoped(scope, task_id).await?;
        if !task.agent_id.starts_with("recipe_") {
            return Ok(None);
        }
        let detail = self.get_task_detail_scoped(scope, task_id).await?;
        let recipe = detail.recipe.as_ref().ok_or_else(|| {
            crate::error::TaskError::invalid_state(
                "the pinned task recipe is unavailable for follow-up context",
            )
        })?;
        if detail.task.followup_session_id.is_some() {
            return Ok(Some(prompt.to_string()));
        }
        let context = serde_json::json!({
            "task_recipe_followup": {
                "recipe": {"id": recipe.id, "version": recipe.version, "params": recipe.params},
                "progress": recipe.progress,
                "artifacts": detail.artifacts,
            },
            "followup": prompt,
        });
        serde_json::to_string_pretty(&context)
            .map(Some)
            .map_err(|error| {
                crate::error::TaskError::invalid_input(format!(
                    "serialize recipe follow-up context: {error}"
                ))
            })
    }

    /// Tiếp tục ngay một Task đang ở trạng thái chờ (`waiting_retry` hoặc `waiting_user`) trong `default_scope`.
    pub async fn resume_waiting_task(&self, task_id: &str) -> TaskResult<TaskView> {
        let scope = &self.core.config.default_scope;
        self.core.resume_waiting_task_internal(scope, task_id).await
    }

    /// Tiếp tục ngay một Task đang ở trạng thái chờ trong `TaskScope` chỉ định.
    pub async fn resume_waiting_task_scoped(
        &self,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<TaskView> {
        self.core.resume_waiting_task_internal(scope, task_id).await
    }

    /// Tiếp tục ngay một Task đang ở trạng thái chờ bất kể `TaskScope` (phục vụ Admin / Global View).
    pub async fn resume_waiting_task_any_scope(&self, task_id: &str) -> TaskResult<TaskView> {
        let scope = self.core.resolve_task_scope(task_id).await?;
        self.resume_waiting_task_scoped(&scope, task_id).await
    }

    /// Đối soát một tool bị kẹt ở trạng thái không rõ kết quả rồi cho Task chạy tiếp.
    pub async fn resolve_stuck_tool(&self, request: ResolveStuckTool) -> TaskResult<TaskView> {
        self.core.resolve_stuck_tool_internal(request).await
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
