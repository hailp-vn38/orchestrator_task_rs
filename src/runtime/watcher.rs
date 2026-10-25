//! Watcher theo dõi tiến độ từng Task đang chạy qua `Orchestrator::subscribe`
//! và cập nhật live `progress_summary` + tự động xử lý `task_followups` khi turn hoàn tất.

use std::sync::Arc;
use std::time::Duration;

use orchestrator_agent_rs::{ExecutionEvent, FeedItem};

use crate::runtime::{lock, SharedTaskCore};
use crate::store::{now_ms, ProgressUpdate};
use crate::types::{TaskEvent, TaskScope, TaskStatus, TaskView};

/// Đảm bảo có đúng 1 watcher task đang chạy cho `task.id`.
pub(crate) fn ensure_task_watcher(core: Arc<SharedTaskCore>, task: TaskView) {
    let mut guards = lock(&core.active_watchers);
    guards.retain(|_, h| !h.is_finished());
    if guards.contains_key(&task.id) {
        return;
    }
    let task_id = task.id.clone();
    let task_id_key = task_id.clone();
    let core_clone = Arc::clone(&core);
    let handle = tokio::spawn(async move {
        run_task_watcher(core_clone.clone(), task).await;
        lock(&core_clone.active_watchers).remove(&task_id);
    });
    guards.insert(task_id_key, handle);
}

async fn run_task_watcher(core: Arc<SharedTaskCore>, initial_task: TaskView) {
    let Ok(flows) = core.flows() else {
        return;
    };
    let Ok(orch) = flows.get_flow(&initial_task.flow_id).cloned() else {
        return;
    };
    let scope = TaskScope::new(&initial_task.owner_id, &initial_task.workspace_id);
    let caller = scope.to_caller_context();
    let refresh_interval = orch
        .snapshot_refresh_interval()
        .max(Duration::from_secs(15));

    loop {
        if core.shutdown.is_cancelled() {
            return;
        }

        // Mở subscription TRƯỚC khi kiểm tra trạng thái để không bỏ lỡ terminal event
        let mut sub_opt = orch.subscribe(&caller, &initial_task.session_id).await.ok();

        // Kiểm tra xem turn đã xong chưa (hoặc có followup tiếp theo cần đẩy vào không)
        let Ok(Some(current_task)) = core.store.get_task_unscoped(&initial_task.id).await else {
            return;
        };
        if current_task.status.is_terminal() {
            return;
        }
        if let Ok(true) = core.settle_or_advance_task(&current_task).await {
            return;
        }

        let Some(ref mut sub) = sub_opt else {
            tokio::select! {
                () = core.shutdown.cancelled() => return,
                () = tokio::time::sleep(Duration::from_millis(200)) => continue,
            }
        };

        let mut tick = tokio::time::interval(refresh_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                () = core.shutdown.cancelled() => return,
                item = sub.recv() => match item {
                    Some(FeedItem::Event(ev)) => {
                        let is_run_terminal = handle_execution_event(&core, &initial_task, &ev).await;
                        if is_run_terminal {
                            if let Ok(Some(latest)) = core.store.get_task_unscoped(&initial_task.id).await {
                                if latest.status.is_terminal() {
                                    return;
                                }
                                if let Ok(true) = core.settle_or_advance_task(&latest).await {
                                    return;
                                }
                            }
                        }
                    }
                    Some(FeedItem::ResyncRequired) | None => {
                        break;
                    }
                },
                _ = tick.tick() => {
                    if let Ok(Some(latest)) = core.store.get_task_unscoped(&initial_task.id).await {
                        if latest.status.is_terminal() {
                            return;
                        }
                        if let Ok(true) = core.settle_or_advance_task(&latest).await {
                            return;
                        }
                    } else {
                        return;
                    }
                }
            }
        }
    }
}

async fn emit_task_progress(
    core: &SharedTaskCore,
    task: &TaskView,
    active_subagent: Option<&str>,
    active_tool: Option<&str>,
    summary: String,
) {
    let _ = core
        .store
        .update_task_progress(
            &task.id,
            ProgressUpdate {
                active_subagent,
                active_tool,
                progress_summary: Some(&summary),
            },
        )
        .await;
    core.event_bus.emit(TaskEvent::TaskProgress {
        task_id: task.id.clone(),
        owner_id: task.owner_id.clone(),
        workspace_id: task.workspace_id.clone(),
        status: TaskStatus::Running,
        active_subagent: active_subagent.map(str::to_string),
        active_tool: active_tool.map(str::to_string),
        progress_summary: Some(summary),
        timestamp_ms: now_ms(),
    });
}

async fn handle_execution_event(
    core: &SharedTaskCore,
    task: &TaskView,
    ev: &ExecutionEvent,
) -> bool {
    if core.event_bus.has_subscribers() {
        core.event_bus.emit(TaskEvent::TaskExecutionStream {
            task_id: task.id.clone(),
            flow_id: task.flow_id.clone(),
            owner_id: task.owner_id.clone(),
            workspace_id: task.workspace_id.clone(),
            event_type: ev.event_type.clone(),
            session_id: ev.session_id.clone(),
            root_session_id: ev.root_session_id.clone(),
            payload: ev.payload.clone(),
            timestamp_ms: ev.timestamp_ms,
        });
    }

    match ev.event_type.as_str() {
        "message.committed" => {
            if let Some(text) = ev
                .payload
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                emit_task_progress(core, task, None, None, text.to_string()).await;
            }
            false
        }
        "tool.started" => {
            let tool_name = ev
                .payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let subagent = if tool_name == "subagent" {
                ev.payload
                    .get("arguments")
                    .and_then(|a| a.get("agent"))
                    .and_then(|v| v.as_str())
            } else {
                None
            };
            let summary = if let Some(sa) = subagent {
                let desc = ev
                    .payload
                    .get("arguments")
                    .and_then(|a| a.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("working");
                format!("Subagent '{sa}': {desc}")
            } else {
                format!("Running tool '{tool_name}'...")
            };

            emit_task_progress(core, task, subagent, Some(tool_name), summary).await;
            false
        }
        "tool.progress" => {
            if let Some(msg) = ev.payload.get("message").and_then(|v| v.as_str()) {
                emit_task_progress(core, task, None, None, msg.to_string()).await;
            }
            false
        }
        "tool.completed" => {
            let tool_name = ev
                .payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let summary = format!("Completed tool '{tool_name}'");
            emit_task_progress(core, task, None, None, summary).await;
            false
        }
        "run.completed" | "run.failed" | "run.cancelled" | "run.interrupted" => {
            // Chỉ coi là terminal của Root Task khi event thuộc chính Root session của Task
            ev.session_id == task.session_id
        }
        _ => false,
    }
}
