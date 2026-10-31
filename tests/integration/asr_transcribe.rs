use super::*;

#[tokio::test]
async fn task_with_input_wav_artifact_and_asr_transcribe_tool_end_to_end() {
    use orchestrator_agent_rs::test_support::{ModelRequest, ModelResponse, ModelToolCall};
    use orchestrator_agent_rs::ArtifactInput;
    use orchestrator_flow_rs::{ErrorCode, VideoGeneratorBackend as _};

    fn build_test_pcm16_wav(sample_rate: u32, num_samples: u32) -> Vec<u8> {
        let data_len = num_samples * 2;
        let riff_len = 36 + data_len;
        let byte_rate = sample_rate * 2;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_len.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(44 + data_len as usize, 0u8);
        out
    }

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

    let wav_bytes = build_test_pcm16_wav(16_000, 16_000);
    let mp4_payload = orchestrator_flow_rs::DefaultVideoGeneratorBackend
        .generate_video(&orchestrator_flow_rs::VideoGenerationRequest {
            filename: "ban-ghi-hinh.mp4".to_string(),
            prompt: "Demo video with spoken narration".to_string(),
            title: Some("Demo Video".to_string()),
            width: 640,
            height: 360,
            duration_secs: 2,
            first_frame: None,
        })
        .await
        .unwrap();
    let mp4_bytes = mp4_payload.bytes;

    let gw = ScriptedGateway::new();
    // 1. Root agent inspects user prompt containing `Attached input artifacts:` and forwards both `art-...` IDs to `media_creator`
    gw.push_with(|req: &ModelRequest| {
        let user_text = req
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let art_ids: Vec<String> = user_text
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .filter(|tok| tok.starts_with("art-") && tok.len() > 8)
            .map(str::to_string)
            .collect();
        assert_eq!(
            art_ids.len(),
            2,
            "root prompt should contain both WAV and MP4 staged input artifact_ids: {user_text}"
        );
        ModelResponse {
            text: "Ủy quyền cho media_processor bóc băng file WAV và MP4.".to_string(),
            finish_reason: "tool_use".to_string(),
            tool_calls: vec![ModelToolCall {
                id: "call_sub_asr".to_string(),
                name: "subagent".to_string(),
                arguments: json!({
                    "agent": "media_processor",
                    "description": "Transcribe user WAV and MP4 and test invalid audio guard",
                    "prompt": format!(
                        "Please transcribe artifacts {} and {} and write a markdown summary.",
                        art_ids[0], art_ids[1]
                    )
                }),
            }],
            usage: None,
        }
    });
    // 2. Subagent `media_processor` extracts both `art_id`s from prompt, calls `asr_transcribe` on the WAV and MP4,
    //    and creates a non-media text artifact to verify `ASR_UNSUPPORTED_AUDIO`.
    gw.push_with(|req: &ModelRequest| {
        let sub_text = req
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let art_ids: Vec<String> = sub_text
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .filter(|tok| tok.starts_with("art-") && tok.len() > 8)
            .map(str::to_string)
            .collect();
        assert_eq!(
            art_ids.len(),
            2,
            "subagent prompt should contain both forwarded artifact_ids"
        );
        ModelResponse {
            text: "Đang gọi asr_transcribe cho file WAV và MP4.".to_string(),
            finish_reason: "tool_use".to_string(),
            tool_calls: vec![
                ModelToolCall {
                    id: "call_asr_wav_ok".to_string(),
                    name: "asr_transcribe".to_string(),
                    arguments: json!({
                        "artifact_id": art_ids[0],
                        "language": "vi-VN"
                    }),
                },
                ModelToolCall {
                    id: "call_asr_mp4_ok".to_string(),
                    name: "asr_transcribe".to_string(),
                    arguments: json!({
                        "artifact_id": art_ids[1],
                        "language": "vi-VN"
                    }),
                },
                ModelToolCall {
                    id: "call_txt_art".to_string(),
                    name: "artifact_create".to_string(),
                    arguments: json!({
                        "filename": "not-audio.txt",
                        "mime_type": "text/plain",
                        "content": "plain text payload",
                        "role": "intermediate"
                    }),
                },
            ],
            usage: None,
        }
    });
    // 3. Subagent `media_creator` tries `asr_transcribe` on the non-media artifact (expects `ASR_UNSUPPORTED_AUDIO`)
    //    and writes a markdown report with the transcripts.
    gw.push_with(|req: &ModelRequest| {
        let all_tool_texts = req
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_tool_texts.contains("[wav_transcript: 16000Hz, 1ch, 16bit, 1000ms, lang=vi-VN]"),
            "expected deterministic WAV transcript in tool result, got: {all_tool_texts}"
        );
        assert!(
            all_tool_texts.contains("00:00:00,000 --> 00:00:01,000"),
            "expected SRT timestamp block for 1000ms WAV in tool result, got: {all_tool_texts}"
        );
        assert!(
            all_tool_texts.contains("[wav_transcript: 16000Hz, 1ch, 16bit, 2000ms, lang=vi-VN]"),
            "expected deterministic MP4 extracted audio transcript in tool result, got: {all_tool_texts}"
        );
        assert!(
            all_tool_texts.contains("00:00:00,000 --> 00:00:02,000"),
            "expected SRT timestamp block for 2000ms MP4 in tool result, got: {all_tool_texts}"
        );
        // Find the second artifact_id (`not-audio.txt`) from `Published ... (id=art-..., ...)`
        let txt_art_id = req
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if m.text.contains("not-audio.txt") {
                    m.text
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                        .find(|tok| tok.starts_with("art-") && tok.len() > 8)
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .expect("not-audio.txt artifact_id");
        ModelResponse {
            text: "Kiểm tra lỗi định dạng và tạo báo cáo.".to_string(),
            finish_reason: "tool_use".to_string(),
            tool_calls: vec![
                ModelToolCall {
                    id: "call_asr_bad".to_string(),
                    name: "asr_transcribe".to_string(),
                    arguments: json!({
                        "artifact_id": txt_art_id
                    }),
                },
                ModelToolCall {
                    id: "call_md_out".to_string(),
                    name: "markdown_create".to_string(),
                    arguments: json!({
                        "filename": "meeting-transcript.md",
                        "title": "Biên bản cuộc họp",
                        "markdown_body": "Nội dung đã bóc băng từ file WAV đính kèm."
                    }),
                },
            ],
            usage: None,
        }
    });
    // 4. Subagent finishes
    gw.push_text("Đã hoàn tất bóc băng âm thanh và tạo biên bản cuộc họp.");
    // 5. Root agent finishes
    gw.push_text("Biên bản cuộc họp từ file ghi âm đã sẵn sàng.");
    // 6. Delayed response for the busy-session test below
    gw.push_delayed(
        Duration::from_millis(500),
        ModelResponse {
            text: "Busy turn completed.".to_string(),
            finish_reason: "stop".to_string(),
            tool_calls: Vec::new(),
            usage: None,
        },
    );

    let root_agent = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["subagent"])
        .with_allowed_subagents(["media_processor"])
        .with_steps(6);

    let media_subagent = AgentDefinition::new("media_processor", "test", "gpt-test")
        .with_mode(AgentMode::Subagent)
        .with_description("Audio/video processor & transcriber")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["asr_transcribe", "artifact_create", "markdown_create"])
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
        .create_task(
            CreateTaskRequest::new(
                "Bóc băng ghi âm cuộc họp",
                "Hãy gỡ băng file ghi âm đính kèm và viết biên bản.",
            )
            .with_input_artifacts(vec![
                ArtifactInput::intermediate_bytes("cuoc-hop.wav", "audio/wav", wav_bytes.clone()),
                ArtifactInput::intermediate_bytes("ban-ghi-hinh.mp4", "video/mp4", mp4_bytes),
            ]),
        )
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
    // Should include:
    // 1. Input WAV (`cuoc-hop.wav`, Audio, Intermediate)
    // 2. Input MP4 (`ban-ghi-hinh.mp4`, Video, Intermediate)
    // 3. `not-audio.txt` (Text, Intermediate)
    // 4. `meeting-transcript.md` (Report, Primary)
    assert_eq!(detail.artifacts.len(), 4);
    let input_wav = detail
        .artifacts
        .iter()
        .find(|a| a.name == "cuoc-hop.wav")
        .expect("input wav artifact recorded in task detail");
    assert_eq!(
        input_wav.kind,
        orchestrator_flow_rs::TaskArtifactKind::Audio
    );
    assert_eq!(
        input_wav.role,
        orchestrator_flow_rs::TaskArtifactRole::Intermediate
    );
    let input_mp4 = detail
        .artifacts
        .iter()
        .find(|a| a.name == "ban-ghi-hinh.mp4")
        .expect("input mp4 artifact recorded in task detail");
    assert_eq!(
        input_mp4.kind,
        orchestrator_flow_rs::TaskArtifactKind::Video
    );
    assert_eq!(
        input_mp4.role,
        orchestrator_flow_rs::TaskArtifactRole::Intermediate
    );

    let summary = detail.execution_summary.expect("execution_summary");
    assert!(summary
        .failed_error_codes
        .contains(&"ASR_UNSUPPORTED_AUDIO".to_string()));

    // Verify `SessionBusy` rejection when `existing_session_id` is currently busy and `input_artifacts` is non-empty
    let running_task = task_mgr
        .create_task(CreateTaskRequest::new(
            "Slow task holding session",
            "Run slow turn",
        ))
        .await
        .unwrap();

    let busy_err = task_mgr
        .create_task(
            CreateTaskRequest::new("Busy attachment task", "Transcribe while busy")
                .with_existing_session_id(running_task.session_id.clone())
                .with_input_artifacts(vec![ArtifactInput::intermediate_bytes(
                    "second.wav",
                    "audio/wav",
                    wav_bytes,
                )]),
        )
        .await
        .expect_err("must reject with SessionBusy when session is busy and has input_artifacts");
    assert_eq!(busy_err.code(), ErrorCode::SessionBusy);

    task_mgr.shutdown().await.unwrap();
    orch.shutdown().await.unwrap();
}
