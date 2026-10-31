use super::*;

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
