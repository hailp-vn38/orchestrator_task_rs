//! Các câu lệnh ghi (INSERT / UPDATE / DELETE) trên bảng `task_schedules`.

use crate::error::{TaskError, TaskResult};
use crate::store::codec::{db_err, encode_links, now_ms};
use crate::store::TaskStore;
use crate::types::{
    Priority, ScheduleActionKind, ScheduleKind, ScheduleSessionMode, ScheduleStatus, ScheduleView,
    TaskScope,
};

pub(crate) struct NewScheduleRow {
    pub id: String,
    pub scope: TaskScope,
    pub title: String,
    pub notes: Option<String>,
    pub action_kind: ScheduleActionKind,
    pub flow_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_mode: ScheduleSessionMode,
    pub priority: Priority,
    pub related_wiki_links: Vec<String>,
    pub schedule_kind: ScheduleKind,
    pub due_at: Option<i64>,
    pub cron_expr: Option<String>,
    pub timezone: String,
    pub status: ScheduleStatus,
    pub next_run_at: Option<i64>,
}

impl TaskStore {
    pub(crate) async fn insert_schedule(&self, row: NewScheduleRow) -> TaskResult<ScheduleView> {
        let now = now_ms();
        let links_json = encode_links(&row.related_wiki_links);
        sqlx::query(
            "INSERT INTO task_schedules (
                id, owner_id, workspace_id, title, notes, action_kind, flow_id, agent_id,
                session_mode, priority, related_wiki_links, schedule_kind, due_at, cron_expr,
                timezone, status, next_run_at, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        )
        .bind(&row.id)
        .bind(&row.scope.owner_id)
        .bind(&row.scope.workspace_id)
        .bind(&row.title)
        .bind(&row.notes)
        .bind(row.action_kind.as_str())
        .bind(&row.flow_id)
        .bind(&row.agent_id)
        .bind(row.session_mode.as_str())
        .bind(row.priority.as_str())
        .bind(&links_json)
        .bind(row.schedule_kind.as_str())
        .bind(row.due_at)
        .bind(&row.cron_expr)
        .bind(&row.timezone)
        .bind(row.status.as_str())
        .bind(row.next_run_at)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("insert schedule"))?;

        Ok(ScheduleView {
            id: row.id,
            owner_id: row.scope.owner_id,
            workspace_id: row.scope.workspace_id,
            title: row.title,
            notes: row.notes,
            action_kind: row.action_kind,
            flow_id: row.flow_id,
            agent_id: row.agent_id,
            session_mode: row.session_mode,
            reused_session_id: None,
            priority: row.priority,
            related_wiki_links: row.related_wiki_links,
            schedule_kind: row.schedule_kind,
            due_at: row.due_at,
            cron_expr: row.cron_expr,
            timezone: row.timezone,
            status: row.status,
            resolution_note: None,
            next_run_at: row.next_run_at,
            last_run_at: None,
            last_task_id: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub(crate) async fn complete_schedule(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
        resolution_note: Option<&str>,
    ) -> TaskResult<ScheduleView> {
        let now = now_ms();
        let res = sqlx::query(
            "UPDATE task_schedules
             SET status = 'completed',
                 resolution_note = COALESCE(?4, resolution_note),
                 next_run_at = NULL,
                 updated_at = ?5
             WHERE id = ?1 AND owner_id = ?2 AND workspace_id = ?3",
        )
        .bind(schedule_id)
        .bind(&scope.owner_id)
        .bind(&scope.workspace_id)
        .bind(resolution_note)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("complete schedule"))?;

        if res.rows_affected() == 0 {
            return Err(TaskError::not_found(format!(
                "schedule or reminder '{schedule_id}' not found"
            )));
        }
        self.get_schedule(scope, schedule_id).await
    }

    pub(crate) async fn set_schedule_status(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
        status: ScheduleStatus,
        next_run_at: Option<i64>,
    ) -> TaskResult<ScheduleView> {
        let now = now_ms();
        let res = sqlx::query(
            "UPDATE task_schedules
             SET status = ?4, next_run_at = ?5, updated_at = ?6
             WHERE id = ?1 AND owner_id = ?2 AND workspace_id = ?3",
        )
        .bind(schedule_id)
        .bind(&scope.owner_id)
        .bind(&scope.workspace_id)
        .bind(status.as_str())
        .bind(next_run_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("set schedule status"))?;

        if res.rows_affected() == 0 {
            return Err(TaskError::not_found(format!(
                "schedule '{schedule_id}' not found"
            )));
        }
        self.get_schedule(scope, schedule_id).await
    }

    pub(crate) async fn delete_schedule(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
    ) -> TaskResult<()> {
        let res = sqlx::query(
            "DELETE FROM task_schedules WHERE id = ?1 AND owner_id = ?2 AND workspace_id = ?3",
        )
        .bind(schedule_id)
        .bind(&scope.owner_id)
        .bind(&scope.workspace_id)
        .execute(&self.pool)
        .await
        .map_err(db_err("delete schedule"))?;

        if res.rows_affected() == 0 {
            return Err(TaskError::not_found(format!(
                "schedule or reminder '{schedule_id}' not found"
            )));
        }
        Ok(())
    }

    /// Ghi nhận 1 lần kích hoạt lịch hẹn (cập nhật `last_run_at`, `last_task_id`, `reused_session_id`, `next_run_at` và `status`).
    pub(crate) async fn record_schedule_fired(
        &self,
        schedule_id: &str,
        fired_at: i64,
        next_status: ScheduleStatus,
        next_run_at: Option<i64>,
        spawned_task_id: Option<&str>,
        reused_session_id: Option<&str>,
    ) -> TaskResult<Option<ScheduleView>> {
        sqlx::query(
            "UPDATE task_schedules
             SET status = ?2,
                 next_run_at = ?3,
                 last_run_at = ?4,
                 last_task_id = COALESCE(?5, last_task_id),
                 reused_session_id = COALESCE(?6, reused_session_id),
                 updated_at = ?4
             WHERE id = ?1",
        )
        .bind(schedule_id)
        .bind(next_status.as_str())
        .bind(next_run_at)
        .bind(fired_at)
        .bind(spawned_task_id)
        .bind(reused_session_id)
        .execute(&self.pool)
        .await
        .map_err(db_err("record schedule fired"))?;

        self.get_schedule_unscoped(schedule_id).await
    }
}
