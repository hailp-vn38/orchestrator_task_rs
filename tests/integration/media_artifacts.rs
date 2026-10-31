use super::*;

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
    // 1. Root agent delegates visual media to `media_creator`
    gw.push_tool_calls(
        "Delegating to media_creator subagent",
        vec![(
            "call_sub_media",
            "subagent",
            json!({
                "agent": "media_creator",
                "description": "Create PNG banner, SVG diagram, and MP4 video",
                "prompt": "Generate PNG, SVG, and MP4 visual artifacts for the B200 briefing."
            }),
        )],
    );
    // 2. Subagent `media_creator` calls 3 visual tools:
    //    - `image_create` PNG (Image)
    //    - `image_create` SVG (Image)
    //    - `video_create` MP4 (Video)
    gw.push_tool_calls(
        "Creating 3 visual artifacts",
        vec![
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
                "call_vid",
                "video_create",
                json!({
                    "filename": "b200-flythrough.mp4",
                    "title": "Blackwell Rack Flythrough",
                    "prompt": "Camera flying through liquid-cooled GB200 NVL72 rack",
                    "width": 640,
                    "height": 360,
                    "duration_secs": 5
                }),
            ),
        ],
    );
    // 3. Subagent `media_creator` finishes
    gw.push_text("Đã tạo 3 visual artifacts: ảnh PNG, sơ đồ SVG và video MP4.");
    // 4. Root agent delegates TTS, JSON data, and Markdown report to `media_processor`
    gw.push_tool_calls(
        "Delegating to media_processor subagent",
        vec![(
            "call_sub_proc",
            "subagent",
            json!({
                "agent": "media_processor",
                "description": "Create report, TTS audio, and JSON metrics",
                "prompt": "Generate Markdown report, TTS WAV narration, and JSON benchmarks."
            }),
        )],
    );
    // 5. Subagent `media_processor` calls 3 tools:
    //    - `markdown_create` (Report)
    //    - `tts_synthesize` WAV (Audio)
    //    - `artifact_create` JSON (Data, intermediate)
    gw.push_tool_calls(
        "Creating report, TTS audio, and JSON data artifacts",
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
    // 6. Subagent `media_processor` finishes
    gw.push_text("Đã tạo báo cáo Markdown, giọng đọc TTS WAV và dữ liệu JSON.");
    // 7. Root agent finishes
    gw.push_text("Hoàn tất gói đa phương tiện B200 gồm báo cáo, hình ảnh, video và âm thanh TTS.");

    let root_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["subagent"])
        .with_allowed_subagents(["media_creator", "media_processor"])
        .with_steps(6);

    let media_subagent = AgentDefinition::new("media_creator", "test", "gpt-test")
        .with_mode(AgentMode::Subagent)
        .with_description("Visual media artifact creator")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["image_create", "video_create"])
        .with_steps(8);

    let processor_subagent = AgentDefinition::new("media_processor", "test", "gpt-test")
        .with_mode(AgentMode::Subagent)
        .with_description("Audio/video processor & structured artifact publisher")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["markdown_create", "tts_synthesize", "artifact_create"])
        .with_steps(8);

    let mut builder = Orchestrator::builder(OrchestratorConfig::new(
        test_llm_config(),
        vec![root_agent, media_subagent, processor_subagent],
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
            "Tạo báo cáo, ảnh minh họa, video và audio TTS cho B200.",
        ))
        .await
        .unwrap();

    let mut terminal_reached = false;
    for _ in 0..120 {
        if let Ok(Some(TaskFeedItem::Event(TaskEvent::TaskTerminal { task: t }))) =
            tokio::time::timeout(Duration::from_secs(3), sub.recv()).await
        {
            if t.id == task.id {
                assert_eq!(t.status, TaskStatus::Completed);
                terminal_reached = true;
                break;
            }
        }
    }
    assert!(terminal_reached, "task should reach TaskTerminal");

    let detail = task_mgr.get_task_detail(&task.id).await.unwrap();
    assert_eq!(
        detail.artifacts.len(),
        6,
        "should have 6 artifacts recorded"
    );

    let summary = detail.execution_summary.expect("execution_summary");
    assert_eq!(summary.total_artifacts, 6);
    assert_eq!(summary.primary_artifacts, 5);
    assert_eq!(summary.report_artifacts, 1);
    assert_eq!(summary.image_artifacts, 2);
    assert_eq!(summary.video_artifacts, 1);
    assert_eq!(summary.audio_artifacts, 1);
    assert_eq!(summary.artifact_ids.len(), 6);

    // Kiểm tra từng loại artifact và đọc lại kèm xác minh SHA-256
    let md_art = detail
        .artifacts
        .iter()
        .find(|a| a.name == "b200-briefing.md")
        .expect("markdown artifact");
    assert_eq!(md_art.kind, orchestrator_flow_rs::TaskArtifactKind::Report);
    assert_eq!(md_art.role, orchestrator_flow_rs::TaskArtifactRole::Primary);
    assert_eq!(md_art.producer_agent_id, "media_processor");
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

    let vid_art = detail
        .artifacts
        .iter()
        .find(|a| a.name == "b200-flythrough.mp4")
        .expect("video artifact");
    assert_eq!(vid_art.kind, orchestrator_flow_rs::TaskArtifactKind::Video);
    assert_eq!(vid_art.mime_type, "video/mp4");
    assert_eq!(vid_art.producer_tool_name.as_deref(), Some("video_create"));
    let (_, vid_bytes) = task_mgr
        .read_task_artifact_bytes(&task.id, &vid_art.id)
        .await
        .unwrap();
    assert_eq!(&vid_bytes[4..8], b"ftyp");

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

    // Kiểm tra Artifact Catalog (`list_all_artifacts` & `list_artifacts`) với bộ lọc và summary
    let catalog_all = task_mgr
        .list_all_artifacts(&orchestrator_flow_rs::TaskArtifactFilter::default())
        .await
        .unwrap();
    assert_eq!(catalog_all.total, 6);
    assert_eq!(catalog_all.items.len(), 6);
    assert_eq!(catalog_all.summary.total_artifacts, 6);
    assert_eq!(catalog_all.summary.primary_artifacts, 5);
    assert_eq!(catalog_all.summary.intermediate_artifacts, 1);
    assert_eq!(catalog_all.summary.report_artifacts, 1);
    assert_eq!(catalog_all.summary.image_artifacts, 2);
    assert_eq!(catalog_all.summary.video_artifacts, 1);
    assert_eq!(catalog_all.summary.audio_artifacts, 1);
    assert_eq!(catalog_all.summary.data_artifacts, 1);
    assert!(catalog_all.summary.total_size_bytes > 0);
    assert_eq!(catalog_all.items[0].task_title, "B200 Multi-Modal Briefing");
    assert_eq!(catalog_all.items[0].artifact.task_id, task.id);

    // Lọc theo kind = Image: `total` chỉ tính 2 ảnh, nhưng `summary` vẫn giữ nguyên phân bố toàn bộ các tab
    let catalog_images = task_mgr
        .list_all_artifacts(&orchestrator_flow_rs::TaskArtifactFilter {
            kind: Some(orchestrator_flow_rs::TaskArtifactKind::Image),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(catalog_images.total, 2);
    assert_eq!(catalog_images.items.len(), 2);
    assert_eq!(catalog_images.summary.total_artifacts, 6);
    assert_eq!(catalog_images.summary.image_artifacts, 2);

    // Lọc theo role = Intermediate + query = "benchmarks"
    let catalog_intermediate = task_mgr
        .list_artifacts(&orchestrator_flow_rs::TaskArtifactFilter {
            role: Some(orchestrator_flow_rs::TaskArtifactRole::Intermediate),
            query: Some("benchmarks".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(catalog_intermediate.total, 1);
    assert_eq!(
        catalog_intermediate.items[0].artifact.name,
        "b200-benchmarks.json"
    );

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}
