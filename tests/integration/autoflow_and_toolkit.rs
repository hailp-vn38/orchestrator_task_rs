use super::*;

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
    assert_eq!(toolkit.pdf_host_tools().len(), 1);
    assert_eq!(toolkit.youtube_host_tools().len(), 2);
    assert_eq!(toolkit.web_host_tools().len(), 6);
    assert_eq!(toolkit.collector_host_tools().len(), 9);
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
                "add_to_sources",
                json!({
                    "pdf_doc_index": 1,
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
