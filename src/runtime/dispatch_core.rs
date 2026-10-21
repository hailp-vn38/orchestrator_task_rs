//! Logic điều phối lõi trên `Arc<SharedTaskCore>`, dùng chung cho `TaskManager`, `Scheduler`,
//! `AutoFlow` và các `HostTool`.

use std::sync::Arc;

use orchestrator_agent_rs::{ErrorCode as OrchErrorCode, RunStatus};

use crate::error::{TaskError, TaskResult};
use crate::runtime::{compute_next_cron_ms, parse_due_at_ms, watcher, SharedTaskCore};
use crate::store::{new_id, now_ms, NewScheduleRow, NewTaskRow, TerminalUpdate};
use crate::types::{
    CreateScheduleRequest, CreateTaskRequest, FollowupStatus, FollowupView, ScheduleActionKind,
    ScheduleKind, ScheduleStatus, ScheduleView, TaskEvent, TaskScope, TaskSource, TaskStatus,
    TaskView,
};

impl SharedTaskCore {
    /// Tạo Task mới trên Flow tương ứng, gọi `Orchestrator::submit`, lưu vào `tasks.db`
    /// và khởi chạy Watcher theo dõi tiến độ.
    pub(crate) async fn create_and_start_task(
        self: &Arc<Self>,
        scope: &TaskScope,
        req: CreateTaskRequest,
    ) -> TaskResult<TaskView> {
        self.ensure_open()?;
        scope.validate()?;

        let title = req.title.trim();
        let prompt = req.prompt.trim();
        if title.is_empty() {
            return Err(TaskError::invalid_input("task title must not be empty"));
        }
        if prompt.is_empty() {
            return Err(TaskError::invalid_input("task prompt must not be empty"));
        }

        let (target, orch) = self.flows()?.resolve_target(req.target_agent.as_deref())?;
        let caller = scope.to_caller_context();

        let session_id = match req.existing_session_id {
            Some(ref sid) if !sid.trim().is_empty() => sid.trim().to_string(),
            _ => orch.create_session(&caller, &target.agent_id).await?,
        };

        let task_id = new_id();
        match orch.submit(&caller, &session_id, prompt.to_string()).await {
            Ok(accepted) => {
                let task = self
                    .store
                    .insert_task(NewTaskRow {
                        id: task_id.clone(),
                        scope: scope.clone(),
                        title: title.to_string(),
                        prompt: prompt.to_string(),
                        flow_id: target.flow_id,
                        agent_id: target.agent_id,
                        source: req.source,
                        schedule_id: req.schedule_id,
                        session_id,
                        latest_turn_id: Some(accepted.turn_id),
                        status: TaskStatus::Running,
                        progress_summary: Some("Starting agent turn...".to_string()),
                        related_wiki_links: req.related_wiki_links,
                    })
                    .await?;
                self.event_bus
                    .emit(TaskEvent::TaskCreated { task: task.clone() });
                watcher::ensure_task_watcher(Arc::clone(self), task.clone());
                Ok(task)
            }
            Err(err) if err.code() == OrchErrorCode::SessionBusy => {
                // Nếu tái sử dụng session đang bận, tạo task ở trạng thái 'queued' kèm followup chờ
                let task = self
                    .store
                    .insert_task(NewTaskRow {
                        id: task_id.clone(),
                        scope: scope.clone(),
                        title: title.to_string(),
                        prompt: prompt.to_string(),
                        flow_id: target.flow_id,
                        agent_id: target.agent_id,
                        source: req.source,
                        schedule_id: req.schedule_id,
                        session_id,
                        latest_turn_id: None,
                        status: TaskStatus::Queued,
                        progress_summary: Some(
                            "Queued (waiting for active session)...".to_string(),
                        ),
                        related_wiki_links: req.related_wiki_links,
                    })
                    .await?;
                self.store
                    .enqueue_followup(&task.id, prompt, req.source)
                    .await?;
                self.event_bus
                    .emit(TaskEvent::TaskCreated { task: task.clone() });
                watcher::ensure_task_watcher(Arc::clone(self), task.clone());
                Ok(task)
            }
            Err(err) => Err(TaskError::from(err)),
        }
    }

    /// Gửi lệnh bổ sung (Followup) vào một Task đã có:
    /// - Nếu session đang rảnh -> `submit` ngay lập tức và mở lại Watcher.
    /// - Nếu session đang `Running` (`SESSION_BUSY`) -> lưu vào bảng `task_followups` để Watcher tự `submit` khi turn xong.
    pub(crate) async fn submit_or_enqueue_followup(
        self: &Arc<Self>,
        scope: &TaskScope,
        task_id: &str,
        prompt: String,
        source: TaskSource,
    ) -> TaskResult<FollowupView> {
        self.ensure_open()?;
        let clean_prompt = prompt.trim();
        if clean_prompt.is_empty() {
            return Err(TaskError::invalid_input(
                "followup prompt must not be empty",
            ));
        }

        let task = self.store.get_task(scope, task_id).await?;
        let orch = self.flows()?.get_flow(&task.flow_id)?.clone();
        let caller = scope.to_caller_context();

        let followup = self
            .store
            .enqueue_followup(&task.id, clean_prompt, source)
            .await?;

        if task.status != TaskStatus::Running {
            if let Ok(accepted) = orch
                .submit(&caller, &task.session_id, clean_prompt.to_string())
                .await
            {
                self.store
                    .mark_followup_submitted(&followup.id, &accepted.turn_id)
                    .await?;
                if let Some(updated_task) = self
                    .store
                    .mark_task_turn_running(
                        &task.id,
                        &accepted.turn_id,
                        "Running follow-up turn...",
                    )
                    .await?
                {
                    watcher::ensure_task_watcher(Arc::clone(self), updated_task);
                }
                return Ok(FollowupView {
                    status: FollowupStatus::Submitted,
                    submitted_turn_id: Some(accepted.turn_id),
                    submitted_at: Some(now_ms()),
                    ..followup
                });
            }
        }

        self.event_bus.emit(TaskEvent::FollowupQueued {
            task_id: task.id.clone(),
            followup_id: followup.id.clone(),
            owner_id: scope.owner_id.clone(),
            workspace_id: scope.workspace_id.clone(),
        });
        watcher::ensure_task_watcher(Arc::clone(self), task);
        Ok(followup)
    }

    /// Hủy một Task đang chạy: hủy các followup đang chờ và gọi `Orchestrator::cancel`.
    pub(crate) async fn cancel_task_internal(
        self: &Arc<Self>,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<TaskView> {
        self.ensure_open()?;
        let task = self.store.get_task(scope, task_id).await?;
        self.store.cancel_pending_followups(&task.id).await?;

        if !task.status.is_terminal() {
            let orch = self.flows()?.get_flow(&task.flow_id)?.clone();
            let caller = scope.to_caller_context();
            let _ = orch.cancel(&caller, &task.session_id).await;
            if let Some(updated) = self
                .store
                .finish_task(
                    &task.id,
                    TerminalUpdate {
                        status: TaskStatus::Cancelled,
                        output_preview: None,
                        error_code: Some("CANCELLED"),
                        error_message: Some("Cancelled by caller"),
                    },
                )
                .await?
            {
                self.event_bus.emit(TaskEvent::TaskTerminal {
                    task: updated.clone(),
                });
                return Ok(updated);
            }
        }
        Ok(task)
    }

    /// Tạo Schedule hoặc Reminder mới trong `task_schedules` và đánh thức `Scheduler`.
    pub(crate) async fn create_schedule_internal(
        self: &Arc<Self>,
        scope: &TaskScope,
        req: CreateScheduleRequest,
    ) -> TaskResult<ScheduleView> {
        self.ensure_open()?;
        scope.validate()?;

        let title = req.title.trim();
        if title.is_empty() {
            return Err(TaskError::invalid_input(
                "schedule/reminder title must not be empty",
            ));
        }

        let tz_name = req
            .timezone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.default_timezone)
            .to_string();

        let (flow_id, agent_id) = match req.action_kind {
            ScheduleActionKind::SpawnTask => {
                if let Some(flows) = self.flows.get() {
                    let (target, _) = flows.resolve_target(req.target_agent.as_deref())?;
                    (Some(target.flow_id), Some(target.agent_id))
                } else {
                    let target = req
                        .target_agent
                        .as_deref()
                        .and_then(|s| s.split_once('/'))
                        .map(|(f, a)| (Some(f.to_string()), Some(a.to_string())))
                        .or_else(|| {
                            self.config
                                .default_target
                                .as_ref()
                                .map(|t| (Some(t.flow_id.clone()), Some(t.agent_id.clone())))
                        })
                        .unwrap_or((
                            Some("daily".to_string()),
                            req.target_agent
                                .clone()
                                .or_else(|| Some("daily_root".to_string())),
                        ));
                    target
                }
            }
            ScheduleActionKind::NotifyOnly => (None, None),
        };

        let now = now_ms();
        let (schedule_kind, due_at, cron_expr, next_run_at, status) = if let Some(cron_raw) =
            req.cron_expr.as_deref().filter(|s| !s.trim().is_empty())
        {
            let (clean_cron, next_ms) = compute_next_cron_ms(cron_raw, &tz_name, now)?;
            (
                ScheduleKind::Cron,
                None,
                Some(clean_cron),
                Some(next_ms),
                ScheduleStatus::Active,
            )
        } else if let Some(delay_secs) = req.delay_seconds {
            let due = now.saturating_add(i64::try_from(delay_secs).unwrap_or(0) * 1000);
            (
                ScheduleKind::Once,
                Some(due),
                None,
                Some(due),
                ScheduleStatus::Pending,
            )
        } else if let Some(due_iso) = req.due_at_iso.as_deref().filter(|s| !s.trim().is_empty()) {
            let due = parse_due_at_ms(due_iso, &tz_name)?;
            (
                ScheduleKind::Once,
                Some(due),
                None,
                Some(due),
                ScheduleStatus::Pending,
            )
        } else {
            return Err(TaskError::invalid_input(
                "schedule requires one of: due_at_iso, delay_seconds, or cron_expr",
            ));
        };

        let created = self
            .store
            .insert_schedule(NewScheduleRow {
                id: new_id(),
                scope: scope.clone(),
                title: title.to_string(),
                notes: req
                    .notes
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                action_kind: req.action_kind,
                flow_id,
                agent_id,
                session_mode: req.session_mode,
                priority: req.priority,
                related_wiki_links: req.related_wiki_links,
                schedule_kind,
                due_at,
                cron_expr,
                timezone: tz_name,
                status,
                next_run_at,
            })
            .await?;

        self.scheduler_wake.notify_one();
        Ok(created)
    }

    /// Đồng bộ trạng thái của một Task với `Orchestrator::result` (dùng khi turn kết thúc hoặc lúc startup recovery).
    pub(crate) async fn settle_or_advance_task(
        self: &Arc<Self>,
        task: &TaskView,
    ) -> TaskResult<bool> {
        let orch = self.flows()?.get_flow(&task.flow_id)?.clone();
        let scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let caller = scope.to_caller_context();

        if let Some(ref turn_id) = task.latest_turn_id {
            let res = orch.result(&caller, &task.session_id, turn_id).await?;
            if res.status == RunStatus::Running {
                return Ok(false);
            }

            // Nếu turn vừa hoàn tất và có followup đang chờ trong hàng đợi -> tự động submit followup kế tiếp!
            if res.status == RunStatus::Completed {
                if let Some(next_fu) = self.store.next_pending_followup(&task.id).await? {
                    if let Ok(accepted) = orch
                        .submit(&caller, &task.session_id, next_fu.prompt.clone())
                        .await
                    {
                        self.store
                            .mark_followup_submitted(&next_fu.id, &accepted.turn_id)
                            .await?;
                        self.store
                            .mark_task_turn_running(
                                &task.id,
                                &accepted.turn_id,
                                "Running queued follow-up...",
                            )
                            .await?;
                        return Ok(false);
                    }
                }
            }

            let status = TaskStatus::from_run_status(res.status);
            let preview = res
                .output
                .as_deref()
                .map(|s| clip_preview(s, self.config.runtime.output_preview_chars));
            let err_code = res.error.as_ref().map(|e| e.code.as_str());
            let err_msg = res.error.as_ref().map(|e| e.message.as_str());

            if let Some(finished) = self
                .store
                .finish_task(
                    &task.id,
                    TerminalUpdate {
                        status,
                        output_preview: preview.as_deref(),
                        error_code: err_code,
                        error_message: err_msg,
                    },
                )
                .await?
            {
                self.event_bus.emit(TaskEvent::TaskTerminal {
                    task: finished.clone(),
                });
                if finished.source == TaskSource::Autoflow {
                    self.autoflow_done_wake.notify_waiters();
                }
            }
            return Ok(true);
        }

        // Trường hợp Task đang 'queued' (chưa có turn_id) và có followup chờ
        if let Some(next_fu) = self.store.next_pending_followup(&task.id).await? {
            if let Ok(accepted) = orch
                .submit(&caller, &task.session_id, next_fu.prompt.clone())
                .await
            {
                self.store
                    .mark_followup_submitted(&next_fu.id, &accepted.turn_id)
                    .await?;
                self.store
                    .mark_task_turn_running(&task.id, &accepted.turn_id, "Starting queued turn...")
                    .await?;
                return Ok(false);
            }
        }

        Ok(false)
    }
}

fn clip_preview(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push('…');
    out
}
