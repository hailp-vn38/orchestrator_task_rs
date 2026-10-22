//! Bộ hiển thị Stream sự kiện trực tiếp (`TaskSubscription`) ra màn hình terminal
//! khi chạy các example.

use std::collections::HashSet;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use orchestrator_flow_rs::{TaskEvent, TaskFeedItem, TaskSource, TaskSubscription, TaskView};

use crate::shared::env::ExampleResult;

pub struct TaskStreamPrinter {
    start: Instant,
    last_delta_tag: Option<String>,
}

impl TaskStreamPrinter {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            last_delta_tag: None,
        }
    }

    fn break_delta_line(&mut self, out: &mut io::StdoutLock<'_>) {
        if self.last_delta_tag.take().is_some() {
            let _ = writeln!(out);
        }
    }

    pub fn print_event(&mut self, ev: &TaskEvent) {
        let mut out = io::stdout().lock();
        let elapsed_s = self.start.elapsed().as_secs_f32();

        match ev {
            TaskEvent::TaskCreated { task } => {
                self.break_delta_line(&mut out);
                let _ = writeln!(
                    out,
                    "\n[{elapsed_s:6.2}s] 🟢 [TASK CREATED] id={} | source={:?} | flow={}/{} | title={:?}",
                    short_id(&task.id),
                    task.source,
                    task.flow_id,
                    task.agent_id,
                    task.title
                );
            }
            TaskEvent::TaskProgress {
                task_id,
                active_subagent,
                active_tool,
                progress_summary,
                ..
            } => {
                self.break_delta_line(&mut out);
                let _ = writeln!(
                    out,
                    "[{elapsed_s:6.2}s] 🔄 [TASK PROGRESS] task={} | subagent={:?} | tool={:?} | {}",
                    short_id(task_id),
                    active_subagent.as_deref().unwrap_or("-"),
                    active_tool.as_deref().unwrap_or("-"),
                    progress_summary.as_deref().unwrap_or("")
                );
            }
            TaskEvent::TaskExecutionStream {
                task_id,
                flow_id,
                event_type,
                session_id,
                root_session_id,
                payload,
                ..
            } => {
                let role_tag = if session_id == root_session_id {
                    format!("{flow_id}:root")
                } else {
                    format!("{flow_id}:sub:{}", short_id(session_id))
                };
                if event_type == "text.delta" {
                    let delta = payload["delta"].as_str().unwrap_or("");
                    if delta.is_empty() {
                        return;
                    }
                    let tag_key = format!("{}:{}", short_id(task_id), role_tag);
                    if self.last_delta_tag.as_deref() != Some(tag_key.as_str()) {
                        let _ = write!(out, "\n[{elapsed_s:6.2}s] 💬 [{tag_key}] ");
                        self.last_delta_tag = Some(tag_key);
                    }
                    let _ = write!(out, "{delta}");
                    let _ = out.flush();
                } else if event_type == "tool.started" {
                    self.break_delta_line(&mut out);
                    let tool = payload["tool_name"].as_str().unwrap_or("?");
                    let args = &payload["arguments"];
                    let _ = writeln!(
                        out,
                        "[{elapsed_s:6.2}s] 🛠️  [STREAM {}:{}] ▶ {tool} args={args}",
                        short_id(task_id),
                        role_tag
                    );
                } else if event_type == "tool.completed" {
                    self.break_delta_line(&mut out);
                    let tool = payload["tool_name"].as_str().unwrap_or("?");
                    let meta = &payload["result"]["metadata"];
                    let _ = writeln!(
                        out,
                        "[{elapsed_s:6.2}s] ✅ [STREAM {}:{}] ✔ {tool} meta={meta}",
                        short_id(task_id),
                        role_tag
                    );
                } else if event_type == "tool.failed" {
                    self.break_delta_line(&mut out);
                    let tool = payload["tool_name"].as_str().unwrap_or("?");
                    let code = payload["error_code"]
                        .as_str()
                        .or_else(|| payload["error"]["code"].as_str())
                        .unwrap_or("ERROR");
                    let msg = payload["error_message"]
                        .as_str()
                        .or_else(|| payload["error"]["message"].as_str())
                        .unwrap_or("");
                    let _ = writeln!(
                        out,
                        "[{elapsed_s:6.2}s] ⚠️  [STREAM {}:{}] ✘ {tool} ({code}): {msg}",
                        short_id(task_id),
                        role_tag
                    );
                }
            }
            TaskEvent::FollowupQueued {
                task_id,
                followup_id,
                ..
            } => {
                self.break_delta_line(&mut out);
                let _ = writeln!(
                    out,
                    "[{elapsed_s:6.2}s] 📥 [FOLLOWUP QUEUED] task={} | followup={}",
                    short_id(task_id),
                    short_id(followup_id)
                );
            }
            TaskEvent::ReminderDue { schedule } => {
                self.break_delta_line(&mut out);
                let _ = writeln!(
                    out,
                    "\n[{elapsed_s:6.2}s] 🔔 [REMINDER DUE] id={} | priority={:?} | title={:?} | links={:?}",
                    short_id(&schedule.id),
                    schedule.priority,
                    schedule.title,
                    schedule.related_wiki_links
                );
            }
            TaskEvent::ScheduleTriggered {
                schedule,
                spawned_task_id,
            } => {
                self.break_delta_line(&mut out);
                let _ = writeln!(
                    out,
                    "\n[{elapsed_s:6.2}s] ⏰ [SCHEDULE TRIGGERED] schedule={} ({:?}) -> spawned_task={}",
                    short_id(&schedule.id),
                    schedule.title,
                    short_id(spawned_task_id)
                );
            }
            TaskEvent::TaskTerminal { task } => {
                self.break_delta_line(&mut out);
                let _ = writeln!(
                    out,
                    "\n[{elapsed_s:6.2}s] 🏁 [TASK TERMINAL] id={} | status={:?} | source={:?} | title={:?}\n    └─ output_preview: {:?}",
                    short_id(&task.id),
                    task.status,
                    task.source,
                    task.title,
                    task.output_preview.as_deref().unwrap_or("-")
                );
            }
            TaskEvent::TaskDeleted {
                task_id,
                flow_id,
                session_id,
                ..
            } => {
                self.break_delta_line(&mut out);
                let _ = writeln!(
                    out,
                    "\n[{elapsed_s:6.2}s] 🗑️  [TASK DELETED] id={} | flow={} | session={}",
                    short_id(task_id),
                    flow_id,
                    short_id(session_id)
                );
            }
        }
    }
}

pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Stream và in trực tiếp mọi sự kiện cho tới khi tất cả `expected_task_ids` đạt trạng thái terminal
/// (và nếu `wait_for_autoflow` = `true`, tự động đợi cả Task `Autoflow` được kích hoạt và hoàn tất!).
pub async fn stream_until_complete(
    sub: &mut TaskSubscription,
    mut pending_task_ids: HashSet<String>,
    wait_for_autoflow: bool,
    timeout: Duration,
) -> ExampleResult<Vec<TaskView>> {
    let mut printer = TaskStreamPrinter::new();
    let mut completed_views = Vec::new();
    let mut saw_autoflow = false;
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        if pending_task_ids.is_empty() {
            if wait_for_autoflow {
                // Đợi ngắn (6.5s nếu chưa thấy Autoflow, hoặc 2.5s giữa các chặng Autoflow)
                // xem Auto-Flow có kích hoạt chặng tiếp theo không; nếu không có Task Autoflow mới thì kết thúc ngay.
                let grace = if saw_autoflow {
                    Duration::from_millis(2500)
                } else {
                    Duration::from_millis(6500)
                };
                tokio::select! {
                    item = sub.recv() => match item {
                        Some(TaskFeedItem::Event(ev)) => {
                            printer.print_event(&ev);
                            if let TaskEvent::TaskCreated { task } = ev {
                                if task.source == TaskSource::Autoflow {
                                    pending_task_ids.insert(task.id);
                                    saw_autoflow = true;
                                    continue;
                                }
                            }
                            continue;
                        }
                        Some(TaskFeedItem::ResyncRequired) | None => break,
                    },
                    () = tokio::time::sleep(grace) => break,
                }
            } else {
                break;
            }
        }
        tokio::select! {
            item = sub.recv() => match item {
                Some(TaskFeedItem::Event(ev)) => {
                    printer.print_event(&ev);
                    match ev {
                        TaskEvent::TaskCreated { task } => {
                            if wait_for_autoflow && task.source == TaskSource::Autoflow {
                                pending_task_ids.insert(task.id);
                                saw_autoflow = true;
                            }
                        }
                        TaskEvent::ScheduleTriggered { spawned_task_id, .. } => {
                            pending_task_ids.insert(spawned_task_id);
                        }
                        TaskEvent::TaskTerminal { task } => {
                            pending_task_ids.remove(&task.id);
                            completed_views.push(task);
                        }
                        _ => {}
                    }
                }
                Some(TaskFeedItem::ResyncRequired) | None => break,
            },
            () = &mut deadline => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out while streaming background tasks",
                )
                .into());
            }
        }
    }

    Ok(completed_views)
}
