//! Các câu lệnh truy vấn đọc (SELECT) trên bảng `task_schedules`.

use sqlx::{QueryBuilder, Row, Sqlite};

use crate::error::{TaskError, TaskResult};
use crate::store::codec::{db_err, map_schedule_row};
use crate::store::TaskStore;
use crate::types::{ScheduleFilter, ScheduleView, TaskScope};

impl TaskStore {
    pub(crate) async fn get_schedule(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
    ) -> TaskResult<ScheduleView> {
        let row = sqlx::query(
            "SELECT * FROM task_schedules WHERE id = ?1 AND owner_id = ?2 AND workspace_id = ?3",
        )
        .bind(schedule_id)
        .bind(&scope.owner_id)
        .bind(&scope.workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err("get schedule"))?
        .ok_or_else(|| {
            TaskError::not_found(format!("schedule or reminder '{schedule_id}' not found"))
        })?;

        Ok(map_schedule_row(&row))
    }

    pub(crate) async fn get_schedule_unscoped(
        &self,
        schedule_id: &str,
    ) -> TaskResult<Option<ScheduleView>> {
        let row = sqlx::query("SELECT * FROM task_schedules WHERE id = ?1")
            .bind(schedule_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err("get schedule unscoped"))?;
        Ok(row.as_ref().map(map_schedule_row))
    }

    pub(crate) async fn list_schedules(
        &self,
        scope: &TaskScope,
        filter: &ScheduleFilter,
        max_page_size: u32,
    ) -> TaskResult<Vec<ScheduleView>> {
        let limit = filter
            .limit
            .unwrap_or(max_page_size)
            .clamp(1, max_page_size);
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT * FROM task_schedules WHERE owner_id = ");
        qb.push_bind(&scope.owner_id);
        qb.push(" AND workspace_id = ");
        qb.push_bind(&scope.workspace_id);

        if let Some(ref status) = filter.status {
            let s = status.trim().to_ascii_lowercase();
            if !s.is_empty() && s != "all" {
                if s == "pending" {
                    qb.push(" AND status IN ('pending', 'active')");
                } else {
                    qb.push(" AND status = ");
                    qb.push_bind(s);
                }
            }
        }
        if let Some(kind) = filter.action_kind {
            qb.push(" AND action_kind = ");
            qb.push_bind(kind.as_str());
        }
        if let Some(from_ts) = filter.from_timestamp_ms {
            qb.push(" AND COALESCE(next_run_at, due_at, created_at) >= ");
            qb.push_bind(from_ts);
        }
        if let Some(to_ts) = filter.to_timestamp_ms {
            qb.push(" AND COALESCE(next_run_at, due_at, created_at) <= ");
            qb.push_bind(to_ts);
        }

        qb.push(" ORDER BY COALESCE(next_run_at, due_at, created_at) ASC LIMIT ");
        qb.push_bind(i64::from(limit));

        let rows = qb
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(db_err("list schedules"))?;

        Ok(rows.iter().map(map_schedule_row).collect())
    }

    /// Lấy tất cả lịch hẹn đã đến hạn (`next_run_at <= now_ms`) đang ở trạng thái `'pending'` hoặc `'active'`.
    pub(crate) async fn due_schedules(&self, now_ms: i64) -> TaskResult<Vec<ScheduleView>> {
        let rows = sqlx::query(
            "SELECT * FROM task_schedules
             WHERE status IN ('pending', 'active')
               AND next_run_at IS NOT NULL
               AND next_run_at <= ?1
             ORDER BY next_run_at ASC
             LIMIT 32",
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err("fetch due schedules"))?;

        Ok(rows.iter().map(map_schedule_row).collect())
    }

    /// Thời điểm `next_run_at` sớm nhất tiếp theo để vòng lặp Scheduler đặt timer chính xác.
    pub(crate) async fn next_due_timestamp(&self) -> TaskResult<Option<i64>> {
        let row = sqlx::query(
            "SELECT MIN(next_run_at) as min_due
             FROM task_schedules
             WHERE status IN ('pending', 'active') AND next_run_at IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_err("fetch next due timestamp"))?;

        let min_due: Option<i64> = row.get("min_due");
        Ok(min_due)
    }
}
