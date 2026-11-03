//! Chuyển đổi giữa SQLite Row và Domain Value Types, cùng các hàm tiện ích ID/thời gian.

use sqlx::sqlite::SqliteRow;
use sqlx::Row;
use uuid::Uuid;

use crate::error::TaskError;
use crate::types::{
    FollowupStatus, FollowupView, Priority, ScheduleActionKind, ScheduleKind, ScheduleSessionMode,
    ScheduleStatus, ScheduleView, TaskSource, TaskStatus, TaskView,
};

pub(crate) fn new_id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn db_err(op: &'static str) -> impl FnOnce(sqlx::Error) -> TaskError {
    move |e| TaskError::storage(op, e)
}

pub(crate) fn encode_links(links: &[String]) -> String {
    serde_json::to_string(links).unwrap_or_else(|_| "[]".to_string())
}

pub(crate) fn decode_links(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

pub(crate) fn map_task_row(row: &SqliteRow) -> TaskView {
    let links_raw: String = row.get("related_wiki_links");
    let status_raw: String = row.get("status");
    let source_raw: String = row.get("source");
    TaskView {
        id: row.get("id"),
        owner_id: row.get("owner_id"),
        workspace_id: row.get("workspace_id"),
        title: row.get("title"),
        prompt: row.get("prompt"),
        flow_id: row.get("flow_id"),
        agent_id: row.get("agent_id"),
        source: TaskSource::from_db(&source_raw),
        schedule_id: row.get("schedule_id"),
        session_id: row.get("session_id"),
        latest_turn_id: row.get("latest_turn_id"),
        status: TaskStatus::from_db(&status_raw),
        active_subagent: row.get("active_subagent"),
        active_tool: row.get("active_tool"),
        progress_summary: row.get("progress_summary"),
        output_preview: row.get("output_preview"),
        related_wiki_links: decode_links(&links_raw),
        wait_reason: row.get("wait_reason"),
        next_attempt_at: row.get("next_attempt_at"),
        error_code: row.get("error_code"),
        error_message: row.get("error_message"),
        recipe: None,
        created_at: row.get("created_at"),
        started_at: row.get("started_at"),
        updated_at: row.get("updated_at"),
        finished_at: row.get("finished_at"),
    }
}

pub(crate) fn map_followup_row(row: &SqliteRow) -> FollowupView {
    let source_raw: String = row.get("source");
    let status_raw: String = row.get("status");
    FollowupView {
        id: row.get("id"),
        task_id: row.get("task_id"),
        prompt: row.get("prompt"),
        source: TaskSource::from_db(&source_raw),
        status: FollowupStatus::from_db(&status_raw),
        submitted_turn_id: row.get("submitted_turn_id"),
        created_at: row.get("created_at"),
        submitted_at: row.get("submitted_at"),
    }
}

pub(crate) fn map_schedule_row(row: &SqliteRow) -> ScheduleView {
    let action_raw: String = row.get("action_kind");
    let session_mode_raw: String = row.get("session_mode");
    let priority_raw: String = row.get("priority");
    let links_raw: String = row.get("related_wiki_links");
    let kind_raw: String = row.get("schedule_kind");
    let status_raw: String = row.get("status");
    ScheduleView {
        id: row.get("id"),
        owner_id: row.get("owner_id"),
        workspace_id: row.get("workspace_id"),
        title: row.get("title"),
        notes: row.get("notes"),
        action_kind: ScheduleActionKind::from_db(&action_raw),
        flow_id: row.get("flow_id"),
        agent_id: row.get("agent_id"),
        session_mode: ScheduleSessionMode::from_db(&session_mode_raw),
        reused_session_id: row.get("reused_session_id"),
        priority: Priority::from_str_lossy(&priority_raw),
        related_wiki_links: decode_links(&links_raw),
        schedule_kind: ScheduleKind::from_db(&kind_raw),
        due_at: row.get("due_at"),
        cron_expr: row.get("cron_expr"),
        timezone: row.get("timezone"),
        status: ScheduleStatus::from_db(&status_raw),
        resolution_note: row.get("resolution_note"),
        next_run_at: row.get("next_run_at"),
        last_run_at: row.get("last_run_at"),
        last_task_id: row.get("last_task_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
