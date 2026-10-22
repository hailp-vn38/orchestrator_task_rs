//! Example 2: Luồng tạo Reminder qua `HostTool` của Flow 2 (`reminder_planner`),
//! gửi lệnh bồi `Followup` để xuất báo cáo Markdown rời (`reports/`), và kích hoạt
//! lịch hẹn giờ tự động (`ScheduleTriggered` + `ReminderDue`) có stream trực tiếp.
//!
//! Dùng chung thư mục dữ liệu với Example 1: `target/homelab_shared_workspace`
//!
//! Chạy:
//!   cargo run --example flow_followup_and_schedule

mod shared;

use std::collections::HashSet;
use std::time::Duration;

use orchestrator_task_rs::{
    CreateScheduleRequest, CreateTaskRequest, Priority, ScheduleActionKind, ScheduleFilter,
    ScheduleSessionMode, TaskFilter, TaskSource,
};

use crate::shared::env::ExampleResult;
use crate::shared::runtime::HomelabExampleApp;
use crate::shared::stream::stream_until_complete;

#[tokio::main]
async fn main() -> ExampleResult<()> {
    let app = HomelabExampleApp::boot(None, None).await?;
    let mut sub = app.task_mgr.subscribe()?;

    println!(
        "\n=== [1. Tạo Task trên `daily_root` gọi subagent `reminder_planner` (`HostTool`)] ==="
    );
    let task = app
        .task_mgr
        .create_task(
            CreateTaskRequest::new(
                "Lên lịch nhắc kiểm tra hệ thống Homelab",
                "Hãy gọi subagent `reminder_planner` để tạo một lời nhắc (`reminder_create`) có tiêu đề \
                 'Kiểm tra định kỳ Wiki Graph & Backup tasks.db' vào thời điểm '2026-12-01T09:00:00+07:00', \
                 priority='high', kèm related_wiki_links=['[[wiki/index]]'].",
            )
            .with_target("daily_root")
            .with_source(TaskSource::Voice),
        )
        .await?;

    let mut pending = HashSet::new();
    pending.insert(task.id.clone());
    stream_until_complete(&mut sub, pending, false, Duration::from_secs(180)).await?;

    println!("\n=== [2. Gửi tiếp lệnh `Followup` vào cùng Task để xuất file báo cáo rời] ===");
    let followup = app
        .task_mgr
        .send_followup(
            &task.id,
            "Tiếp theo, hãy giao cho subagent `report_stager` gọi `markdown_create` tạo file \
             `filename='homelab-task-report.md'`, `title='Báo cáo Vận hành Homelab TaskManager'`, \
             nội dung tóm tắt ngắn gọn kết quả vừa tạo lịch nhắc.",
            TaskSource::Api,
        )
        .await?;
    println!(
        "📨 Đã gửi Followup id={} (status={:?}) vào task={}",
        &followup.id[..8],
        followup.status,
        &task.id[..8]
    );

    let mut pending_fu = HashSet::new();
    pending_fu.insert(task.id.clone());
    stream_until_complete(&mut sub, pending_fu, false, Duration::from_secs(180)).await?;

    println!(
        "\n=== [3. Hẹn giờ `SpawnTask` (sau 1 giây tự động tạo Task tra cứu Wiki) & `NotifyOnly`] ==="
    );
    let _once_reminder = app
        .task_mgr
        .create_schedule(CreateScheduleRequest {
            title: "Nhắc nhở tức thì: Kiểm tra bảng UI View Task".to_string(),
            notes: Some("Kiểm tra kết quả stream trên màn hình".to_string()),
            action_kind: ScheduleActionKind::NotifyOnly,
            target_agent: None,
            session_mode: ScheduleSessionMode::NewSession,
            priority: Priority::Medium,
            related_wiki_links: vec!["[[wiki/index]]".to_string()],
            due_at_iso: None,
            delay_seconds: Some(1),
            cron_expr: None,
            timezone: None,
        })
        .await?;

    let sched_spawn = app
        .task_mgr
        .create_schedule(CreateScheduleRequest {
            title: "Scheduled Task: Kiểm tra trạng thái Wiki Index".to_string(),
            notes: Some(
                "Hãy gọi subagent `wiki_researcher` kiểm tra nhanh `wiki_status` và báo cáo ngắn gọn số lượng tài liệu hiện có trong Vault."
                    .to_string(),
            ),
            action_kind: ScheduleActionKind::SpawnTask,
            target_agent: Some("daily_root".to_string()),
            session_mode: ScheduleSessionMode::NewSession,
            priority: Priority::High,
            related_wiki_links: Vec::new(),
            due_at_iso: None,
            delay_seconds: Some(1),
            cron_expr: None,
            timezone: None,
        })
        .await?;
    println!(
        "⏳ Đã đăng ký lịch tự chạy Agent id={} (kích hoạt sau 1s)...",
        &sched_spawn.id[..8]
    );

    // Chờ sự kiện ScheduleTriggered sinh ra task mới rồi stream task đó tới khi hoàn tất
    let mut empty_set = HashSet::new();
    // Đợi 1.5s để Scheduler kích hoạt và đẩy ScheduleTriggered vào stream
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let active = app
        .task_mgr
        .list_tasks(&TaskFilter {
            source: Some(TaskSource::Schedule),
            ..Default::default()
        })
        .await?;
    for t in &active.items {
        if !t.status.is_terminal() {
            empty_set.insert(t.id.clone());
        }
    }
    stream_until_complete(&mut sub, empty_set, false, Duration::from_secs(180)).await?;

    println!("\n=== [4. Tổng kết Tasks & Schedules trong `target/homelab_shared_workspace`] ===");
    let all_tasks = app.task_mgr.list_tasks(&TaskFilter::default()).await?;
    println!("Tổng số Tasks đã lưu trong `tasks.db`: {}", all_tasks.total);
    for t in &all_tasks.items {
        println!(
            "  • [{}] {:?} | source={:?} | {}/{} | {}",
            &t.id[..8],
            t.status,
            t.source,
            t.flow_id,
            t.agent_id,
            t.title
        );
    }

    let all_schedules = app
        .task_mgr
        .list_schedules(&ScheduleFilter {
            status: Some("all".to_string()),
            ..Default::default()
        })
        .await?;
    println!(
        "Tổng số Schedules/Reminders trong `tasks.db`: {}",
        all_schedules.len()
    );
    for s in &all_schedules {
        println!(
            "  • [{}] {:?} | kind={:?} | status={:?} | title={:?}",
            &s.id[..8],
            s.action_kind,
            s.schedule_kind,
            s.status,
            s.title
        );
    }

    app.shutdown().await?;
    Ok(())
}
