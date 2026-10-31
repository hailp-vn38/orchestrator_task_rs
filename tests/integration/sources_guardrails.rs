use super::*;

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
async fn add_to_sources_stages_markdown_body_when_zero_pdf_doc_index_and_empty_optional_fields_are_passed(
) {
    let dir = tempfile::tempdir().unwrap();
    let layout = WorkspaceLayout::new(dir.path().join("workspace"));
    layout.ensure_dirs().unwrap();

    let prompt_path = layout.prompts_dir().join("agent.md");
    std::fs::write(&prompt_path, "You are report_stager.").unwrap();

    let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir()).unwrap());
    engine.apply_template(WikiTemplateId::Research).unwrap();
    let autoflow_notify = Arc::new(Notify::new());

    let task_init =
        TaskManager::prepare(TaskConfig::from_layout(&layout).with_default_agent("report_stager"))
            .await
            .unwrap();

    let toolkit = WorkspaceToolKit::from_layout(&layout, engine.clone(), autoflow_notify.clone());

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Staging transcript synthesis with all optional fields populated as zero/empty",
        vec![(
            "call_stage_breakaway_gap",
            "add_to_sources",
            json!({
                "conflict_with": "",
                "doc_index": 0,
                "file_stem": "breakaway-gap-fvg-transcript-synthesis",
                "markdown_body": "# Breakaway Gap/FVG: nhận diện trong bối cảnh dòng giá\n\n## Tóm tắt cốt lõi\nFVG không nên được đánh giá chỉ bằng hình dạng ba nến mà cần đặt trong bối cảnh mục tiêu kế tiếp.",
                "open_questions": [
                    "Cần đối chiếu video gốc để xác nhận thuật ngữ ASR như MMT, PD Array."
                ],
                "pdf_doc_index": 0,
                "pdf_path": "",
                "provenance": "source",
                "tags": [
                    "topic/trading/breakaway-gap-fvg",
                    "technical-analysis",
                    "market-structure",
                    "fair-value-gap"
                ],
                "target_hint": "concepts/quantitative-finance/breakaway-gap-fvg",
                "title": "Breakaway Gap/FVG — nhận diện theo bối cảnh và mục tiêu",
                "topic_folder": "trading/breakaway-gap",
                "video_id": ""
            }),
        )],
    );
    gw.push_text("Đã lưu bản tổng hợp vào sources/trading/breakaway-gap/breakaway-gap-fvg-transcript-synthesis.md.");

    let stager_agent = AgentDefinition::new("report_stager", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Report stager")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["add_to_sources"])
        .with_steps(4);

    let mut builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![stager_agent],
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
            "Stage Breakaway Gap Synthesis",
            "Stage synthesized transcript into sources.",
        ))
        .await
        .unwrap();

    let mut saw_terminal = false;
    for _ in 0..80 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(2), sub.recv()).await
        {
            if t.id == task.id {
                assert_eq!(t.status, TaskStatus::Completed);
                saw_terminal = true;
                break;
            }
        }
    }
    assert!(saw_terminal, "task should complete");

    let detail = task_mgr.get_task_detail(&task.id).await.unwrap();
    let root = detail.root_agent.expect("root session");
    assert_eq!(root.invocations.len(), 1);
    assert_eq!(
        root.invocations[0].status, "completed",
        "expected add_to_sources to succeed, got error_code={:?}, summary={:?}",
        root.invocations[0].error_code, detail.execution_summary
    );
    assert!(
        layout
            .wiki_vault_dir()
            .join("sources/trading/breakaway-gap/breakaway-gap-fvg-transcript-synthesis.md")
            .exists(),
        "source file should be written to sources/trading/breakaway-gap/"
    );

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}
