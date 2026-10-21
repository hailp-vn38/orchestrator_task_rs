//! Vòng lặp Timer & Cron cho `task_schedules` (xử lý cả Reminder `NotifyOnly` và `SpawnTask`).

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::runtime::{compute_next_cron_ms, SharedTaskCore};
use crate::store::now_ms;
use crate::types::{
    CreateTaskRequest, ScheduleActionKind, ScheduleKind, ScheduleSessionMode, ScheduleStatus,
    ScheduleView, TaskEvent, TaskScope, TaskSource,
};

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
