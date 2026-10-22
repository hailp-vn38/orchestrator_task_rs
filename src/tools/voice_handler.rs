//! Bộ thực thi Raw JSON cho Voice Agent & `HostTool` (thời gian phản hồi < 5ms).

use std::sync::Arc;

use serde_json::{json, Value};

use crate::error::{TaskError, TaskResult};
use crate::runtime::{parse_due_at_ms, SharedTaskCore};
use crate::tools::catalog::{
    TOOL_REMINDER_COMPLETE, TOOL_REMINDER_CREATE, TOOL_REMINDER_DELETE, TOOL_REMINDER_LIST,
    TOOL_TASK_CANCEL, TOOL_TASK_CREATE, TOOL_TASK_DETAIL, TOOL_TASK_FOLLOWUP, TOOL_TASK_LIST,
};
use crate::types::{
    CreateScheduleRequest, CreateTaskRequest, Priority, ScheduleActionKind, ScheduleFilter,
    ScheduleSessionMode, TaskFilter, TaskScope, TaskSource, TaskStatus,
};

pub(crate) async fn execute_tool_raw(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    tool_name: &str,
    args: Value,
) -> TaskResult<Value> {
    match tool_name {
        TOOL_TASK_CREATE => handle_task_create(core, scope, &args).await,
        TOOL_TASK_FOLLOWUP => handle_task_followup(core, scope, &args).await,
        TOOL_TASK_LIST => handle_task_list(core, scope, &args).await,
        TOOL_TASK_DETAIL => handle_task_detail(core, scope, &args).await,
        TOOL_TASK_CANCEL => handle_task_cancel(core, scope, &args).await,
        TOOL_REMINDER_CREATE => handle_reminder_create(core, scope, &args).await,
        TOOL_REMINDER_LIST => handle_reminder_list(core, scope, &args).await,
        TOOL_REMINDER_COMPLETE => handle_reminder_complete(core, scope, &args).await,
        TOOL_REMINDER_DELETE => handle_reminder_delete(core, scope, &args).await,
        other => Err(TaskError::invalid_input(format!(
            "unknown task/reminder tool '{other}'"
        ))),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> TaskResult<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TaskError::invalid_input(format!("missing or empty required field '{key}'")))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn opt_string_vec(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

async fn handle_task_create(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let title = req_str(args, "title")?;
    let prompt = req_str(args, "prompt")?;
    let target_agent = opt_str(args, "target_agent").map(String::from);
    let related_wiki_links = opt_string_vec(args, "related_wiki_links");

    let req = CreateTaskRequest {
        title: title.to_string(),
        prompt: prompt.to_string(),
        target_agent,
        source: TaskSource::Voice,
        schedule_id: None,
        existing_session_id: None,
        related_wiki_links,
    };

    let task = core.create_and_start_task(scope, req).await?;
    Ok(json!({
        "task_id": task.id,
        "title": task.title,
        "flow_id": task.flow_id,
        "agent_id": task.agent_id,
        "session_id": task.session_id,
        "status": task.status.as_str(),
        "message": format!(
            "Background task '{}' has been dispatched to agent '{}/{}'. View live progress on the Task View UI.",
            task.title, task.flow_id, task.agent_id
        )
    }))
}

async fn handle_task_followup(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let task_id = req_str(args, "task_id")?;
    let prompt = req_str(args, "prompt")?;
    let followup = core
        .submit_or_enqueue_followup(scope, task_id, prompt.to_string(), TaskSource::Voice)
        .await?;

    Ok(json!({
        "followup_id": followup.id,
        "task_id": followup.task_id,
        "status": followup.status.as_str(),
        "submitted_turn_id": followup.submitted_turn_id,
    }))
}

async fn handle_task_list(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let status = match opt_str(args, "status") {
        Some("all") | None => None,
        Some(s) => Some(TaskStatus::from_db(s)),
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .or(Some(10));

    let filter = TaskFilter {
        status,
        limit,
        ..Default::default()
    };
    let page = core
        .store
        .list_tasks(scope, &filter, core.config.runtime.page_size)
        .await?;

    Ok(json!({
        "total": page.total,
        "tasks": page.items,
    }))
}

async fn handle_task_detail(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let task_id = req_str(args, "task_id")?;
    let include_messages = args
        .get("include_messages")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut detail = core.get_task_detail_internal(scope, task_id).await?;
    if !include_messages {
        if let Some(ref mut root) = detail.root_agent {
            root.recent_messages.clear();
        }
        for sa in &mut detail.subagents {
            sa.recent_messages.clear();
        }
    }

    serde_json::to_value(&detail).map_err(|e| TaskError::storage("serialize task_detail", e))
}

async fn handle_task_cancel(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let task_id = req_str(args, "task_id")?;
    let task = core.cancel_task_internal(scope, task_id).await?;
    Ok(json!({
        "task_id": task.id,
        "status": task.status.as_str(),
        "cancelled": true,
    }))
}

async fn handle_reminder_create(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let title = req_str(args, "title")?;
    let due_at_iso = opt_str(args, "due_at").map(String::from);
    let delay_seconds = args.get("delay_seconds").and_then(Value::as_u64);
    let cron_expr = opt_str(args, "repeat_cron")
        .or_else(|| opt_str(args, "cron_expr"))
        .map(String::from);
    let priority = opt_str(args, "priority")
        .map(Priority::from_str_lossy)
        .unwrap_or(Priority::Medium);
    let notes = opt_str(args, "notes").map(String::from);
    let related_wiki_links = opt_string_vec(args, "related_wiki_links");
    let action_kind = opt_str(args, "action_kind")
        .map(ScheduleActionKind::from_db)
        .unwrap_or(ScheduleActionKind::NotifyOnly);
    let target_agent = opt_str(args, "target_agent").map(String::from);

    let req = CreateScheduleRequest {
        title: title.to_string(),
        notes,
        action_kind,
        target_agent,
        session_mode: ScheduleSessionMode::NewSession,
        priority,
        related_wiki_links,
        due_at_iso,
        delay_seconds,
        cron_expr,
        timezone: None,
    };

    let created = core.create_schedule_internal(scope, req).await?;
    let next_run_at_iso = created.next_run_at.and_then(|ms| {
        let dt = chrono::DateTime::from_timestamp_millis(ms)?;
        if let Ok(tz) = created.timezone.parse::<chrono_tz::Tz>() {
            Some(dt.with_timezone(&tz).to_rfc3339())
        } else {
            Some(dt.to_rfc3339())
        }
    });
    Ok(json!({
        "reminder_id": created.id,
        "schedule_id": created.id,
        "title": created.title,
        "priority": created.priority.as_str(),
        "action_kind": created.action_kind.as_str(),
        "schedule_kind": created.schedule_kind.as_str(),
        "status": created.status.as_str(),
        "timezone": created.timezone,
        "next_run_at": created.next_run_at,
        "next_run_at_iso": next_run_at_iso,
        "related_wiki_links": created.related_wiki_links,
    }))
}

async fn handle_reminder_list(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let status = opt_str(args, "status")
        .map(String::from)
        .or_else(|| Some("pending".to_string()));
    let tz = &core.config.default_timezone;
    let from_timestamp_ms = match opt_str(args, "from_date") {
        Some(s) => Some(parse_due_at_ms(s, tz)?),
        None => None,
    };
    let to_timestamp_ms = match opt_str(args, "to_date") {
        Some(s) => Some(parse_due_at_ms(s, tz)?),
        None => None,
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .or(Some(20));

    let filter = ScheduleFilter {
        status,
        action_kind: None,
        from_timestamp_ms,
        to_timestamp_ms,
        limit,
    };
    let items = core
        .store
        .list_schedules(scope, &filter, core.config.runtime.page_size)
        .await?;

    Ok(json!({
        "count": items.len(),
        "reminders": items,
    }))
}

async fn handle_reminder_complete(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let reminder_id = req_str(args, "reminder_id")?;
    let note = opt_str(args, "resolution_note");
    let updated = core
        .store
        .complete_schedule(scope, reminder_id, note)
        .await?;
    Ok(json!({
        "reminder_id": updated.id,
        "status": updated.status.as_str(),
        "resolution_note": updated.resolution_note,
    }))
}

async fn handle_reminder_delete(
    core: &Arc<SharedTaskCore>,
    scope: &TaskScope,
    args: &Value,
) -> TaskResult<Value> {
    let reminder_id = req_str(args, "reminder_id")?;
    core.store.delete_schedule(scope, reminder_id).await?;
    Ok(json!({
        "reminder_id": reminder_id,
        "deleted": true,
    }))
}
