//! Các câu lệnh truy vấn đọc (SELECT) trên bảng `tasks`.

use sqlx::{QueryBuilder, Row, Sqlite};

use crate::error::{TaskError, TaskResult};
use crate::store::codec::{db_err, map_task_row};
use crate::store::TaskStore;
use crate::types::{TaskFilter, TaskPage, TaskScope, TaskView};

impl TaskStore {
    pub(crate) async fn get_task(&self, scope: &TaskScope, task_id: &str) -> TaskResult<TaskView> {
        let include_personal_homelab = scope.owner_id.starts_with("device:");
        let row = sqlx::query(
            "SELECT * FROM tasks WHERE id = ?1 AND (
                (owner_id = ?2 AND workspace_id = ?3)
                OR (?4 = 1 AND owner_id = 'personal' AND workspace_id = 'homelab')
            )",
        )
        .bind(task_id)
        .bind(&scope.owner_id)
        .bind(&scope.workspace_id)
        .bind(i32::from(include_personal_homelab))
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
        self.list_tasks_internal(Some(scope), filter, max_page_size)
            .await
    }

    pub(crate) async fn list_tasks_all(
        &self,
        filter: &TaskFilter,
        max_page_size: u32,
    ) -> TaskResult<TaskPage<TaskView>> {
        self.list_tasks_internal(None, filter, max_page_size).await
    }

    async fn list_tasks_internal(
        &self,
        scope: Option<&TaskScope>,
        filter: &TaskFilter,
        max_page_size: u32,
    ) -> TaskResult<TaskPage<TaskView>> {
        let limit = filter
            .limit
            .unwrap_or(max_page_size)
            .clamp(1, max_page_size);
        let offset = filter.offset.unwrap_or(0);

        let mut count_qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT COUNT(*) as total FROM tasks WHERE 1=1");
        apply_scope_clause(&mut count_qb, scope);
        apply_task_filters(&mut count_qb, filter);

        let count_row = count_qb
            .build()
            .fetch_one(&self.pool)
            .await
            .map_err(db_err("count tasks"))?;
        let total: i64 = count_row.get("total");

        let mut list_qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT * FROM tasks WHERE 1=1");
        apply_scope_clause(&mut list_qb, scope);
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

fn apply_scope_clause<'a>(qb: &mut QueryBuilder<'a, Sqlite>, scope: Option<&'a TaskScope>) {
    if let Some(scope) = scope {
        if scope.owner_id.starts_with("device:") {
            qb.push(" AND ((owner_id = ");
            qb.push_bind(&scope.owner_id);
            qb.push(" AND workspace_id = ");
            qb.push_bind(&scope.workspace_id);
            qb.push(") OR (owner_id = 'personal' AND workspace_id = 'homelab'))");
        } else {
            qb.push(" AND owner_id = ");
            qb.push_bind(&scope.owner_id);
            qb.push(" AND workspace_id = ");
            qb.push_bind(&scope.workspace_id);
        }
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
    if let Some(q) = filter
        .query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let pattern = format!("%{}%", q.to_lowercase());
        qb.push(" AND (LOWER(id) LIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR LOWER(title) LIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR LOWER(prompt) LIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR LOWER(agent_id) LIKE ");
        qb.push_bind(pattern);
        qb.push(")");
    }
}
