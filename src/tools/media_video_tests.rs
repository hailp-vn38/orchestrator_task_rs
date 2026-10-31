use super::*;
use std::sync::Mutex;

struct MultiClipTestVideoBackend {
    constraints: Mutex<VideoProviderConstraints>,
    clip_durations: Vec<u32>,
    provider_message: Option<String>,
}

#[async_trait::async_trait]
impl VideoGeneratorBackend for MultiClipTestVideoBackend {
    fn video_constraints(&self) -> VideoProviderConstraints {
        self.constraints.lock().unwrap().clone()
    }

    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
    ) -> Result<GeneratedVideoPayload, String> {
        let (width, height) = req.effective_dimensions();
        let mut clips = Vec::new();
        for (idx, &dur) in self.clip_durations.iter().enumerate() {
            let bytes = encode_minimal_mp4(
                &format!("{} [scene {}]", req.prompt, idx + 1),
                width,
                height,
                dur,
            );
            clips.push(GeneratedVideoClip {
                filename: format!("part-{}.mp4", idx + 1),
                mime_type: "video/mp4".to_string(),
                bytes,
                width,
                height,
                duration_secs: dur,
            });
        }
        let first = clips.first().cloned().unwrap();
        Ok(GeneratedVideoPayload {
            filename: req.filename.clone(),
            mime_type: "video/mp4".to_string(),
            bytes: first.bytes,
            width,
            height,
            duration_secs: self.clip_durations.iter().sum(),
            provider: "pool:muse".to_string(),
            clips,
            provider_message: self.provider_message.clone(),
        })
    }
}

#[test]
fn video_create_descriptor_reflects_dynamic_provider_constraints() {
    let backend = Arc::new(MultiClipTestVideoBackend {
        constraints: Mutex::new(VideoProviderConstraints {
            provider_label: "Muse Web Gateway (model: muse-video)".to_string(),
            max_clip_duration_secs: 10,
            max_clips_per_call: 6,
            max_total_duration_secs: 60,
            provider_guidance: "Max 10s per clip, up to 6 clips per call; merge with video_concat."
                .to_string(),
        }),
        clip_durations: vec![10],
        provider_message: None,
    });
    let tool = VideoCreateTool::new(backend.clone());

    let d1 = tool.descriptor();
    assert!(d1
        .description
        .contains("Muse Web Gateway (model: muse-video)"));
    assert!(d1.description.contains("max_clip_duration_secs=10s"));
    assert!(d1.description.contains("max_clips_per_call=6"));
    assert_eq!(
        d1.input_schema["properties"]["duration_secs"]["maximum"],
        60
    );
    // Existing schema properties must be preserved
    assert!(d1.input_schema["properties"].get("width").is_some());
    assert!(d1.input_schema["properties"].get("height").is_some());
    assert!(d1.input_schema["properties"]
        .get("first_frame_artifact_id")
        .is_some());
    assert!(d1.input_schema["properties"]
        .get("first_frame_image")
        .is_some());
    assert!(d1.input_schema["properties"].get("role").is_some());
    assert_eq!(d1.input_schema["additionalProperties"], false);
    assert_eq!(d1.effect, ToolEffect::Mutating);

    // Switch active provider constraints dynamically
    *backend.constraints.lock().unwrap() = VideoProviderConstraints {
        provider_label: "OpenAI Video (model: sora-1)".to_string(),
        max_clip_duration_secs: 10,
        max_clips_per_call: 1,
        max_total_duration_secs: 10,
        provider_guidance: "Single 10s clip per call.".to_string(),
    };

    let d2 = tool.descriptor();
    assert!(d2.description.contains("OpenAI Video (model: sora-1)"));
    assert!(d2.description.contains("max_clips_per_call=1"));
    assert_eq!(
        d2.input_schema["properties"]["duration_secs"]["maximum"],
        10
    );
}

#[tokio::test]
async fn video_create_auto_merges_multiple_clips_into_primary_mp4_and_video_concat_merges_existing_mp4_artifacts(
) {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, PageRequest, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();

    let backend = Arc::new(MultiClipTestVideoBackend {
        constraints: Mutex::new(VideoProviderConstraints {
            provider_label: "Muse Web Gateway (model: muse-video)".to_string(),
            max_clip_duration_secs: 10,
            max_clips_per_call: 6,
            max_total_duration_secs: 60,
            provider_guidance: "Auto-merges 10s clips into a single primary MP4.".to_string(),
        }),
        clip_durations: vec![10, 10, 10, 10, 10, 10],
        provider_message: Some(
            "Tôi sẽ tạo bộ video 6 phân cảnh (60s):\n- Phân cảnh 1..6".to_string(),
        ),
    });

    let vid_tool = VideoCreateTool::new(backend)
        .with_output_dir(dir.path().join("videos"))
        .into_host_tool();
    let concat_tool = VideoConcatTool::with_default_backend()
        .with_output_dir(dir.path().join("videos"))
        .into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating 60s video via Muse",
        vec![(
            "call_vid_60s",
            "video_create",
            json!({
                "filename": "cang-thang-my-iran.mp4",
                "prompt": "6-scene geopolitical news overview",
                "duration_secs": 60
            }),
        )],
    );
    gw.push_text("Created merged 60s video from 6 clips.");

    let agent = AgentDefinition::new("media_agent", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Media creator")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["video_create", "video_concat"])
        .with_steps(6);

    let llm_cfg = LlmConfig {
        providers: vec![LlmProviderConfig {
            id: "test".to_string(),
            backend: LlmBackend::OpenAiCompatible,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            auth: ProviderAuth::ApiKey(SecretString::new("test-key")),
            request_timeout: Duration::from_secs(10),
        }],
    };

    let orch = Orchestrator::builder(OrchestratorConfig::new(
        llm_cfg,
        vec![agent],
        dir.path().join("orch_store"),
    ))
    .with_gateway(gw.clone())
    .register_tool(vid_tool)
    .register_tool(concat_tool)
    .build()
    .await
    .unwrap();

    let caller = CallerContext::new("owner1", "ws1");
    let session_id = orch.create_session(&caller, "media_agent").await.unwrap();
    let accepted1 = orch
        .submit(&caller, &session_id, "Create 60s video".to_string())
        .await
        .unwrap();

    let mut turn1_arts = Vec::new();
    for _ in 0..100 {
        let page = orch
            .list_artifacts(
                &caller,
                &session_id,
                &accepted1.turn_id,
                PageRequest::first(20),
            )
            .await
            .unwrap();
        if page.items.len() == 7
            && !orch
                .snapshot(&caller, &session_id)
                .await
                .unwrap()
                .has_active_work()
        {
            turn1_arts = page.items;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 6 intermediate clips + 1 auto-merged primary MP4 = 7 artifacts in Turn 1
    assert_eq!(turn1_arts.len(), 7);
    let intermediate_arts: Vec<_> = turn1_arts
        .iter()
        .filter(|a| a.role == ArtifactRole::Intermediate)
        .collect();
    assert_eq!(intermediate_arts.len(), 6);
    for (idx, art) in intermediate_arts.iter().enumerate() {
        assert_eq!(art.name, format!("cang-thang-my-iran-part-{}.mp4", idx + 1));
        let bytes = orch
            .open_artifact(&caller, &art.id)
            .await
            .unwrap()
            .read_all()
            .await
            .unwrap();
        assert_eq!(inspect_mp4_duration_secs(&bytes), Some(10));
    }

    let primary_arts: Vec<_> = turn1_arts
        .iter()
        .filter(|a| a.role == ArtifactRole::Primary)
        .collect();
    assert_eq!(primary_arts.len(), 1);
    let primary_art = primary_arts[0];
    assert_eq!(primary_art.name, "cang-thang-my-iran.mp4");
    let primary_bytes = orch
        .open_artifact(&caller, &primary_art.id)
        .await
        .unwrap()
        .read_all()
        .await
        .unwrap();
    assert_eq!(inspect_mp4_duration_secs(&primary_bytes), Some(60));

    // Verify `video_create` invocation metadata has `requires_concat == false` and `intermediate_artifact_ids`
    let snap = orch.snapshot(&caller, &session_id).await.unwrap();
    let vid_inv = snap
        .root
        .invocations
        .iter()
        .find(|i| i.tool_name == "video_create")
        .expect("video_create invocation persisted");
    let meta = &vid_inv.result.as_ref().expect("video_create result")["metadata"];
    assert_eq!(meta["artifact_id"], primary_art.id);
    assert_eq!(meta["artifact_ids"], json!([primary_art.id]));
    assert_eq!(meta["total_duration_secs"], 60);
    assert_eq!(meta["requires_concat"], false);
    assert_eq!(meta["clip_count"], 6);
    assert_eq!(
        meta["intermediate_artifact_ids"].as_array().unwrap().len(),
        6
    );
    assert_eq!(
        meta["provider_message"],
        "Tôi sẽ tạo bộ video 6 phân cảnh (60s):\n- Phân cảnh 1..6"
    );

    // Also verify standalone `video_concat` merges any 2 specified MP4 artifacts
    let two_clip_ids: Vec<String> = intermediate_arts
        .iter()
        .take(2)
        .map(|a| a.id.clone())
        .collect();
    gw.push_tool_calls(
        "Concatenating 2 existing clips into a 20s highlight",
        vec![(
            "call_concat_20s",
            "video_concat",
            json!({
                "filename": "highlight-20s.mp4",
                "video_artifact_ids": two_clip_ids,
                "title": "Highlight 20s"
            }),
        )],
    );
    gw.push_text("Merged 2 clips into 20s highlight.");

    let accepted2 = orch
        .submit(&caller, &session_id, "Concatenate 2 clips".to_string())
        .await
        .unwrap();

    let mut merged_arts = Vec::new();
    for _ in 0..100 {
        let page = orch
            .list_artifacts(
                &caller,
                &session_id,
                &accepted2.turn_id,
                PageRequest::first(20),
            )
            .await
            .unwrap();
        if !page.items.is_empty()
            && !orch
                .snapshot(&caller, &session_id)
                .await
                .unwrap()
                .has_active_work()
        {
            merged_arts = page.items;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(merged_arts.len(), 1);
    let final_art = &merged_arts[0];
    assert_eq!(final_art.role, ArtifactRole::Primary);
    assert_eq!(final_art.name, "highlight-20s.mp4");
    let final_bytes = orch
        .open_artifact(&caller, &final_art.id)
        .await
        .unwrap()
        .read_all()
        .await
        .unwrap();
    assert_eq!(inspect_mp4_duration_secs(&final_bytes), Some(20));

    orch.shutdown().await.unwrap();
}

struct FailingConcatBackend;

#[async_trait::async_trait]
impl VideoConcatBackend for FailingConcatBackend {
    async fn concat_videos(
        &self,
        _req: &VideoConcatRequest,
    ) -> Result<ConcatenatedVideoPayload, String> {
        Err("simulated ffmpeg concat failure".to_string())
    }
}

#[tokio::test]
async fn video_create_falls_back_to_requires_concat_when_concat_backend_fails() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, PageRequest, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();

    let backend = Arc::new(MultiClipTestVideoBackend {
        constraints: Mutex::new(VideoProviderConstraints {
            provider_label: "Muse Web Gateway (model: muse-video)".to_string(),
            max_clip_duration_secs: 10,
            max_clips_per_call: 2,
            max_total_duration_secs: 20,
            provider_guidance: "Auto-merges clips.".to_string(),
        }),
        clip_durations: vec![10, 10],
        provider_message: None,
    });

    let vid_tool = VideoCreateTool::new(backend)
        .with_concat_backend(Arc::new(FailingConcatBackend))
        .into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating 20s video with failing concat",
        vec![(
            "call_vid_20s",
            "video_create",
            json!({
                "filename": "fallback-test.mp4",
                "prompt": "2-scene test",
                "duration_secs": 20
            }),
        )],
    );
    gw.push_text("Created intermediate clips.");

    let agent = AgentDefinition::new("media_agent", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Media creator")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["video_create"])
        .with_steps(6);

    let llm_cfg = LlmConfig {
        providers: vec![LlmProviderConfig {
            id: "test".to_string(),
            backend: LlmBackend::OpenAiCompatible,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            auth: ProviderAuth::ApiKey(SecretString::new("test-key")),
            request_timeout: Duration::from_secs(10),
        }],
    };

    let orch = Orchestrator::builder(OrchestratorConfig::new(
        llm_cfg,
        vec![agent],
        dir.path().join("orch_store"),
    ))
    .with_gateway(gw)
    .register_tool(vid_tool)
    .build()
    .await
    .unwrap();

    let caller = CallerContext::new("owner1", "ws1");
    let session_id = orch.create_session(&caller, "media_agent").await.unwrap();
    let accepted = orch
        .submit(&caller, &session_id, "Create 20s video".to_string())
        .await
        .unwrap();

    let mut arts = Vec::new();
    for _ in 0..100 {
        let page = orch
            .list_artifacts(
                &caller,
                &session_id,
                &accepted.turn_id,
                PageRequest::first(20),
            )
            .await
            .unwrap();
        if page.items.len() == 2
            && !orch
                .snapshot(&caller, &session_id)
                .await
                .unwrap()
                .has_active_work()
        {
            arts = page.items;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(arts.len(), 2);
    assert!(arts.iter().all(|a| a.role == ArtifactRole::Intermediate));

    let snap = orch.snapshot(&caller, &session_id).await.unwrap();
    let vid_inv = snap
        .root
        .invocations
        .iter()
        .find(|i| i.tool_name == "video_create")
        .expect("video_create invocation persisted");
    let meta = &vid_inv.result.as_ref().expect("video_create result")["metadata"];
    assert_eq!(meta["requires_concat"], true);
    assert_eq!(meta["clip_count"], 2);
    assert_eq!(
        meta["intermediate_artifact_ids"].as_array().unwrap().len(),
        2
    );

    orch.shutdown().await.unwrap();
}

struct RecordingVideoBackend {
    seen_keys: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait::async_trait]
impl VideoGeneratorBackend for RecordingVideoBackend {
    fn video_constraints(&self) -> VideoProviderConstraints {
        VideoProviderConstraints::default()
    }

    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
    ) -> Result<GeneratedVideoPayload, String> {
        self.seen_keys
            .lock()
            .unwrap()
            .push(req.continuation_key.clone());
        let (width, height) = req.effective_dimensions();
        let bytes = encode_minimal_mp4(&req.prompt, width, height, 5);
        Ok(GeneratedVideoPayload {
            filename: req.filename.clone(),
            mime_type: "video/mp4".to_string(),
            bytes: bytes.clone(),
            width,
            height,
            duration_secs: 5,
            provider: "test".to_string(),
            clips: vec![GeneratedVideoClip {
                filename: req.filename.clone(),
                mime_type: "video/mp4".to_string(),
                bytes,
                width,
                height,
                duration_secs: 5,
            }],
            provider_message: None,
        })
    }
}

#[tokio::test]
async fn video_create_passes_the_job_root_session_id_as_continuation_key() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();

    let seen_keys = Arc::new(Mutex::new(Vec::new()));
    let vid_tool = VideoCreateTool::new(Arc::new(RecordingVideoBackend {
        seen_keys: Arc::clone(&seen_keys),
    }))
    .with_output_dir(dir.path().join("videos"))
    .into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating",
        vec![(
            "call_vid_1",
            "video_create",
            json!({ "filename": "a.mp4", "prompt": "scene", "duration_secs": 5 }),
        )],
    );
    gw.push_text("done");

    let agent = AgentDefinition::new("media_agent", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Media creator")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["video_create"])
        .with_steps(4);
    let llm_cfg = LlmConfig {
        providers: vec![LlmProviderConfig {
            id: "test".to_string(),
            backend: LlmBackend::OpenAiCompatible,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            auth: ProviderAuth::ApiKey(SecretString::new("test-key")),
            request_timeout: Duration::from_secs(10),
        }],
    };
    let orch = Orchestrator::builder(OrchestratorConfig::new(
        llm_cfg,
        vec![agent],
        dir.path().join("orch_store"),
    ))
    .with_gateway(gw.clone())
    .register_tool(vid_tool)
    .build()
    .await
    .unwrap();

    let caller = CallerContext::new("owner1", "ws1");
    let session_id = orch.create_session(&caller, "media_agent").await.unwrap();
    orch.submit(&caller, &session_id, "Make a video".to_string())
        .await
        .unwrap();
    for _ in 0..100 {
        if !seen_keys.lock().unwrap().is_empty()
            && !orch
                .snapshot(&caller, &session_id)
                .await
                .unwrap()
                .has_active_work()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        *seen_keys.lock().unwrap(),
        vec![Some(session_id.to_string())],
        "a root session's continuation key is its own session id"
    );
}
