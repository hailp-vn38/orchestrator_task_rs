//! Các câu lệnh truy vấn đọc (SELECT) trên bảng `tasks`.

use sqlx::{QueryBuilder, Row, Sqlite};

use crate::error::{TaskError, TaskResult};
use crate::store::codec::{db_err, map_task_row};
use crate::store::TaskStore;
use crate::types::{TaskFilter, TaskPage, TaskScope, TaskView};

impl TaskStore {
    pub(crate) async fn get_task(&self, scope: &TaskScope, task_id: &str) -> TaskResult<TaskView> {
        let row = sqlx::query(
            "SELECT * FROM tasks WHERE id = ?1 AND owner_id = ?2 AND workspace_id = ?3",
        )
        .bind(task_id)
        .bind(&scope.owner_id)
        .bind(&scope.workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err("get task"))?
        .ok_or_else(|| TaskError::not_found(format!("task '{task_id}' not found")))?;

        Ok(map_task_row(&row))
    }

    pub(crate) async fn get_task_unscoped(&self, task_id: &str) -> TaskResult<Option<TaskView>> {
        let row = sqlx::query("SELECT * FROM tasks WHERE id = ?1")
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err("get task unscoped"))?;
        Ok(row.as_ref().map(map_task_row))
    }

    pub(crate) async fn count_tasks_for_session(
        &self,
        flow_id: &str,
        session_id: &str,
    ) -> TaskResult<i64> {
        let row =
            sqlx::query("SELECT COUNT(*) as cnt FROM tasks WHERE flow_id = ?1 AND session_id = ?2")
                .bind(flow_id)
                .bind(session_id)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err("count tasks for session"))?;
        Ok(row.get("cnt"))
    }

    pub(crate) async fn list_tasks(
        &self,
        scope: &TaskScope,
        filter: &TaskFilter,
        max_page_size: u32,
    ) -> TaskResult<TaskPage<TaskView>> {
        let limit = filter
            .limit
            .unwrap_or(max_page_size)
            .clamp(1, max_page_size);
        let offset = filter.offset.unwrap_or(0);

        let mut count_qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT COUNT(*) as total FROM tasks WHERE owner_id = ");
        count_qb.push_bind(&scope.owner_id);
        count_qb.push(" AND workspace_id = ");
        count_qb.push_bind(&scope.workspace_id);
        apply_task_filters(&mut count_qb, filter);

        let count_row = count_qb
            .build()
            .fetch_one(&self.pool)
            .await
            .map_err(db_err("count tasks"))?;
        let total: i64 = count_row.get("total");

        let mut list_qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT * FROM tasks WHERE owner_id = ");
        list_qb.push_bind(&scope.owner_id);
        list_qb.push(" AND workspace_id = ");
        list_qb.push_bind(&scope.workspace_id);
        apply_task_filters(&mut list_qb, filter);
        list_qb.push(" ORDER BY created_at DESC LIMIT ");
        list_qb.push_bind(i64::from(limit));
        list_qb.push(" OFFSET ");
        list_qb.push_bind(i64::from(offset));

        let rows = list_qb
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(db_err("list tasks"))?;

        let items = rows.iter().map(map_task_row).collect();
        Ok(TaskPage {
            items,
            total: u64::try_from(total).unwrap_or(0),
        })
    }
}

fn apply_task_filters<'a>(qb: &mut QueryBuilder<'a, Sqlite>, filter: &'a TaskFilter) {
    if let Some(status) = filter.status {
        qb.push(" AND status = ");
        qb.push_bind(status.as_str());
    }
    if let Some(source) = filter.source {
        qb.push(" AND source = ");
        qb.push_bind(source.as_str());
    }
    if let Some(ref flow_id) = filter.flow_id {
        qb.push(" AND flow_id = ");
        qb.push_bind(flow_id.as_str());
    }
    if let Some(ref agent_id) = filter.agent_id {
        qb.push(" AND agent_id = ");
        qb.push_bind(agent_id.as_str());
    }
}
