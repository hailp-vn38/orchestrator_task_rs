//! Các thao tác quản lý Schedule & Reminder (`create_schedule`, `complete_schedule`, `pause_schedule`, `resume_schedule`, `delete_schedule`, `list_schedules`).

use crate::error::TaskResult;
use crate::manager::TaskManager;
use crate::runtime::compute_next_cron_ms;
use crate::store::now_ms;
use crate::types::{
    CreateScheduleRequest, ScheduleFilter, ScheduleKind, ScheduleStatus, ScheduleView, TaskScope,
};

impl TaskManager {
    pub async fn create_schedule(&self, req: CreateScheduleRequest) -> TaskResult<ScheduleView> {
        let scope = &self.core.config.default_scope;
        self.core.create_schedule_internal(scope, req).await
    }

    pub async fn create_schedule_scoped(
        &self,
        scope: &TaskScope,
        req: CreateScheduleRequest,
    ) -> TaskResult<ScheduleView> {
        self.core.create_schedule_internal(scope, req).await
    }

    pub async fn complete_schedule(
        &self,
        schedule_id: &str,
        resolution_note: Option<&str>,
    ) -> TaskResult<ScheduleView> {
        let scope = &self.core.config.default_scope;
        self.complete_schedule_scoped(scope, schedule_id, resolution_note)
            .await
    }

    pub async fn complete_schedule_scoped(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
        resolution_note: Option<&str>,
    ) -> TaskResult<ScheduleView> {
        self.core.ensure_open()?;
        self.core
            .store
            .complete_schedule(scope, schedule_id, resolution_note)
            .await
    }

    pub async fn pause_schedule(&self, schedule_id: &str) -> TaskResult<ScheduleView> {
        let scope = &self.core.config.default_scope;
        self.pause_schedule_scoped(scope, schedule_id).await
    }

    pub async fn pause_schedule_scoped(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
    ) -> TaskResult<ScheduleView> {
        self.core.ensure_open()?;
        self.core
            .store
            .set_schedule_status(scope, schedule_id, ScheduleStatus::Paused, None)
            .await
    }

    pub async fn resume_schedule(&self, schedule_id: &str) -> TaskResult<ScheduleView> {
        let scope = &self.core.config.default_scope;
        self.resume_schedule_scoped(scope, schedule_id).await
    }

    pub async fn resume_schedule_scoped(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
    ) -> TaskResult<ScheduleView> {
        self.core.ensure_open()?;
        let existing = self.core.store.get_schedule(scope, schedule_id).await?;
        let now = now_ms();
        let (status, next_run_at) = match existing.schedule_kind {
            ScheduleKind::Cron => {
                let expr = existing.cron_expr.as_deref().unwrap_or("0 * * * *");
                let (_, next_ms) = compute_next_cron_ms(expr, &existing.timezone, now)?;
                (ScheduleStatus::Active, Some(next_ms))
            }
            ScheduleKind::Once => {
                let due = existing.due_at.unwrap_or(now);
                (ScheduleStatus::Pending, Some(due))
            }
        };
        let updated = self
            .core
            .store
            .set_schedule_status(scope, schedule_id, status, next_run_at)
            .await?;
        self.core.scheduler_wake.notify_one();
        Ok(updated)
    }

    pub async fn delete_schedule(&self, schedule_id: &str) -> TaskResult<()> {
        let scope = &self.core.config.default_scope;
        self.delete_schedule_scoped(scope, schedule_id).await
    }

    pub async fn delete_schedule_scoped(
        &self,
        scope: &TaskScope,
        schedule_id: &str,
    ) -> TaskResult<()> {
        self.core.ensure_open()?;
        self.core.store.delete_schedule(scope, schedule_id).await
    }

    pub async fn list_schedules(&self, filter: &ScheduleFilter) -> TaskResult<Vec<ScheduleView>> {
        let scope = &self.core.config.default_scope;
        self.list_schedules_scoped(scope, filter).await
    }

    pub async fn list_schedules_scoped(
        &self,
        scope: &TaskScope,
        filter: &ScheduleFilter,
    ) -> TaskResult<Vec<ScheduleView>> {
        self.core.ensure_open()?;
        self.core
            .store
            .list_schedules(scope, filter, self.core.config.runtime.page_size)
            .await
    }
}
