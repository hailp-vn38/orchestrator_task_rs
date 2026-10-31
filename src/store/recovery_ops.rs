//! Các câu lệnh truy vấn phục vụ Startup Recovery & Auto-Flow deduplication.

use sqlx::Row;

use crate::error::TaskResult;
use crate::store::codec::{db_err, map_task_row};
use crate::store::TaskStore;
use crate::types::TaskView;

impl TaskStore {
    /// Lấy danh sách mọi Task đang ở trạng thái `'queued'`, `'running'` hoặc `'waiting'` lúc khởi động lại process.
    pub(crate) async fn list_active_tasks_unscoped(&self) -> TaskResult<Vec<TaskView>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks WHERE status IN ('queued', 'running', 'waiting') ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err("list active tasks for recovery"))?;

        Ok(rows.iter().map(map_task_row).collect())
    }

    /// Kiểm tra xem đã có Task `source = 'autoflow'` hoặc Task đang chạy trên `agent_id` của Auto-Flow
    /// hay chưa để tránh spawn trùng khi Agent đang thu thập nguồn (`source_collector`) hoặc đang biên dịch.
    pub(crate) async fn has_active_autoflow_task(
        &self,
        flow_id: &str,
        agent_id: &str,
    ) -> TaskResult<bool> {
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM tasks
             WHERE flow_id = ?1 AND (source = 'autoflow' OR agent_id = ?2) AND status IN ('queued', 'running', 'waiting')",
        )
        .bind(flow_id)
        .bind(agent_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err("check active autoflow task"))?;

        let cnt: i64 = row.get("cnt");
        Ok(cnt > 0)
    }
}
