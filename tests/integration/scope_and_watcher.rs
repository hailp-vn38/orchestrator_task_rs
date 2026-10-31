use super::*;

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
async fn watcher_emits_task_progress_for_message_committed_and_tool_completed() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a helpful test agent.").unwrap();

    let task_cfg = TaskConfig::new(dir.path().join("task_store")).with_default_agent("daily_root");
    let task_init = TaskManager::prepare(task_cfg).await.unwrap();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Đang kiểm tra danh sách nhắc nhở hiện tại trước khi tổng hợp.",
        vec![("call_rem_list", "reminder_list", json!({}))],
    );
    gw.push_text("Đã kiểm tra xong, hiện chưa có nhắc nhở nào.");

    let daily_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["reminder_list"]);

    let mut builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![daily_agent],
        dir.path().join("orch_store"),
    ))
    .with_gateway(gw);
    for tool in task_init.reminder_host_tools() {
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
            "Kiểm tra nhắc nhở",
            "Kiểm tra danh sách nhắc nhở",
        ))
        .await
        .unwrap();

    let mut progress_summaries = Vec::new();
    let mut committed_texts = Vec::new();
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
            Ok(Some(TaskFeedItem::Event(ev))) => match ev {
                TaskEvent::TaskProgress {
                    task_id,
                    progress_summary: Some(summary),
                    ..
                } if task_id == task.id => {
                    progress_summaries.push(summary);
                }
                TaskEvent::TaskExecutionStream {
                    task_id,
                    event_type,
                    payload,
                    ..
                } if task_id == task.id && event_type == "message.committed" => {
                    if let Some(t) = payload["text"].as_str() {
                        committed_texts.push(t.to_string());
                    }
                }
                TaskEvent::TaskTerminal { task: t } if t.id == task.id => {
                    assert_eq!(t.status, TaskStatus::Completed);
                    break;
                }
                _ => {}
            },
            _ => break,
        }
    }

    assert_eq!(
        committed_texts,
        vec![
            "Đang kiểm tra danh sách nhắc nhở hiện tại trước khi tổng hợp.".to_string(),
            "Đã kiểm tra xong, hiện chưa có nhắc nhở nào.".to_string(),
        ]
    );
    assert!(
        progress_summaries
            .iter()
            .any(|s| s == "Đang kiểm tra danh sách nhắc nhở hiện tại trước khi tổng hợp."),
        "expected message.committed text in TaskProgress, got: {progress_summaries:?}"
    );
    assert!(
        progress_summaries
            .iter()
            .any(|s| s == "Completed tool 'reminder_list'"),
        "expected tool.completed summary in TaskProgress, got: {progress_summaries:?}"
    );

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}
