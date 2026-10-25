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
use orchestrator_flow_rs::{
    build_sample_technical_pdf, CreateScheduleRequest, CreateTaskRequest, Priority,
    ScheduleActionKind, ScheduleFilter, TaskConfig, TaskEvent, TaskFeedItem, TaskFilter,
    TaskManager, TaskRuntimeConfig, TaskScope, TaskSource, TaskStatus, WikiAutoFlowBinding,
    WorkspaceLayout, WorkspaceToolKit, COLLECTOR_TOOL_NAMES,
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

    // Xây dựng daily_orch (Flow 1) có đăng ký các reminder HostTool từ Pha 1
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

    // Xây dựng wiki_orch (Flow 2)
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
            session_mode: orchestrator_flow_rs::ScheduleSessionMode::NewSession,
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

    // 2. Ghi một file mới vào `sources/` và bắn `autoflow_notify` (mô phỏng `add_to_sources` từ Flow 1)
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
    assert_eq!(toolkit.web_host_tools().len(), 6);
    assert_eq!(toolkit.collector_host_tools().len(), 10);
    assert_eq!(toolkit.report_host_tools().len(), 2);
    assert!(!toolkit.wiki_readonly_host_tools().is_empty());
    assert!(toolkit.wiki_host_tools().len() > toolkit.wiki_readonly_host_tools().len());

    // Kiểm tra phân bổ Tool chuẩn giữa Flow 1 (`daily_flow_host_tools` — Thu thập & Đa chức năng)
    // và Flow 2 (`wiki_flow_host_tools` — Đường ống biên dịch Wiki khi `sources/` có file mới)
    let daily_tool_names: Vec<String> = toolkit
        .daily_flow_host_tools(&task_init)
        .iter()
        .map(|t| t.descriptor().name)
        .collect();
    for expected in [
        "yt_list_playlist",
        "yt_fetch_transcript",
        "anydoc_inspect_pdf",
        "anydoc_save_to_sources",
        "fetch_html",
        "add_to_sources",
        "markdown_create",
        "image_create",
        "tts_synthesize",
        "artifact_create",
        "reminder_create",
        "wiki_query",
        "wiki_get",
    ] {
        assert!(
            daily_tool_names.iter().any(|n| n == expected),
            "daily_flow_host_tools must include '{expected}'"
        );
    }
    assert!(
        !daily_tool_names.iter().any(|n| n == "wiki_ingest_source"),
        "daily_flow_host_tools must remain read-only on wiki/"
    );

    let wiki_tool_names: Vec<String> = toolkit
        .wiki_flow_host_tools()
        .iter()
        .map(|t| t.descriptor().name)
        .collect();
    assert_eq!(wiki_tool_names.len(), toolkit.wiki_host_tools().len());
    for &collector_tool in COLLECTOR_TOOL_NAMES {
        assert!(
            daily_tool_names.iter().any(|n| n == collector_tool),
            "daily_flow_host_tools must include collector tool '{collector_tool}'"
        );
        assert!(
            !wiki_tool_names.iter().any(|n| n == collector_tool),
            "wiki_flow_host_tools must not include external collector tool '{collector_tool}'"
        );
    }

    // Tạo sample PDF và nạp vào `toolkit.pdf_state`
    let pdf_path = dir.path().join("sample.pdf");
    std::fs::write(&pdf_path, build_sample_technical_pdf()).unwrap();
    toolkit.pdf_state.set_input_paths(vec![pdf_path]);

    // Nạp sẵn HTML mẫu vào `toolkit.html_workspace` để test các tool Web (`fetch_html`, `html_extract_article`)
    let sample_url = "https://example.com/rust-agent-architecture.html";
    toolkit.html_workspace.insert_cached_html(
        sample_url,
        r#"<!DOCTYPE html><html><head><title>Rust Agent Architecture</title></head>
        <body><article><h1>Rust Agent Architecture</h1><h2>High-performance async task orchestration</h2>
        <p>Background tasks and Dual-Flow knowledge compilation run on SQLite WAL.</p></article></body></html>"#,
    );

    // Kịch bản Agent gọi các HostTool tái sử dụng:
    // Bước 1: markdown_create + anydoc_inspect_pdf + fetch_html + html_extract_article
    // Bước 2: markdown_read + anydoc_save_to_sources + add_to_sources
    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Creating markdown report, inspecting PDF, and extracting Web article",
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
            ("c_web1", "fetch_html", json!({ "url": sample_url })),
            (
                "c_web2",
                "html_extract_article",
                json!({ "url": sample_url }),
            ),
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
            "fetch_html",
            "html_extract_article",
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
    assert_eq!(root.invocations.len(), 7);

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn javis_guardrails_provenance_policy_and_autoflow_change_signature() {
    let dir = tempfile::tempdir().unwrap();
    let layout = WorkspaceLayout::new(dir.path().join("workspace"));
    layout.ensure_dirs().unwrap();

    let prompt_path = layout.prompts_dir().join("agent.md");
    std::fs::write(&prompt_path, "You are a guarded Dual-Flow agent.").unwrap();

    let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir()).unwrap());
    engine.apply_template(WikiTemplateId::Research).unwrap();
    let autoflow_notify = Arc::new(Notify::new());

    let runtime_cfg = TaskRuntimeConfig {
        scheduler_tick: Duration::from_millis(40),
        autoflow_poll_interval: Duration::from_millis(80),
        ..Default::default()
    };

    let task_init = TaskManager::prepare(
        TaskConfig::from_layout(&layout)
            .with_default_agent("daily_root")
            .with_runtime(runtime_cfg),
    )
    .await
    .unwrap();

    let toolkit = WorkspaceToolKit::from_layout(&layout, engine.clone(), autoflow_notify.clone());

    let gw = ScriptedGateway::new();
    // Turn 1 của daily_root: thử 3 lệnh `add_to_sources` bị chặn (provenance="assistant", secret, quá ngắn)
    // và 1 lệnh hợp lệ có đầy đủ `provenance="user"`, `conflict_with`, `open_questions`.
    gw.push_tool_calls(
        "Testing add_to_sources guardrails",
        vec![
            (
                "call_assistant_blocked",
                "add_to_sources",
                json!({
                    "topic_folder": "software-architecture/self-learning",
                    "title": "Unverified AI Speculation",
                    "markdown_body": "This is an unverified claim invented solely by the assistant without user or source backing.",
                    "provenance": "assistant"
                }),
            ),
            (
                "call_secret_blocked",
                "add_to_sources",
                json!({
                    "topic_folder": "software-architecture/self-learning",
                    "title": "Leaked Key Note",
                    "markdown_body": "Production credential sk-proj-1234567890abcdef should never enter Wiki sources.",
                    "provenance": "user"
                }),
            ),
            (
                "call_short_blocked",
                "add_to_sources",
                json!({
                    "topic_folder": "software-architecture/self-learning",
                    "title": "Too Short",
                    "markdown_body": "ok",
                    "provenance": "user"
                }),
            ),
            (
                "call_valid_staged",
                "add_to_sources",
                json!({
                    "topic_folder": "software-architecture/self-learning",
                    "file_stem": "javis-guardrails",
                    "title": "Javis Guardrails in Dual-Flow",
                    "markdown_body": "## Quyết định Kiến trúc\nTách biệt hoàn toàn đề xuất của AI với quyền ghi Wiki và kiểm tra chữ ký thay đổi Vault trước khi chạy Lint.",
                    "tags": ["topic/software-architecture/self-learning", "rust"],
                    "provenance": "user",
                    "target_hint": "concepts/software-architecture/dual-flow",
                    "conflict_with": "concepts/software-architecture/legacy-autoflow",
                    "open_questions": ["Cần đo thêm ngưỡng debounce tối ưu cho homelab"]
                }),
            ),
        ],
    );
    gw.push_text("Đã kiểm tra xong các cổng bảo vệ của add_to_sources.");
    // Phản hồi cho Auto-Flow Task #1 (không sửa đổi Vault để kiểm tra Anti-Stall & Change Signature)
    gw.push_text("Auto-Flow Stage 1 finished without mutating vault.");
    // Phản hồi cho Auto-Flow Task #2 (chỉ chạy sau khi thực sự có file nguồn thứ 2 làm đổi chữ ký Vault):
    // gọi wiki_ingest_source cho cả 2 nguồn + wiki_file_synthesis + wiki_lint để kiểm chứng wiki_pages_written & last_lint_healthy.
    gw.push_tool_calls(
        "Ingesting sources and running lint",
        vec![
            (
                "call_ingest_1",
                "wiki_ingest_source",
                json!({
                    "source_rel_path": "software-architecture/self-learning/javis-guardrails.md",
                    "title": "Javis Guardrails",
                    "one_line_summary": "Summary of Javis guardrails in Dual-Flow.",
                    "wiki_markdown_body": "# Javis Guardrails\n\nSummary of [[sources/software-architecture/self-learning/javis-guardrails]] and [[wiki/syntheses/software-architecture/self-learning-synthesis]].\n",
                    "tags": ["topic/software-architecture/self-learning"]
                }),
            ),
            (
                "call_ingest_2",
                "wiki_ingest_source",
                json!({
                    "source_rel_path": "software-architecture/self-learning/second-source.md",
                    "title": "Second Source",
                    "one_line_summary": "Summary of second source.",
                    "wiki_markdown_body": "# Second Source\n\nSummary of [[sources/software-architecture/self-learning/second-source]] and [[wiki/syntheses/software-architecture/self-learning-synthesis]].\n",
                    "tags": ["topic/software-architecture/self-learning"]
                }),
            ),
            (
                "call_synth",
                "wiki_file_synthesis",
                json!({
                    "rel_path": "syntheses/software-architecture/self-learning-synthesis.md",
                    "query": "Self-learning guardrails synthesis",
                    "title": "Self Learning Synthesis",
                    "one_line_summary": "Synthesis of self-learning guardrails.",
                    "markdown_body": "# Self Learning Synthesis\n\n- [[wiki/sources/software-architecture/self-learning/javis-guardrails]]\n- [[wiki/sources/software-architecture/self-learning/second-source]]\n",
                    "tags": ["topic/software-architecture/self-learning"]
                }),
            ),
            ("call_lint", "wiki_lint", json!({})),
        ],
    );
    gw.push_text("Auto-Flow Stage 2 ran after new source changed vault signature.");

    let daily_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["add_to_sources"])
        .with_steps(6);

    let wiki_agent = AgentDefinition::new("wiki_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Wiki root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["wiki_ingest_source", "wiki_file_synthesis", "wiki_lint"])
        .with_steps(6);

    let mut builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![daily_agent, wiki_agent],
        layout.orchestrator_store_dir(),
    ))
    .with_gateway(gw);

    for tool in toolkit.all_host_tools(&task_init) {
        builder = builder.register_tool(tool);
    }
    let orch = Arc::new(builder.build().await.unwrap());

    let task_mgr = task_init
        .with_orchestrator(orch.clone())
        .with_wiki_autoflow(WikiAutoFlowBinding::for_agent(
            engine.clone(),
            autoflow_notify.clone(),
            "wiki_root",
        ))
        .start()
        .await
        .unwrap();

    let mut sub = task_mgr.subscribe().unwrap();

    let daily_task = task_mgr
        .create_task(CreateTaskRequest::new(
            "Test Guardrails",
            "Run add_to_sources guardrail checks.",
        ))
        .await
        .unwrap();

    // Đợi cả `daily_task` và `Autoflow` Task #1 hoàn tất
    let mut completed_ids = Vec::new();
    for _ in 0..80 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            assert_eq!(t.status, TaskStatus::Completed);
            completed_ids.push((t.id.clone(), t.source));
            if completed_ids.len() >= 2 {
                break;
            }
        }
    }
    assert_eq!(completed_ids.len(), 2);

    // Kiểm tra 3 lệnh đầu bị chặn và chỉ có đúng 1 file nguồn hợp lệ được ghi vào `sources/`
    assert_eq!(engine.status().unwrap().sources_count, 1);
    let af_task_1_id = completed_ids
        .iter()
        .find(|(_, src)| *src == TaskSource::Autoflow)
        .map(|(id, _)| id.clone())
        .expect("Autoflow Task #1 id");
    // Đợi một nhịp ngắn để Auto-Flow cập nhật progress_summary sau khi đối chiếu post_sig == pre_sig
    tokio::time::sleep(Duration::from_millis(80)).await;
    let af_task_1 = task_mgr.get_task(&af_task_1_id).await.unwrap();
    assert_eq!(
        af_task_1.progress_summary.as_deref(),
        Some("Auto-Flow stopped: no Vault changes produced in this stage")
    );

    let detail = task_mgr.get_task_detail(&daily_task.id).await.unwrap();
    let summary = detail.execution_summary.expect("execution summary");
    assert_eq!(summary.succeeded_tool_calls, 1);
    assert_eq!(summary.failed_tool_calls, 3);
    assert_eq!(
        summary.failed_error_codes,
        vec![
            "PROVENANCE_BLOCKED".to_string(),
            "CONTENT_POLICY_BLOCKED".to_string(),
            "INSUFFICIENT_SUBSTANCE".to_string(),
        ]
    );
    assert_eq!(
        summary.sources_written,
        vec!["sources/software-architecture/self-learning/javis-guardrails.md".to_string()]
    );
    assert!(summary.wiki_pages_written.is_empty());
    assert_eq!(summary.last_lint_healthy, None);

    let root = detail.root_agent.expect("root session");
    assert_eq!(root.invocations.len(), 4);
    assert_eq!(root.invocations[0].status, "failed");
    assert_eq!(root.invocations[1].status, "failed");
    assert_eq!(root.invocations[2].status, "failed");
    assert_eq!(root.invocations[3].status, "completed");

    // Kiểm tra nội dung file nguồn đã ghi có frontmatter provenance/conflict_with và callout Câu hỏi mở
    let staged_raw = std::fs::read_to_string(
        layout
            .wiki_vault_dir()
            .join("sources/software-architecture/self-learning/javis-guardrails.md"),
    )
    .unwrap();
    assert!(staged_raw.contains("provenance: \"user\""));
    assert!(
        staged_raw.contains("conflict_with: \"concepts/software-architecture/legacy-autoflow\"")
    );
    assert!(staged_raw.contains("## Mâu thuẫn & Câu hỏi mở (Conflict & Open Questions)"));
    assert!(staged_raw.contains("Cần đo thêm ngưỡng debounce tối ưu cho homelab"));

    // Bắn thêm `autoflow_notify` khi Vault KHÔNG thay đổi -> Change Signature Guard phải chặn, không tạo thêm Task Autoflow!
    autoflow_notify.notify_one();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let autoflow_tasks_before = task_mgr
        .list_tasks(&TaskFilter {
            source: Some(TaskSource::Autoflow),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        autoflow_tasks_before.total, 1,
        "Change Signature Guard must prevent duplicate Autoflow tasks when vault is unchanged"
    );

    // Bây giờ thêm một file nguồn mới làm thay đổi chữ ký Vault -> Autoflow được phép chạy Task #2!
    engine
        .write_topic_source(WriteTopicSourceRequest {
            topic_folder: "software-architecture/self-learning".to_string(),
            file_stem: Some("second-source".to_string()),
            title: "Second Source".to_string(),
            tags: vec!["topic/software-architecture/self-learning".to_string()],
            extra_frontmatter: Vec::new(),
            header_bullets: Vec::new(),
            body_heading: None,
            raw_body: "## New Content\nVault signature has now changed.".to_string(),
            preview_chars: Some(200),
        })
        .unwrap();
    autoflow_notify.notify_one();

    let mut second_autoflow_id = None;
    for _ in 0..40 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if t.source == TaskSource::Autoflow {
                second_autoflow_id = Some(t.id);
                break;
            }
        }
    }
    let af_task_2_id =
        second_autoflow_id.expect("Autoflow should run again once vault signature changes");
    let af2_detail = task_mgr.get_task_detail(&af_task_2_id).await.unwrap();
    let af2_summary = af2_detail.execution_summary.expect("af2 execution summary");
    assert_eq!(af2_summary.succeeded_tool_calls, 4);
    assert_eq!(af2_summary.failed_tool_calls, 0);
    assert_eq!(
        af2_summary.wiki_pages_written,
        vec![
            "wiki/sources/software-architecture/self-learning/javis-guardrails.md".to_string(),
            "wiki/sources/software-architecture/self-learning/second-source.md".to_string(),
            "wiki/syntheses/software-architecture/self-learning-synthesis.md".to_string(),
        ]
    );
    assert_eq!(af2_summary.last_lint_healthy, Some(true));

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn cross_scope_admin_queries_search_filter_and_device_scope_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are daily_root.").unwrap();

    let task_cfg =
        TaskConfig::new(dir.path().join("task_store")).with_default_target("daily", "daily_root");
    let task_init = TaskManager::prepare(task_cfg).await.unwrap();

    let daily_gw = ScriptedGateway::new();
    daily_gw.push_text("Admin task output.");
    daily_gw.push_text("Device 1 task output.");
    daily_gw.push_text("Device 2 task output.");

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

    let mut sub_all = task_mgr.subscribe_all().unwrap();

    let admin_task = task_mgr
        .create_task(CreateTaskRequest::new(
            "System homelab audit",
            "Run homelab audit",
        ))
        .await
        .unwrap();

    let dev1_scope = TaskScope::new("device:1", "agent:10");
    let dev1_task = task_mgr
        .create_task_scoped(
            &dev1_scope,
            CreateTaskRequest::new("Living room voice request", "Check weather forecast")
                .with_source(TaskSource::Voice),
        )
        .await
        .unwrap();

    let dev2_scope = TaskScope::new("device:2", "agent:10");
    let dev2_task = task_mgr
        .create_task_scoped(
            &dev2_scope,
            CreateTaskRequest::new("Bedroom voice request", "Set alarm note")
                .with_source(TaskSource::Voice),
        )
        .await
        .unwrap();

    let mut terminal_count = 0;
    for _ in 0..40 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { .. }))) =
            tokio::time::timeout(Duration::from_secs(2), sub_all.recv()).await
        {
            terminal_count += 1;
            if terminal_count == 3 {
                break;
            }
        }
    }
    assert_eq!(
        terminal_count, 3,
        "subscribe_all should receive terminal events from all scopes"
    );

    // Admin list_all_tasks sees all 3 tasks across personal + device:1 + device:2
    let all_page = task_mgr
        .list_all_tasks(&TaskFilter::default())
        .await
        .unwrap();
    assert_eq!(all_page.total, 3);

    // SQL query filter (`query`) matches title/prompt with accurate total count
    let search_page = task_mgr
        .list_all_tasks(&TaskFilter {
            query: Some("weather".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(search_page.total, 1);
    assert_eq!(search_page.items[0].id, dev1_task.id);

    // Device 1 sees its own task AND the system personal:homelab task, but NOT Device 2's task
    let dev1_page = task_mgr
        .list_tasks_scoped(&dev1_scope, &TaskFilter::default())
        .await
        .unwrap();
    assert_eq!(dev1_page.total, 2);
    assert!(dev1_page.items.iter().any(|t| t.id == dev1_task.id));
    assert!(dev1_page.items.iter().any(|t| t.id == admin_task.id));
    assert!(!dev1_page.items.iter().any(|t| t.id == dev2_task.id));

    // Device 1 can inspect detail of system personal:homelab task, but NOT Device 2's task
    let admin_detail_from_dev1 = task_mgr
        .get_task_detail_scoped(&dev1_scope, &admin_task.id)
        .await
        .unwrap();
    assert!(admin_detail_from_dev1.root_agent.is_some());
    assert!(task_mgr
        .get_task_detail_scoped(&dev1_scope, &dev2_task.id)
        .await
        .is_err());

    // Admin *_any_scope can inspect and delete Device 1's task
    let dev1_detail_admin = task_mgr
        .get_task_detail_any_scope(&dev1_task.id)
        .await
        .unwrap();
    assert_eq!(dev1_detail_admin.task.owner_id, "device:1");
    task_mgr.delete_task_any_scope(&dev1_task.id).await.unwrap();
    assert_eq!(
        task_mgr
            .list_all_tasks(&TaskFilter::default())
            .await
            .unwrap()
            .total,
        2
    );

    task_mgr.shutdown().await.unwrap();
    daily_orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagent_multi_kind_artifacts_report_image_tts_and_data_verified() {
    let dir = tempfile::tempdir().unwrap();
    let layout = WorkspaceLayout::new(dir.path().join("workspace"));
    layout.ensure_dirs().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a multi-modal orchestrator.").unwrap();

    let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir()).unwrap());
    engine.apply_template(WikiTemplateId::Personal).unwrap();
    let autoflow_notify = Arc::new(Notify::new());
    let toolkit = WorkspaceToolKit::from_layout(&layout, engine.clone(), autoflow_notify.clone());

    let task_cfg = TaskConfig::from_layout(&layout).with_default_agent("daily_root");
    let task_init = TaskManager::prepare(task_cfg).await.unwrap();

    let gw = ScriptedGateway::new();
    // 1. Root agent delegates to `media_creator` subagent
    gw.push_tool_calls(
        "Delegating to media_creator subagent",
        vec![(
            "call_sub_media",
            "subagent",
            json!({
                "agent": "media_creator",
                "description": "Create report, PNG banner, SVG diagram, TTS audio, and JSON metrics",
                "prompt": "Generate all 5 artifacts for the B200 briefing."
            }),
        )],
    );
    // 2. Subagent `media_creator` calls 5 tools
    gw.push_tool_calls(
        "Creating report, images, TTS audio, and JSON artifact",
        vec![
            (
                "call_md",
                "markdown_create",
                json!({
                    "filename": "b200-briefing.md",
                    "title": "B200 Architecture Briefing",
                    "markdown_body": "Blackwell B200 delivers 20 PFLOPS FP4 with NVLink 5."
                }),
            ),
            (
                "call_png",
                "image_create",
                json!({
                    "filename": "b200-banner.png",
                    "prompt": "Futuristic NVIDIA Blackwell B200 GPU rack in dark datacenter",
                    "format": "png",
                    "width": 64,
                    "height": 48
                }),
            ),
            (
                "call_svg",
                "image_create",
                json!({
                    "filename": "b200-topology.svg",
                    "title": "NVLink 5 Topology",
                    "prompt": "Dual-die Blackwell GPU interconnect",
                    "format": "svg",
                    "width": 800,
                    "height": 400
                }),
            ),
            (
                "call_tts",
                "tts_synthesize",
                json!({
                    "filename": "b200-voiceover.wav",
                    "text": "Báo cáo nhanh kiến trúc Blackwell B200 với băng thông NVLink thế hệ năm.",
                    "voice": "vi-female",
                    "language": "vi-VN",
                    "speed": 1.1
                }),
            ),
            (
                "call_json",
                "artifact_create",
                json!({
                    "filename": "b200-benchmarks.json",
                    "mime_type": "application/json",
                    "content": "{\"fp4_pflops\":20,\"hbm3e_gb\":192}",
                    "role": "intermediate"
                }),
            ),
        ],
    );
    // 3. Subagent finishes
    gw.push_text("Đã tạo đủ 5 artifacts: báo cáo Markdown, ảnh PNG, sơ đồ SVG, giọng đọc TTS WAV và dữ liệu JSON.");
    // 4. Root agent finishes
    gw.push_text("Hoàn tất gói đa phương tiện B200 gồm báo cáo, hình ảnh và âm thanh TTS.");

    let root_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["subagent"])
        .with_allowed_subagents(["media_creator"])
        .with_steps(6);

    let media_subagent = AgentDefinition::new("media_creator", "test", "gpt-test")
        .with_mode(AgentMode::Subagent)
        .with_description("Multi-media artifact creator")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools([
            "markdown_create",
            "image_create",
            "tts_synthesize",
            "artifact_create",
        ])
        .with_steps(8);

    let mut builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![root_agent, media_subagent],
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
            "B200 Multi-Modal Briefing",
            "Tạo báo cáo, ảnh minh họa và audio TTS cho B200.",
        ))
        .await
        .unwrap();

    for _ in 0..40 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(3), sub.recv()).await
        {
            if t.id == task.id {
                assert_eq!(t.status, TaskStatus::Completed);
                break;
            }
        }
    }

    let detail = task_mgr.get_task_detail(&task.id).await.unwrap();
    assert_eq!(
        detail.artifacts.len(),
        5,
        "should have 5 artifacts recorded"
    );

    let summary = detail.execution_summary.expect("execution_summary");
    assert_eq!(summary.total_artifacts, 5);
    assert_eq!(summary.primary_artifacts, 4);
    assert_eq!(summary.report_artifacts, 1);
    assert_eq!(summary.image_artifacts, 2);
    assert_eq!(summary.audio_artifacts, 1);
    assert_eq!(summary.artifact_ids.len(), 5);

    // Kiểm tra từng loại artifact và đọc lại kèm xác minh SHA-256
    let md_art = detail
        .artifacts
        .iter()
        .find(|a| a.name == "b200-briefing.md")
        .expect("markdown artifact");
    assert_eq!(md_art.kind, orchestrator_flow_rs::TaskArtifactKind::Report);
    assert_eq!(md_art.role, orchestrator_flow_rs::TaskArtifactRole::Primary);
    assert_eq!(md_art.producer_agent_id, "media_creator");
    assert_eq!(
        md_art.producer_tool_name.as_deref(),
        Some("markdown_create")
    );
    let md_text = task_mgr
        .open_task_artifact(&task.id, &md_art.id)
        .await
        .unwrap()
        .read_to_string()
        .await
        .unwrap();
    assert!(md_text.contains("# B200 Architecture Briefing"));
    assert!(md_text.contains("20 PFLOPS FP4"));

    let png_art = detail
        .artifacts
        .iter()
        .find(|a| a.name == "b200-banner.png")
        .expect("png artifact");
    assert_eq!(png_art.kind, orchestrator_flow_rs::TaskArtifactKind::Image);
    assert_eq!(png_art.mime_type, "image/png");
    assert_eq!(png_art.producer_tool_name.as_deref(), Some("image_create"));
    let (_, png_bytes) = task_mgr
        .read_task_artifact_bytes(&task.id, &png_art.id)
        .await
        .unwrap();
    assert!(png_bytes.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));

    let svg_art = detail
        .artifacts
        .iter()
        .find(|a| a.name == "b200-topology.svg")
        .expect("svg artifact");
    assert_eq!(svg_art.kind, orchestrator_flow_rs::TaskArtifactKind::Image);
    assert_eq!(svg_art.mime_type, "image/svg+xml");
    let svg_text = task_mgr
        .open_task_artifact_any_scope(&task.id, &svg_art.id)
        .await
        .unwrap()
        .read_to_string()
        .await
        .unwrap();
    assert!(svg_text.contains("<svg"));
    assert!(svg_text.contains("NVLink 5 Topology"));

    let wav_art = detail
        .artifacts
        .iter()
        .find(|a| a.name == "b200-voiceover.wav")
        .expect("wav artifact");
    assert_eq!(wav_art.kind, orchestrator_flow_rs::TaskArtifactKind::Audio);
    assert_eq!(wav_art.mime_type, "audio/wav");
    assert_eq!(
        wav_art.producer_tool_name.as_deref(),
        Some("tts_synthesize")
    );
    let (_, wav_bytes) = task_mgr
        .read_task_artifact_bytes_any_scope(&task.id, &wav_art.id)
        .await
        .unwrap();
    assert!(wav_bytes.starts_with(b"RIFF"));
    assert_eq!(&wav_bytes[8..12], b"WAVE");

    let json_art = detail
        .artifacts
        .iter()
        .find(|a| a.name == "b200-benchmarks.json")
        .expect("json artifact");
    assert_eq!(json_art.kind, orchestrator_flow_rs::TaskArtifactKind::Data);
    assert_eq!(
        json_art.role,
        orchestrator_flow_rs::TaskArtifactRole::Intermediate
    );
    let json_text = task_mgr
        .open_task_artifact(&task.id, &json_art.id)
        .await
        .unwrap()
        .read_to_string()
        .await
        .unwrap();
    assert!(json_text.contains("\"fp4_pflops\":20"));

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}
