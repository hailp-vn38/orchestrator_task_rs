//! Xây dựng JSON Schema động cho bộ Tool `task_*` và `reminder_*`, tự động nhúng
//! danh sách `TargetAgent` kèm `description` để Voice Agent lựa chọn.

use serde_json::{json, Value};

use crate::tools::TaskToolSpec;
use crate::types::AgentCatalogEntry;

pub(crate) const TOOL_TASK_CREATE: &str = "task_create";
pub(crate) const TOOL_TASK_FOLLOWUP: &str = "task_followup";
pub(crate) const TOOL_TASK_LIST: &str = "task_list";
pub(crate) const TOOL_TASK_CANCEL: &str = "task_cancel";

pub(crate) const TOOL_REMINDER_CREATE: &str = "reminder_create";
pub(crate) const TOOL_REMINDER_LIST: &str = "reminder_list";
pub(crate) const TOOL_REMINDER_COMPLETE: &str = "reminder_complete";
pub(crate) const TOOL_REMINDER_DELETE: &str = "reminder_delete";

pub(crate) fn build_all_tool_specs(agents: &[AgentCatalogEntry]) -> Vec<TaskToolSpec> {
    let mut specs = build_voice_task_specs(agents);
    specs.extend(build_reminder_specs(agents));
    specs
}

pub(crate) fn build_voice_task_specs(agents: &[AgentCatalogEntry]) -> Vec<TaskToolSpec> {
    let target_prop = build_target_agent_property(agents);
    let agent_guide = render_agent_selection_guide(agents);

    vec![
        TaskToolSpec {
            name: TOOL_TASK_CREATE.to_string(),
            description: format!(
                "Create and dispatch a background Task immediately (fire-and-forget, does NOT wait for completion). \
                 The task will run asynchronously in the background and appear on the Task View UI.{agent_guide}"
            ),
            parameters: json!({
                "type": "object",
                "required": ["title", "prompt"],
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Concise human-readable title shown on the Task View UI card."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed instruction or objective for the background agent to execute."
                    },
                    "target_agent": target_prop,
                    "related_wiki_links": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of related Obsidian [[WikiLinks]]."
                    }
                },
                "additionalProperties": false
            }),
        },
        TaskToolSpec {
            name: TOOL_TASK_FOLLOWUP.to_string(),
            description: "Send a follow-up instruction to an existing background Task. If the Task is currently running, the instruction is queued and automatically submitted as soon as the current turn completes.".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["task_id", "prompt"],
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "ID of the existing Task."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Additional instruction to send to the Task session."
                    }
                },
                "additionalProperties": false
            }),
        },
        TaskToolSpec {
            name: TOOL_TASK_LIST.to_string(),
            description: "List background Tasks and their live status/progress summary on the Task View UI.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["queued", "running", "completed", "failed", "cancelled", "interrupted", "all"],
                        "description": "Optional status filter (defaults to 'all')."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of tasks to return (1..=50, default 10)."
                    }
                },
                "additionalProperties": false
            }),
        },
        TaskToolSpec {
            name: TOOL_TASK_CANCEL.to_string(),
            description: "Cancel a running or queued background Task and clear its pending follow-ups.".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "ID of the Task to cancel."
                    }
                },
                "additionalProperties": false
            }),
        },
    ]
}

pub(crate) fn build_reminder_specs(agents: &[AgentCatalogEntry]) -> Vec<TaskToolSpec> {
    let target_prop = build_target_agent_property(agents);

    vec![
        TaskToolSpec {
            name: TOOL_REMINDER_CREATE.to_string(),
            description: "Create a user reminder notification (`notify_only`) or a scheduled background agent task (`spawn_task`) triggered at `due_at`, after `delay_seconds`, or on a `repeat_cron` schedule.".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Title of the reminder or scheduled task."
                    },
                    "due_at": {
                        "type": "string",
                        "description": "ISO-8601 timestamp for one-shot trigger (e.g. '2026-10-22T09:00:00+07:00')."
                    },
                    "delay_seconds": {
                        "type": "integer",
                        "description": "Optional relative delay in seconds from now (alternative to due_at)."
                    },
                    "repeat_cron": {
                        "type": "string",
                        "description": "Optional 5-field or 6-field cron expression for recurring schedules (e.g. '0 7 * * *')."
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "Priority level (default 'medium')."
                    },
                    "notes": {
                        "type": "string",
                        "description": "Detailed notes for the reminder, or the prompt to execute when action_kind='spawn_task'."
                    },
                    "related_wiki_links": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of [[WikiLinks]] attached to this reminder/schedule."
                    },
                    "action_kind": {
                        "type": "string",
                        "enum": ["notify_only", "spawn_task"],
                        "description": "Whether to only notify the user ('notify_only', default) or automatically spawn a background agent Task ('spawn_task')."
                    },
                    "target_agent": target_prop
                },
                "additionalProperties": false
            }),
        },
        TaskToolSpec {
            name: TOOL_REMINDER_LIST.to_string(),
            description: "List upcoming or completed reminders and scheduled tasks.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "active", "paused", "completed", "cancelled", "all"],
                        "description": "Filter by status (default 'pending')."
                    },
                    "from_date": {
                        "type": "string",
                        "description": "Optional ISO-8601 lower bound."
                    },
                    "to_date": {
                        "type": "string",
                        "description": "Optional ISO-8601 upper bound."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max items to return (default 20)."
                    }
                },
                "additionalProperties": false
            }),
        },
        TaskToolSpec {
            name: TOOL_REMINDER_COMPLETE.to_string(),
            description: "Mark a reminder or schedule as completed.".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["reminder_id"],
                "properties": {
                    "reminder_id": {
                        "type": "string",
                        "description": "ID of the reminder/schedule to mark completed."
                    },
                    "resolution_note": {
                        "type": "string",
                        "description": "Optional completion note."
                    }
                },
                "additionalProperties": false
            }),
        },
        TaskToolSpec {
            name: TOOL_REMINDER_DELETE.to_string(),
            description: "Delete a reminder or schedule.".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["reminder_id"],
                "properties": {
                    "reminder_id": {
                        "type": "string",
                        "description": "ID of the reminder/schedule to delete."
                    }
                },
                "additionalProperties": false
            }),
        },
    ]
}

fn render_agent_selection_guide(agents: &[AgentCatalogEntry]) -> String {
    if agents.is_empty() {
        return String::new();
    }
    let mut lines = vec!["\nAvailable `target_agent` options:".to_string()];
    for entry in agents {
        lines.push(format!(
            "- `{}` (or `{}`): {}",
            entry.target.agent_id,
            entry.target.canonical_key(),
            entry.description
        ));
    }
    lines.join("\n")
}

fn build_target_agent_property(agents: &[AgentCatalogEntry]) -> Value {
    if agents.is_empty() {
        return json!({
            "type": "string",
            "description": "Target primary agent ID (e.g. 'daily_root' or 'wiki_root')."
        });
    }
    let mut enum_vals = Vec::new();
    let mut desc_parts = Vec::new();
    for entry in agents {
        if !enum_vals.contains(&entry.target.agent_id) {
            enum_vals.push(entry.target.agent_id.clone());
        }
        let canonical = entry.target.canonical_key();
        if !enum_vals.contains(&canonical) {
            enum_vals.push(canonical);
        }
        desc_parts.push(format!(
            "'{}' [{}]: {}",
            entry.target.agent_id, entry.target.flow_id, entry.description
        ));
    }
    json!({
        "type": "string",
        "enum": enum_vals,
        "description": format!(
            "Select which specialist agent should run this task: {}",
            desc_parts.join("; ")
        )
    })
}
