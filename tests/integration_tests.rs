//! Integration tests tại public interface của `TaskManager` (theo `codebase-design`:
//! "The interface is the test surface").

use std::sync::Arc;
use std::time::Duration;

use llm_wiki::prompts::WikiTemplateId;
use llm_wiki::{WikiEngine, WriteTopicSourceRequest};
use orchestrator_agent_rs::test_support::ScriptedGateway;
use orchestrator_agent_rs::{
    AgentDefinition, AgentMode, LlmBackend, LlmConfig, LlmProviderConfig, Orchestrator,
    OrchestratorConfig, ProviderAuth, SecretString,
};
use orchestrator_task_rs::{
    build_sample_technical_pdf, CreateScheduleRequest, CreateTaskRequest, Priority,
    ScheduleActionKind, ScheduleFilter, TaskConfig, TaskEvent, TaskFeedItem, TaskFilter,
    TaskManager, TaskRuntimeConfig, TaskScope, TaskSource, TaskStatus, WikiAutoFlowBinding,
    WorkspaceLayout, WorkspaceToolKit,
};
use serde_json::json;
use tokio::sync::Notify;

fn test_llm_config() -> LlmConfig {
    LlmConfig {
        providers: vec![LlmProviderConfig {
            id: "test".to_string(),
            backend: LlmBackend::OpenAiCompatible,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            auth: ProviderAuth::ApiKey(SecretString::new("test-key")),
            request_timeout: Duration::from_secs(10),
        }],
    }
}

#[tokio::test]
async fn two_phase_init_voice_catalog_and_task_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a helpful test agent.").unwrap();

    // PHA 1: Prepare TaskManager trên file tasks.db riêng
    let runtime_cfg = TaskRuntimeConfig {
        scheduler_tick: Duration::from_millis(50),
        autoflow_poll_interval: Duration::from_millis(100),
        ..Default::default()
    };

    let task_cfg = TaskConfig::new(dir.path().join("task_store"))
        .with_default_target("daily", "daily_root")
        .with_runtime(runtime_cfg);

    let task_init = TaskManager::prepare(task_cfg).await.unwrap();

    // Xây dựng daily_orch (Flow 2) có đăng ký các reminder HostTool từ Pha 1
    let daily_gw = ScriptedGateway::new();
    daily_gw.push_text("Báo cáo phân tích đã hoàn tất.");

    let daily_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Trợ lý hằng ngày: tra cứu Wiki, tạo báo cáo và quản lý nhắc nhở.")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools([
            "reminder_create",
            "reminder_list",
            "reminder_complete",
            "reminder_delete",
        ]);

    let mut daily_builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![daily_agent],
        dir.path().join("daily_store"),
    ))
    .with_gateway(daily_gw.clone());

    for tool in task_init.reminder_host_tools() {
        daily_builder = daily_builder.register_tool(tool);
    }
    let daily_orch = Arc::new(daily_builder.build().await.unwrap());

    // Xây dựng wiki_orch (Flow 1)
    let wiki_gw = ScriptedGateway::new();
    wiki_gw.push_text("Đã biên dịch xong nguồn mới vào Wiki.");

    let wiki_agent = AgentDefinition::new("wiki_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Bộ điều phối thu thập nguồn ngoài và biên dịch/kiểm toán Wiki.")
        .with_prompt_path(&prompt_path);

    let wiki_orch = Arc::new(
        Orchestrator::builder(OrchestratorConfig::new(
            test_llm_config(),
            vec![wiki_agent],
            dir.path().join("wiki_store"),
        ))
        .with_gateway(wiki_gw.clone())
        .build()
        .await
        .unwrap(),
    );

    // PHA 2: Gắn cả 2 Flow và start TaskManager
    let task_mgr = task_init
        .register_flow("daily", daily_orch.clone())
        .register_flow("wiki", wiki_orch.clone())
        .start()
        .await
        .unwrap();

    // Kiểm tra catalog động cho Voice Agent
    let suite = task_mgr.voice_tool_suite();
    let agents = suite.available_agents();
    assert_eq!(agents.len(), 2);

    let specs = suite.voice_task_tool_specs();
    let create_spec = specs.iter().find(|s| s.name == "task_create").unwrap();
    assert!(create_spec.description.contains("daily_root"));
    assert!(create_spec.description.contains("wiki_root"));

    // Mở subscription cho UI View Task trước khi Voice Agent tạo task
    let mut sub = task_mgr.subscribe().unwrap();

    // Voice Agent gọi `task_create` (Fire-and-Forget) chọn `daily_root`
    let created_json = suite
        .execute_raw(
            "task_create",
            json!({
                "title": "Phân tích chiến lược DCA",
                "prompt": "Hãy tổng hợp các điểm chính của chiến lược DCA",
                "target_agent": "daily_root",
                "related_wiki_links": ["[[concepts/dca]]"]
            }),
        )
        .await
        .unwrap();

    let task_id = created_json["task_id"].as_str().unwrap().to_string();
    assert_eq!(created_json["flow_id"], "daily");
    assert_eq!(created_json["agent_id"], "daily_root");

    // Chờ sự kiện TaskTerminal từ Watcher
    let mut finished_task = None;
    for _ in 0..30 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if task.id == task_id {
                finished_task = Some(task);
                break;
            }
        }
    }

    let final_view = finished_task.expect("task should reach terminal Completed state");
    assert_eq!(final_view.status, TaskStatus::Completed);
    assert_eq!(final_view.owner_id, "personal");
    assert_eq!(final_view.workspace_id, "homelab");
    assert_eq!(
        final_view.output_preview.as_deref(),
        Some("Báo cáo phân tích đã hoàn tất.")
    );

    // Kiểm tra TaskDetailView cho UI View Task + `task_detail` tool + `get_task_agent_history`
    let detail = task_mgr.get_task_detail(&task_id).await.unwrap();
    assert_eq!(detail.task.status, TaskStatus::Completed);
    assert!(detail.family_snapshot.is_some());
    let summary = detail
        .execution_summary
        .as_ref()
        .expect("execution_summary present");
    assert_eq!(summary.total_agents, 1);
    assert_eq!(summary.subagent_count, 0);
    assert!(summary.total_steps >= 1);
    assert!(detail.find_agent_session("daily_root").is_some());

    let detail_json = suite
        .execute_raw(
            "task_detail",
            json!({
                "task_id": task_id,
                "include_messages": true
            }),
        )
        .await
        .unwrap();
    assert_eq!(detail_json["task"]["id"], task_id);
    assert_eq!(detail_json["root_agent"]["agent_id"], "daily_root");
    assert!(!detail_json["root_agent"]["recent_messages"]
        .as_array()
        .unwrap()
        .is_empty());

    let root_history = task_mgr
        .get_task_agent_history(&task_id, None, Some(10))
        .await
        .unwrap();
    assert!(root_history.len() >= 2);
    assert_eq!(root_history[0].role, "user");

    let page = task_mgr.list_tasks(&TaskFilter::default()).await.unwrap();
    assert_eq!(page.total, 1);

    // Xóa Task qua tool `task_delete` -> kiểm tra bản ghi trong `tasks.db`, session trong `Orchestrator` và `TaskEvent::TaskDeleted`
    let del_resp = suite
        .execute_raw("task_delete", json!({ "task_id": task_id }))
        .await
        .unwrap();
    assert_eq!(del_resp["deleted"], true);
    assert!(task_mgr.get_task(&task_id).await.is_err());
    let caller = TaskScope::default().to_caller_context();
    assert!(daily_orch
        .get_session(&caller, &final_view.session_id)
        .await
        .is_err());

    let mut saw_deleted_event = false;
    for _ in 0..10 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskDeleted {
            task_id: del_id, ..
        }))) = tokio::time::timeout(Duration::from_millis(500), sub.recv()).await
        {
            if del_id == task_id {
                saw_deleted_event = true;
                break;
            }
        }
    }
    assert!(saw_deleted_event, "TaskDeleted event should be emitted");

    task_mgr.shutdown().await.unwrap();
    daily_orch.shutdown().await.unwrap();
    wiki_orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn schedule_reminder_and_managed_autoflow() {
    let dir = tempfile::tempdir().unwrap();
    let layout = WorkspaceLayout::new(dir.path());
    layout.ensure_dirs().unwrap();
    let prompt_path = layout.prompts_dir().join("prompt.md");
    std::fs::write(&prompt_path, "You are a helpful test agent.").unwrap();

    // Khởi tạo WikiEngine từ `layout.wiki_vault_dir()`
    let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir()).unwrap());
    engine.apply_template(WikiTemplateId::Research).unwrap();
    let autoflow_notify = Arc::new(Notify::new());

    let runtime_cfg = TaskRuntimeConfig {
        scheduler_tick: Duration::from_millis(50),
        autoflow_poll_interval: Duration::from_millis(100),
        ..Default::default()
    };

    let task_cfg = TaskConfig::from_layout(&layout)
        .with_default_scope(TaskScope::new("personal", "homelab"))
        .with_default_agent("wiki_root")
        .with_runtime(runtime_cfg);

    let task_init = TaskManager::prepare(task_cfg).await.unwrap();

    let wiki_gw = ScriptedGateway::new();
    let wiki_agent = AgentDefinition::new("wiki_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Wiki compiler root agent")
        .with_prompt_path(&prompt_path);

    let wiki_orch = Arc::new(
        Orchestrator::builder(OrchestratorConfig::new(
            test_llm_config(),
            vec![wiki_agent],
            layout.orchestrator_store_dir(),
        ))
        .with_gateway(wiki_gw.clone())
        .build()
        .await
        .unwrap(),
    );

    let task_mgr = task_init
        .with_orchestrator(wiki_orch.clone())
        .with_wiki_autoflow(WikiAutoFlowBinding::for_agent(
            engine.clone(),
            autoflow_notify.clone(),
            "wiki_root",
        ))
        .start()
        .await
        .unwrap();

    assert_eq!(task_mgr.store_dir(), layout.tasks_store_dir().as_path());
    assert_eq!(
        task_mgr.wiki_vault_dir(),
        Some(layout.wiki_vault_dir().as_path())
    );

    let mut sub = task_mgr.subscribe().unwrap();

    // 1. Tạo Reminder (NotifyOnly) với delay 0 giây để kích hoạt ngay
    let sched = task_mgr
        .create_schedule(CreateScheduleRequest {
            title: "Kiểm tra lại rủi ro DCA".to_string(),
            notes: Some("Xem lại phần drawdown".to_string()),
            action_kind: ScheduleActionKind::NotifyOnly,
            target_agent: None,
            session_mode: orchestrator_task_rs::ScheduleSessionMode::NewSession,
            priority: Priority::High,
            related_wiki_links: vec!["[[wiki/syntheses/dca]]".to_string()],
            due_at_iso: None,
            delay_seconds: Some(0),
            cron_expr: None,
            timezone: None,
        })
        .await
        .unwrap();

    let mut reminder_fired = false;
    for _ in 0..20 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::ReminderDue { schedule }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if schedule.id == sched.id {
                reminder_fired = true;
                break;
            }
        }
    }
    assert!(reminder_fired, "ReminderDue event should be emitted");

    let completed_reminders = task_mgr
        .list_schedules(&ScheduleFilter {
            status: Some("completed".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(completed_reminders.len(), 1);

    // 2. Ghi một file mới vào `sources/` và bắn `autoflow_notify` (mô phỏng `add_to_sources` từ Flow 2)
    let written = engine
        .write_topic_source(WriteTopicSourceRequest {
            topic_folder: "software-architecture/agent-tasks".to_string(),
            file_stem: Some("background-task-spec".to_string()),
            title: "Background Task Specification".to_string(),
            tags: vec!["topic/software-architecture/agent-tasks".to_string()],
            extra_frontmatter: Vec::new(),
            header_bullets: Vec::new(),
            body_heading: None,
            raw_body: "## Core Architecture\nManaged background tasks on SQLite.".to_string(),
            preview_chars: Some(300),
        })
        .unwrap();

    // Để autoflow dừng lại sau 1 chặng trong test, khi wiki_root chạy xong chúng ta ingest luôn source đó
    wiki_gw.push_text("Auto-Flow cycle completed.");
    autoflow_notify.notify_one();

    let mut autoflow_task_created = None;
    for _ in 0..30 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskCreated { task }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if task.source == TaskSource::Autoflow {
                autoflow_task_created = Some(task);
                break;
            }
        }
    }

    let af_task = autoflow_task_created.expect("Auto-Flow should create a first-class Task");
    assert_eq!(af_task.source, TaskSource::Autoflow);
    assert_eq!(af_task.flow_id, "default");
    assert_eq!(af_task.agent_id, "wiki_root");
    assert!(af_task.prompt.contains(&written.info.scoped_path));

    let _ = CreateTaskRequest::new("noop", "noop").with_source(TaskSource::Api);

    // 3. Kiểm tra API xóa Wiki & Source trên `TaskManager` và `WikiEngine`
    assert!(task_mgr.wiki_engine().is_some());
    engine
        .write_wiki_file(
            "entities/software-architecture/task-manager.md",
            "---\ntitle: TaskManager\ntype: entity\n---\n# TaskManager\n",
        )
        .unwrap();
    engine
        .write_wiki_file(
            &format!("sources/{}", written.info.scoped_path),
            "---\ntitle: Mirror Summary\ntype: source\n---\n# Mirror\n",
        )
        .unwrap();

    let wiki_del = task_mgr
        .delete_wiki_file("entities/software-architecture/task-manager.md")
        .unwrap();
    assert!(wiki_del.deleted);

    let src_del = task_mgr
        .delete_source_file(&written.info.scoped_path, true)
        .unwrap();
    assert!(src_del.deleted);
    assert!(src_del.mirrored_wiki_deleted);

    task_mgr.shutdown().await.unwrap();
    wiki_orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_tool_inside_daily_orch_and_followup_queue() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are daily_root.").unwrap();

    let task_cfg =
        TaskConfig::new(dir.path().join("task_store")).with_default_target("daily", "daily_root");
    let task_init = TaskManager::prepare(task_cfg).await.unwrap();

    let daily_gw = ScriptedGateway::new();
    // Turn 1 của daily_root: gọi HostTool `reminder_create` rồi kết thúc turn 1
    daily_gw.push_tool_calls(
        "Creating reminder",
        vec![(
            "call_rem_1",
            "reminder_create",
            json!({
                "title": "Review Wiki Graph",
                "due_at": "2026-12-01T09:00:00+07:00",
                "priority": "high",
                "related_wiki_links": ["[[wiki/index]]"]
            }),
        )],
    );
    daily_gw.push_text("Đã tạo nhắc nhở Review Wiki Graph.");
    // Turn 2 (tự động chạy từ followup):
    daily_gw.push_text("Đã thực hiện thêm yêu cầu followup.");

    let daily_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily assistant")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools([
            "reminder_create",
            "reminder_list",
            "reminder_complete",
            "reminder_delete",
        ]);

    let mut daily_builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![daily_agent],
        dir.path().join("daily_store"),
    ))
    .with_gateway(daily_gw.clone());

    for tool in task_init.reminder_host_tools() {
        daily_builder = daily_builder.register_tool(tool);
    }
    let daily_orch = Arc::new(daily_builder.build().await.unwrap());

    let task_mgr = task_init
        .register_flow("daily", daily_orch.clone())
        .start()
        .await
        .unwrap();

    let mut sub = task_mgr.subscribe().unwrap();

    let task = task_mgr
        .create_task(
            CreateTaskRequest::new("Tạo lịch nhắc", "Nhắc tôi xem lại Wiki Graph")
                .with_source(TaskSource::Voice),
        )
        .await
        .unwrap();

    // Đợi Task hoàn tất Turn 1
    for _ in 0..30 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if t.id == task.id {
                assert_eq!(t.status, TaskStatus::Completed);
                break;
            }
        }
    }

    // Kiểm tra reminder đã được tạo thông qua HostTool bên trong daily_orch
    let reminders = task_mgr
        .list_schedules(&ScheduleFilter::default())
        .await
        .unwrap();
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0].title, "Review Wiki Graph");

    // Gửi tiếp 1 Followup vào Task vừa hoàn tất -> tự động kích hoạt Turn 2
    let fu = task_mgr
        .send_followup(&task.id, "Bổ sung thêm ghi chú nhé", TaskSource::Api)
        .await
        .unwrap();
    assert_eq!(fu.task_id, task.id);

    for _ in 0..30 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if t.id == task.id {
                assert_eq!(
                    t.output_preview.as_deref(),
                    Some("Đã thực hiện thêm yêu cầu followup.")
                );
                break;
            }
        }
    }

    // Kiểm tra chi tiết invocation của tool `reminder_create` trong `TaskDetailView`
    let detail = task_mgr.get_task_detail(&task.id).await.unwrap();
    let root_view = detail.root_agent.as_ref().expect("root_agent view");
    assert_eq!(root_view.invocations.len(), 1);
    assert_eq!(root_view.invocations[0].tool_name, "reminder_create");
    assert_eq!(root_view.invocations[0].status, "completed");
    assert_eq!(detail.followups.len(), 1);

    task_mgr.shutdown().await.unwrap();
    daily_orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn thinking_blocks_are_stripped_and_empty_thinking_fails_task() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are daily_root.").unwrap();

    let task_cfg =
        TaskConfig::new(dir.path().join("task_store")).with_default_target("daily", "daily_root");
    let task_init = TaskManager::prepare(task_cfg).await.unwrap();

    let daily_gw = ScriptedGateway::new();
    daily_gw.push_text("<think>\nPhân tích nội bộ dài...\n</think>\n\nKết quả cuối cùng sạch sẽ.");
    daily_gw.push_text("<think>Suy nghĩ bị cắt cụt do hết max_output_tokens");

    let daily_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily assistant")
        .with_prompt_path(&prompt_path);

    let daily_orch = Arc::new(
        Orchestrator::builder(OrchestratorConfig::new(
            test_llm_config(),
            vec![daily_agent],
            dir.path().join("daily_store"),
        ))
        .with_gateway(daily_gw)
        .build()
        .await
        .unwrap(),
    );

    let task_mgr = task_init
        .register_flow("daily", daily_orch.clone())
        .start()
        .await
        .unwrap();

    let mut sub = task_mgr.subscribe().unwrap();

    // Task 1: có `<think>...</think>` kèm câu trả lời thật -> strip sạch trong output_preview
    let t1 = task_mgr
        .create_task(CreateTaskRequest::new("Task 1", "Kiểm tra lọc think"))
        .await
        .unwrap();

    for _ in 0..30 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if t.id == t1.id {
                assert_eq!(t.status, TaskStatus::Completed);
                assert_eq!(
                    t.output_preview.as_deref(),
                    Some("Kết quả cuối cùng sạch sẽ.")
                );
                break;
            }
        }
    }

    // Task 2: chỉ có `<think>` bị cắt cụt (không có câu trả lời thực) -> chuyển sang Failed thay vì Completed rỗng
    let t2 = task_mgr
        .create_task(CreateTaskRequest::new("Task 2", "Kiểm tra cạn token think"))
        .await
        .unwrap();

    for _ in 0..30 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if t.id == t2.id {
                assert_eq!(t.status, TaskStatus::Failed);
                assert_eq!(t.error_code.as_deref(), Some("PROVIDER_PROTOCOL_ERROR"));
                break;
            }
        }
    }

    task_mgr.shutdown().await.unwrap();
    daily_orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn workspace_toolkit_provides_reusable_host_tools_out_of_the_box() {
    let dir = tempfile::tempdir().unwrap();
    let layout = WorkspaceLayout::new(dir.path().join("workspace"));
    layout.ensure_dirs().unwrap();

    let prompt_path = layout.prompts_dir().join("agent.md");
    std::fs::write(&prompt_path, "You are a unified test agent.").unwrap();

    let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir()).unwrap());
    engine.apply_template(WikiTemplateId::Research).unwrap();
    let autoflow_notify = Arc::new(Notify::new());

    let task_init =
        TaskManager::prepare(TaskConfig::from_layout(&layout).with_default_agent("unified_agent"))
            .await
            .unwrap();

    assert_eq!(task_init.task_host_tools().len(), 6);
    assert_eq!(task_init.reminder_host_tools().len(), 4);
    assert_eq!(task_init.all_host_tools().len(), 10);

    let toolkit = WorkspaceToolKit::from_layout(&layout, engine.clone(), autoflow_notify.clone())
        .with_context_limit_tokens(128_000)
        .with_youtube_options("en,vi", 10);

    // Kiểm tra các nhóm tool xuất ra từ WorkspaceToolKit
    assert_eq!(toolkit.pdf_host_tools().len(), 2);
    assert_eq!(toolkit.youtube_host_tools().len(), 2);
    assert_eq!(toolkit.collector_host_tools().len(), 4);
    assert_eq!(toolkit.report_host_tools().len(), 2);
    assert!(!toolkit.wiki_readonly_host_tools().is_empty());
    assert!(toolkit.wiki_host_tools().len() > toolkit.wiki_readonly_host_tools().len());

    // Tạo sample PDF và nạp vào `toolkit.pdf_state`
    let pdf_path = dir.path().join("sample.pdf");
    std::fs::write(&pdf_path, build_sample_technical_pdf()).unwrap();
    toolkit.pdf_state.set_input_paths(vec![pdf_path]);

    // Kịch bản Agent gọi các HostTool tái sử dụng:
    // Bước 1: markdown_create + anydoc_inspect_pdf
    // Bước 2: markdown_read + anydoc_save_to_sources + add_to_sources
    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Creating markdown report and inspecting PDF",
        vec![
            (
                "c1",
                "markdown_create",
                json!({
                    "filename": "architecture-summary.md",
                    "title": "Architecture Summary",
                    "markdown_body": "Detailed standalone report content."
                }),
            ),
            ("c2", "anydoc_inspect_pdf", json!({ "doc_index": 1 })),
        ],
    );
    gw.push_tool_calls(
        "Reading report and saving sources",
        vec![
            (
                "c3",
                "markdown_read",
                json!({
                    "filename": "architecture-summary.md"
                }),
            ),
            (
                "c4",
                "anydoc_save_to_sources",
                json!({
                    "doc_index": 1,
                    "topic_folder": "ai-engineering/hybrid-search",
                    "title": "Hybrid Search Technical PDF",
                    "tags": ["topic/ai-engineering/hybrid-search", "pdf"]
                }),
            ),
            (
                "c5",
                "add_to_sources",
                json!({
                    "topic_folder": "software-architecture/reusable-tools",
                    "file_stem": "workspace-toolkit",
                    "title": "WorkspaceToolKit Design",
                    "markdown_body": "## Overview\nHost applications reuse built-in HostTools directly.",
                    "tags": ["topic/software-architecture/reusable-tools", "rust"]
                }),
            ),
        ],
    );
    gw.push_text("Đã hoàn tất kiểm tra toàn bộ bộ công cụ tái sử dụng.");

    let agent = AgentDefinition::new("unified_agent", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Unified agent testing WorkspaceToolKit")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools([
            "markdown_create",
            "markdown_read",
            "anydoc_inspect_pdf",
            "anydoc_save_to_sources",
            "add_to_sources",
            "wiki_tree",
        ])
        .with_steps(10);

    let mut builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![agent],
        layout.orchestrator_store_dir(),
    ))
    .with_gateway(gw);

    for tool in toolkit.all_host_tools(&task_init) {
        builder = builder.register_tool(tool);
    }
    let orch = Arc::new(builder.build().await.unwrap());

    let task_mgr = task_init
        .with_orchestrator(orch.clone())
        .start()
        .await
        .unwrap();
    let mut sub = task_mgr.subscribe().unwrap();

    let task = task_mgr
        .create_task(CreateTaskRequest::new(
            "Test WorkspaceToolKit",
            "Run markdown, pdf, and add_to_sources tools.",
        ))
        .await
        .unwrap();

    let mut saw_terminal = false;
    for _ in 0..200 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(5), sub.recv()).await
        {
            if t.id == task.id {
                assert_eq!(t.status, TaskStatus::Completed);
                saw_terminal = true;
                break;
            }
        }
    }
    assert!(saw_terminal, "Task should reach terminal state");

    assert!(layout
        .reports_dir()
        .join("architecture-summary.md")
        .exists());
    assert_eq!(toolkit.pdf_state.recorded_docs().len(), 1);
    assert!(toolkit.pdf_state.recorded_docs()[0]
        .saved_source_scoped_path
        .is_some());

    let detail = task_mgr.get_task_detail(&task.id).await.unwrap();
    let root = detail.root_agent.expect("root session");
    assert_eq!(root.invocations.len(), 5);

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}
