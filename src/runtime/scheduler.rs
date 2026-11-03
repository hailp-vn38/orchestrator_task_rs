//! Vòng lặp Timer & Cron cho `task_schedules` (xử lý cả Reminder `NotifyOnly` và `SpawnTask`).

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::error::{TaskError, TaskResult};
use crate::runtime::{compute_next_cron_ms, parse_due_at_ms, SharedTaskCore};
use crate::store::{new_id, now_ms, NewScheduleRow};
use crate::types::{
    CreateScheduleRequest, CreateTaskRequest, ScheduleActionKind, ScheduleKind,
    ScheduleSessionMode, ScheduleStatus, ScheduleView, TaskEvent, TaskScope, TaskSource,
};

impl SharedTaskCore {
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
}

pub(crate) fn spawn_scheduler_loop(core: Arc<SharedTaskCore>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let max_tick = core.config.runtime.scheduler_tick;
        loop {
            if core.shutdown.is_cancelled() {
                break;
            }
            process_due_schedules(&core).await;

            let sleep_dur = match core.store.next_due_timestamp().await {
                Ok(Some(next_ms)) => {
                    let delta_ms = next_ms.saturating_sub(now_ms()).max(0);
                    Duration::from_millis(u64::try_from(delta_ms).unwrap_or(0)).min(max_tick)
                }
                _ => max_tick,
            };

            tokio::select! {
                biased;
                () = core.shutdown.cancelled() => break,
                () = core.scheduler_wake.notified() => {}
                () = tokio::time::sleep(sleep_dur) => {}
            }
        }
    })
}

async fn process_due_schedules(core: &Arc<SharedTaskCore>) {
    let now = now_ms();
    let due_list = match core.store.due_schedules(now).await {
        Ok(list) => list,
        Err(e) => {
            warn!("scheduler failed to query due schedules: {e}");
            return;
        }
    };

    for sched in due_list {
        if core.shutdown.is_cancelled() {
            break;
        }
        fire_single_schedule(core, sched, now).await;
    }
}

async fn fire_single_schedule(core: &Arc<SharedTaskCore>, sched: ScheduleView, fired_at: i64) {
    let (next_status, next_run_at) = match sched.schedule_kind {
        ScheduleKind::Once => (ScheduleStatus::Completed, None),
        ScheduleKind::Cron => {
            let expr = sched.cron_expr.as_deref().unwrap_or("0 * * * *");
            match compute_next_cron_ms(expr, &sched.timezone, fired_at + 1000) {
                Ok((_, next_ms)) => (ScheduleStatus::Active, Some(next_ms)),
                Err(e) => {
                    warn!(schedule_id = %sched.id, "invalid cron on fire: {e}");
                    (ScheduleStatus::Paused, None)
                }
            }
        }
    };

    match sched.action_kind {
        ScheduleActionKind::NotifyOnly => {
            if let Ok(Some(updated)) = core
                .store
                .record_schedule_fired(&sched.id, fired_at, next_status, next_run_at, None, None)
                .await
            {
                debug!(schedule_id = %updated.id, "reminder due fired");
                core.event_bus
                    .emit(TaskEvent::ReminderDue { schedule: updated });
            }
        }
        ScheduleActionKind::SpawnTask => {
            let scope = TaskScope::new(&sched.owner_id, &sched.workspace_id);
            let target_agent = match (&sched.flow_id, &sched.agent_id) {
                (Some(f), Some(a)) => Some(format!("{f}/{a}")),
                (None, Some(a)) => Some(a.clone()),
                _ => None,
            };
            let prompt = sched
                .notes
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| sched.title.clone());
            let existing_session_id = if sched.session_mode == ScheduleSessionMode::ReuseSession {
                sched.reused_session_id.clone()
            } else {
                None
            };

            let req = CreateTaskRequest {
                title: sched.title.clone(),
                prompt,
                target_agent,
                source: TaskSource::Schedule,
                schedule_id: Some(sched.id.clone()),
                existing_session_id,
                related_wiki_links: sched.related_wiki_links.clone(),
                recipe: None,
                input_artifacts: Vec::new(),
            };

            match core.create_and_start_task(&scope, req).await {
                Ok(task) => {
                    let reused_sid = if sched.session_mode == ScheduleSessionMode::ReuseSession {
                        Some(task.session_id.as_str())
                    } else {
                        None
                    };
                    if let Ok(Some(updated)) = core
                        .store
                        .record_schedule_fired(
                            &sched.id,
                            fired_at,
                            next_status,
                            next_run_at,
                            Some(&task.id),
                            reused_sid,
                        )
                        .await
                    {
                        core.event_bus.emit(TaskEvent::ScheduleTriggered {
                            schedule: updated,
                            spawned_task_id: task.id,
                        });
                    }
                }
                Err(e) => {
                    warn!(schedule_id = %sched.id, "failed to spawn scheduled task: {e}");
                    let _ = core
                        .store
                        .record_schedule_fired(
                            &sched.id,
                            fired_at,
                            next_status,
                            next_run_at,
                            None,
                            None,
                        )
                        .await;
                }
            }
        }
    }
}
