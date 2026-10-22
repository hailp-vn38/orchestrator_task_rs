//! Các hàm truy vấn trạng thái cho UI View Task, SSE/WS Event Subscription và `TaskToolSuite`.

use std::path::Path;
use std::sync::Arc;

use crate::error::TaskResult;
use crate::manager::TaskManager;
use crate::tools::TaskToolSuite;
use crate::types::{
    AgentCatalogEntry, TaskAgentMessageView, TaskDetailView, TaskFilter, TaskPage, TaskScope,
    TaskSubscription, TaskView,
};

impl TaskManager {
    /// Đường dẫn thư mục chứa `tasks.db` và `tasks.owner.lock`.
    pub fn store_dir(&self) -> &Path {
        &self.core.config.store_dir
    }

    /// Đường dẫn thư mục gốc của Wiki Vault (`data_root`), nếu đã cấu hình trong `TaskConfig`.
    pub fn wiki_vault_dir(&self) -> Option<&Path> {
        self.core.config.wiki_vault_dir.as_deref()
    }

    /// Lấy `TaskToolSuite` (hỗ trợ cả Raw JSON Handler cho Voice Agent và `HostTool`).
    pub fn voice_tool_suite(&self) -> TaskToolSuite {
        TaskToolSuite::new(Arc::clone(&self.core))
    }

    /// Danh sách các Primary/Root Agents kèm mô tả từ mọi Flow đã đăng ký.
    pub fn available_agents(&self) -> Vec<AgentCatalogEntry> {
        self.core
            .flows
            .get()
            .map(|f| f.catalog().to_vec())
            .unwrap_or_default()
    }

    /// Mở kênh nhận sự kiện real-time (`TaskEvent`) cho UI View Task trên `default_scope`.
    pub fn subscribe(&self) -> TaskResult<TaskSubscription> {
        let scope = self.core.config.default_scope.clone();
        self.subscribe_scoped(scope)
    }

    /// Mở kênh nhận sự kiện real-time (`TaskEvent`) cho UI View Task trên `TaskScope` chỉ định.
    pub fn subscribe_scoped(&self, scope: TaskScope) -> TaskResult<TaskSubscription> {
        self.core.ensure_open()?;
        scope.validate()?;
        Ok(self.core.event_bus.subscribe(scope))
    }

    /// Lấy thông tin một Task theo `task_id` trong `default_scope`.
    pub async fn get_task(&self, task_id: &str) -> TaskResult<TaskView> {
        let scope = &self.core.config.default_scope;
        self.get_task_scoped(scope, task_id).await
    }

    /// Lấy thông tin một Task theo `task_id` trong `TaskScope` chỉ định.
    pub async fn get_task_scoped(&self, scope: &TaskScope, task_id: &str) -> TaskResult<TaskView> {
        self.core.ensure_open()?;
        self.core.store.get_task(scope, task_id).await
    }

    /// Lấy chi tiết đầy đủ của một Task cho màn hình chi tiết trên UI View Task:
    /// gồm bản ghi `TaskView`, danh sách `FollowupView`, tổng hợp `execution_summary`,
    /// chi tiết phiên `root_agent` + `subagents`, và `FamilySnapshot` từ `Orchestrator`.
    pub async fn get_task_detail(&self, task_id: &str) -> TaskResult<TaskDetailView> {
        let scope = &self.core.config.default_scope;
        self.get_task_detail_scoped(scope, task_id).await
    }

    pub async fn get_task_detail_scoped(
        &self,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<TaskDetailView> {
        self.core.get_task_detail_internal(scope, task_id).await
    }

    /// Lấy lịch sử tin nhắn (`user`, `assistant`, `tool`) của Root Agent hoặc một Subagent cụ thể
    /// (truyền `session_id` hoặc `agent_id` vào `session_or_agent_id`; nếu `None` sẽ lấy của Root Agent)
    /// thuộc một Task trong `default_scope`.
    pub async fn get_task_agent_history(
        &self,
        task_id: &str,
        session_or_agent_id: Option<&str>,
        limit: Option<u32>,
    ) -> TaskResult<Vec<TaskAgentMessageView>> {
        let scope = &self.core.config.default_scope;
        self.get_task_agent_history_scoped(scope, task_id, session_or_agent_id, limit)
            .await
    }

    /// Lấy lịch sử tin nhắn của Root Agent hoặc Subagent thuộc một Task trong `TaskScope` chỉ định.
    pub async fn get_task_agent_history_scoped(
        &self,
        scope: &TaskScope,
        task_id: &str,
        session_or_agent_id: Option<&str>,
        limit: Option<u32>,
    ) -> TaskResult<Vec<TaskAgentMessageView>> {
        self.core
            .get_task_agent_history_internal(scope, task_id, session_or_agent_id, limit)
            .await
    }

    /// Liệt kê các Task theo bộ lọc cho màn hình danh sách của UI View Task (`default_scope`).
    pub async fn list_tasks(&self, filter: &TaskFilter) -> TaskResult<TaskPage<TaskView>> {
        let scope = &self.core.config.default_scope;
        self.list_tasks_scoped(scope, filter).await
    }

    /// Liệt kê các Task theo bộ lọc cho `TaskScope` chỉ định.
    pub async fn list_tasks_scoped(
        &self,
        scope: &TaskScope,
        filter: &TaskFilter,
    ) -> TaskResult<TaskPage<TaskView>> {
        self.core.ensure_open()?;
        self.core
            .store
            .list_tasks(scope, filter, self.core.config.runtime.page_size)
            .await
    }
}
