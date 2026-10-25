//! Các câu lệnh ghi (INSERT / UPDATE / DELETE) trên bảng `tasks`.

use crate::error::{TaskError, TaskResult};
use crate::store::codec::{db_err, encode_links, now_ms};
use crate::store::TaskStore;
use crate::types::{TaskScope, TaskSource, TaskStatus, TaskView};

pub(crate) struct NewTaskRow {
    pub id: String,
    pub scope: TaskScope,
    pub title: String,
    pub prompt: String,
    pub flow_id: String,
    pub agent_id: String,
    pub source: TaskSource,
    pub schedule_id: Option<String>,
    pub session_id: String,
    pub latest_turn_id: Option<String>,
    pub status: TaskStatus,
    pub progress_summary: Option<String>,
    pub related_wiki_links: Vec<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct ProgressUpdate<'a> {
    pub active_subagent: Option<&'a str>,
    pub active_tool: Option<&'a str>,
    pub progress_summary: Option<&'a str>,
}

pub(crate) struct TerminalUpdate<'a> {
    pub status: TaskStatus,
    pub output_preview: Option<&'a str>,
    pub error_code: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

impl TaskStore {
    pub(crate) async fn insert_task(&self, row: NewTaskRow) -> TaskResult<TaskView> {
        let now = now_ms();
        let started_at = if row.status == TaskStatus::Running {
            Some(now)
        } else {
            None
        };
        let links_json = encode_links(&row.related_wiki_links);
        sqlx::query(
            "INSERT INTO tasks (
                id, owner_id, workspace_id, title, prompt, flow_id, agent_id, source,
                schedule_id, session_id, latest_turn_id, status, progress_summary,
                related_wiki_links, created_at, started_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )
        .bind(&row.id)
        .bind(&row.scope.owner_id)
        .bind(&row.scope.workspace_id)
        .bind(&row.title)
        .bind(&row.prompt)
        .bind(&row.flow_id)
        .bind(&row.agent_id)
        .bind(row.source.as_str())
        .bind(&row.schedule_id)
        .bind(&row.session_id)
        .bind(&row.latest_turn_id)
        .bind(row.status.as_str())
        .bind(&row.progress_summary)
        .bind(&links_json)
        .bind(now)
        .bind(started_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("insert task"))?;

        Ok(TaskView {
            id: row.id,
            owner_id: row.scope.owner_id,
            workspace_id: row.scope.workspace_id,
            title: row.title,
            prompt: row.prompt,
            flow_id: row.flow_id,
            agent_id: row.agent_id,
            source: row.source,
            schedule_id: row.schedule_id,
            session_id: row.session_id,
            latest_turn_id: row.latest_turn_id,
            status: row.status,
            active_subagent: None,
            active_tool: None,
            progress_summary: row.progress_summary,
            output_preview: None,
            related_wiki_links: row.related_wiki_links,
            error_code: None,
            error_message: None,
            created_at: now,
            started_at,
            updated_at: now,
            finished_at: None,
        })
    }

    pub(crate) async fn update_task_progress(
        &self,
        task_id: &str,
        update: ProgressUpdate<'_>,
    ) -> TaskResult<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE tasks
             SET active_subagent = COALESCE(?2, active_subagent),
                 active_tool = ?3,
                 progress_summary = COALESCE(?4, progress_summary),
                 updated_at = ?5
             WHERE id = ?1 AND status IN ('queued', 'running')",
        )
        .bind(task_id)
        .bind(update.active_subagent)
        .bind(update.active_tool)
        .bind(update.progress_summary)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("update task progress"))?;
        Ok(())
    }

    pub(crate) async fn mark_task_turn_running(
        &self,
        task_id: &str,
        turn_id: &str,
        progress_summary: &str,
    ) -> TaskResult<Option<TaskView>> {
        let now = now_ms();
        sqlx::query(
            "UPDATE tasks
             SET latest_turn_id = ?2,
                 status = 'running',
                 active_subagent = NULL,
                 active_tool = NULL,
                 progress_summary = ?3,
                 error_code = NULL,
                 error_message = NULL,
                 started_at = COALESCE(started_at, ?4),
                 finished_at = NULL,
                 updated_at = ?4
             WHERE id = ?1",
        )
        .bind(task_id)
        .bind(turn_id)
        .bind(progress_summary)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("mark task turn running"))?;
        self.get_task_unscoped(task_id).await
    }

    pub(crate) async fn finish_task(
        &self,
        task_id: &str,
        term: TerminalUpdate<'_>,
    ) -> TaskResult<Option<TaskView>> {
        let now = now_ms();
        let summary = match term.status {
            TaskStatus::Completed => "Completed",
            TaskStatus::Cancelled => "Cancelled",
            TaskStatus::Interrupted => "Interrupted",
            _ => "Failed",
        };
        sqlx::query(
            "UPDATE tasks
             SET status = ?2,
                 active_subagent = NULL,
                 active_tool = NULL,
                 progress_summary = ?3,
                 output_preview = COALESCE(?4, output_preview),
                 error_code = ?5,
                 error_message = ?6,
                 updated_at = ?7,
                 finished_at = ?7
             WHERE id = ?1",
        )
        .bind(task_id)
        .bind(term.status.as_str())
        .bind(summary)
        .bind(term.output_preview)
        .bind(term.error_code)
        .bind(term.error_message)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("finish task"))?;
        self.get_task_unscoped(task_id).await
    }

    pub(crate) async fn update_task_summary(&self, task_id: &str, summary: &str) -> TaskResult<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE tasks
             SET progress_summary = ?2,
                 updated_at = ?3
             WHERE id = ?1",
        )
        .bind(task_id)
        .bind(summary)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("update task summary"))?;
        Ok(())
    }

    pub(crate) async fn delete_task(&self, scope: &TaskScope, task_id: &str) -> TaskResult<()> {
        let res =
            sqlx::query("DELETE FROM tasks WHERE id = ?1 AND owner_id = ?2 AND workspace_id = ?3")
                .bind(task_id)
                .bind(&scope.owner_id)
                .bind(&scope.workspace_id)
                .execute(&self.pool)
                .await
                .map_err(db_err("delete task"))?;
        if res.rows_affected() == 0 {
            return Err(TaskError::not_found(format!("task '{task_id}' not found")));
        }
        Ok(())
    }
}
