//! Các câu lệnh truy vấn và cập nhật bảng `task_followups`.

use crate::error::TaskResult;
use crate::store::codec::{db_err, map_followup_row, new_id, now_ms};
use crate::store::TaskStore;
use crate::types::{FollowupStatus, FollowupView, TaskSource};

impl TaskStore {
    pub(crate) async fn enqueue_followup(
        &self,
        task_id: &str,
        prompt: &str,
        source: TaskSource,
    ) -> TaskResult<FollowupView> {
        let id = new_id();
        let now = now_ms();
        sqlx::query(
            "INSERT INTO task_followups (id, task_id, prompt, source, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
        )
        .bind(&id)
        .bind(task_id)
        .bind(prompt)
        .bind(source.as_str())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("enqueue followup"))?;

        Ok(FollowupView {
            id,
            task_id: task_id.to_string(),
            prompt: prompt.to_string(),
            source,
            status: FollowupStatus::Pending,
            submitted_turn_id: None,
            created_at: now,
            submitted_at: None,
        })
    }

    pub(crate) async fn next_pending_followup(
        &self,
        task_id: &str,
    ) -> TaskResult<Option<FollowupView>> {
        let row = sqlx::query(
            "SELECT * FROM task_followups
             WHERE task_id = ?1 AND status = 'pending'
             ORDER BY created_at ASC
             LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err("next pending followup"))?;

        Ok(row.as_ref().map(map_followup_row))
    }

    pub(crate) async fn mark_followup_submitted(
        &self,
        followup_id: &str,
        turn_id: &str,
    ) -> TaskResult<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE task_followups
             SET status = 'submitted', submitted_turn_id = ?2, submitted_at = ?3
             WHERE id = ?1",
        )
        .bind(followup_id)
        .bind(turn_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err("mark followup submitted"))?;
        Ok(())
    }

    pub(crate) async fn cancel_pending_followups(&self, task_id: &str) -> TaskResult<u64> {
        let res = sqlx::query(
            "UPDATE task_followups SET status = 'cancelled'
             WHERE task_id = ?1 AND status = 'pending'",
        )
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(db_err("cancel pending followups"))?;
        Ok(res.rows_affected())
    }

    pub(crate) async fn list_followups(&self, task_id: &str) -> TaskResult<Vec<FollowupView>> {
        let rows =
            sqlx::query("SELECT * FROM task_followups WHERE task_id = ?1 ORDER BY created_at ASC")
                .bind(task_id)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err("list followups"))?;

        Ok(rows.iter().map(map_followup_row).collect())
    }
}
