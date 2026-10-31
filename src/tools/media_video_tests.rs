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
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
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
    assert_eq!(d1.effect, ToolEffect::Resumable);

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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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

#[tokio::test]
async fn video_create_rejects_duration_above_provider_limit_instead_of_clamping() {
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

    // The recording backend advertises the default 60s ceiling.
    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating",
        vec![(
            "call_vid_90",
            "video_create",
            json!({ "filename": "long.mp4", "prompt": "scene", "duration_secs": 90 }),
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
    orch.submit(&caller, &session_id, "Make a 90s video".to_string())
        .await
        .unwrap();
    let mut failed_text = None;
    for _ in 0..100 {
        let snap = orch.snapshot(&caller, &session_id).await.unwrap();
        let result = snap
            .root
            .invocations
            .iter()
            .find(|i| i.tool_name == "video_create")
            .and_then(|inv| inv.result.as_ref());
        if let Some(result) = result {
            if !snap.has_active_work() {
                failed_text = Some(result.to_string());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        seen_keys.lock().unwrap().is_empty(),
        "the backend must not be asked for a shortened video"
    );
    let text = failed_text.expect("video_create produced a result");
    assert!(
        text.contains("INVALID_ARGUMENT") && text.contains("max_total_duration_secs=60"),
        "error names the limit so the agent can split the video: {text}"
    );
}

struct HangingVideoBackend {
    started: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl VideoGeneratorBackend for HangingVideoBackend {
    fn video_constraints(&self) -> VideoProviderConstraints {
        VideoProviderConstraints::default()
    }

    async fn generate_video(
        &self,
        _req: &VideoGenerationRequest,
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("the hanging backend never completes")
    }
}

#[tokio::test]
async fn video_create_stops_waiting_for_the_provider_when_the_job_is_cancelled() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let vid_tool = VideoCreateTool::new(Arc::new(HangingVideoBackend {
        started: Arc::clone(&started),
    }))
    .with_output_dir(dir.path().join("videos"))
    .into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating",
        vec![(
            "call_vid_hang",
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
        if started.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(started.load(Ordering::SeqCst), "provider call started");

    orch.cancel(&caller, &session_id).await.unwrap();

    let mut idle = false;
    for _ in 0..150 {
        if !orch
            .snapshot(&caller, &session_id)
            .await
            .unwrap()
            .has_active_work()
        {
            idle = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        idle,
        "a cancelled job must not keep waiting on the provider"
    );
}

/// Runs one `video_create` call (the recording backend always delivers a 5s clip) and returns the
/// text the model receives as the tool result.
async fn video_create_model_text(requested_secs: u32) -> String {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let vid_tool = VideoCreateTool::new(Arc::new(RecordingVideoBackend {
        seen_keys: Arc::new(Mutex::new(Vec::new())),
    }))
    .with_output_dir(dir.path().join("videos"))
    .into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating",
        vec![(
            "call_vid",
            "video_create",
            json!({ "filename": "a.mp4", "prompt": "scene", "duration_secs": requested_secs }),
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
        let snap = orch.snapshot(&caller, &session_id).await.unwrap();
        let result = snap
            .root
            .invocations
            .iter()
            .find(|i| i.tool_name == "video_create")
            .and_then(|inv| inv.result.as_ref());
        if let Some(result) = result {
            if !snap.has_active_work() {
                return result["content"].as_str().unwrap_or_default().to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("video_create did not settle");
}

#[tokio::test]
async fn video_create_text_tells_the_agent_when_the_video_is_short() {
    let text = video_create_model_text(30).await;
    assert!(
        text.contains("requested=30s actual=5s complete=false next_action=call_again_same_job"),
        "short video must say so in the text the model reads: {text}"
    );
}

#[tokio::test]
async fn video_create_text_reports_a_full_length_video_as_complete() {
    let text = video_create_model_text(5).await;
    assert!(
        text.contains("requested=5s actual=5s complete=true next_action=none"),
        "{text}"
    );
}

struct UnmeasurableVideoBackend;

#[async_trait::async_trait]
impl VideoGeneratorBackend for UnmeasurableVideoBackend {
    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        let (width, height) = req.effective_dimensions();
        // Bytes without an mvhd atom so inspect_mp4_duration_secs returns None.
        let raw_bytes = b"\x00\x00\x00\x14ftypisom\x00\x00\x02\x00isom".to_vec();
        Ok(GeneratedVideoPayload {
            filename: req.filename.clone(),
            mime_type: "video/mp4".to_string(),
            bytes: raw_bytes.clone(),
            width,
            height,
            duration_secs: 0,
            provider: "test".to_string(),
            clips: vec![GeneratedVideoClip {
                filename: req.filename.clone(),
                mime_type: "video/mp4".to_string(),
                bytes: raw_bytes,
                width,
                height,
                duration_secs: 0,
            }],
            provider_message: None,
        })
    }
}

#[tokio::test]
async fn unmeasurable_video_not_complete() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let vid_tool = VideoCreateTool::new(Arc::new(UnmeasurableVideoBackend))
        .with_output_dir(dir.path().join("videos"))
        .into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating",
        vec![(
            "call_vid",
            "video_create",
            json!({ "filename": "a.mp4", "prompt": "scene", "duration_secs": 10 }),
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
    let mut text = None;
    for _ in 0..100 {
        let snap = orch.snapshot(&caller, &session_id).await.unwrap();
        let result = snap
            .root
            .invocations
            .iter()
            .find(|i| i.tool_name == "video_create")
            .and_then(|inv| inv.result.as_ref());
        if let Some(result) = result {
            if !snap.has_active_work() {
                assert_eq!(result["metadata"]["complete"], "unknown");
                text = Some(result["content"].as_str().unwrap_or_default().to_string());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let text = text.expect("video_create settled");
    assert!(
        text.contains("complete=unknown"),
        "unmeasurable MP4 duration must report complete=unknown: {text}"
    );
    orch.shutdown().await.unwrap();
}

struct TimeoutVideoBackend;

#[async_trait::async_trait]
impl VideoGeneratorBackend for TimeoutVideoBackend {
    async fn generate_video(
        &self,
        _req: &VideoGenerationRequest,
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        Err(MediaBackendError::new(
            "VIDEO_GENERATION_TIMEOUT",
            "the video provider did not finish in time",
            true,
        ))
    }
}

#[tokio::test]
async fn video_timeout_code() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let vid_tool = VideoCreateTool::new(Arc::new(TimeoutVideoBackend)).into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Generating",
        vec![(
            "call_vid_to",
            "video_create",
            json!({ "filename": "a.mp4", "prompt": "scene", "duration_secs": 10 }),
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
    let mut failed_text = None;
    for _ in 0..100 {
        let snap = orch.snapshot(&caller, &session_id).await.unwrap();
        let result = snap
            .root
            .invocations
            .iter()
            .find(|i| i.tool_name == "video_create")
            .and_then(|inv| inv.result.as_ref());
        if let Some(result) = result {
            if !snap.has_active_work() {
                failed_text = Some(result.to_string());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let text = failed_text.expect("video_create produced a result");
    assert!(
        text.contains("VIDEO_GENERATION_TIMEOUT"),
        "video_create must propagate VIDEO_GENERATION_TIMEOUT in ToolError: {text}"
    );
    orch.shutdown().await.unwrap();
}

struct DurablePhase4Backend {
    submit_calls: Arc<std::sync::atomic::AtomicUsize>,
    resume_calls: Arc<std::sync::atomic::AtomicUsize>,
    download_calls: Arc<std::sync::atomic::AtomicUsize>,
    reconcile_mode: SubmittingReconciliation,
    seen_resume_batch_index: Arc<Mutex<Vec<u32>>>,
    seen_resume_batch_paths: Arc<Mutex<Vec<Option<String>>>>,
    seen_submit_remote_session_ids: Arc<Mutex<Vec<Option<String>>>>,
}

impl DurablePhase4Backend {
    fn new(reconcile_mode: SubmittingReconciliation) -> Self {
        Self {
            submit_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            resume_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            download_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            reconcile_mode,
            seen_resume_batch_index: Arc::new(Mutex::new(Vec::new())),
            seen_resume_batch_paths: Arc::new(Mutex::new(Vec::new())),
            seen_submit_remote_session_ids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn make_payload(req: &VideoGenerationRequest, dur: u32) -> GeneratedVideoPayload {
        let (width, height) = req.effective_dimensions();
        let bytes = encode_minimal_mp4(&req.prompt, width, height, dur);
        GeneratedVideoPayload {
            filename: req.filename.clone(),
            mime_type: "video/mp4".to_string(),
            bytes: bytes.clone(),
            width,
            height,
            duration_secs: dur,
            provider: "muse".to_string(),
            clips: vec![GeneratedVideoClip {
                filename: req.filename.clone(),
                mime_type: "video/mp4".to_string(),
                bytes,
                width,
                height,
                duration_secs: dur,
            }],
            provider_message: None,
        }
    }
}

#[async_trait::async_trait]
impl VideoGeneratorBackend for DurablePhase4Backend {
    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
        ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        self.submit_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.seen_submit_remote_session_ids.lock().unwrap().push(
            ctx.operation
                .as_ref()
                .and_then(|op| op.remote_session_id.clone()),
        );
        Ok(Self::make_payload(req, req.duration_secs.max(5)))
    }

    async fn reconcile_submitting(
        &self,
        _req: &VideoGenerationRequest,
        _op: &orchestrator_agent_rs::ToolOperation,
        _ctx: &MediaCallContext,
    ) -> SubmittingReconciliation {
        self.reconcile_mode
    }

    async fn resume_video(
        &self,
        req: &VideoGenerationRequest,
        op: &orchestrator_agent_rs::ToolOperation,
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        self.resume_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.seen_resume_batch_index
            .lock()
            .unwrap()
            .push(op.batch_index);
        self.seen_resume_batch_paths
            .lock()
            .unwrap()
            .push(op.batch_paths_json.clone());
        Ok(Self::make_payload(req, req.duration_secs.max(5)))
    }

    async fn download_video(
        &self,
        req: &VideoGenerationRequest,
        _op: &orchestrator_agent_rs::ToolOperation,
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        self.download_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Self::make_payload(req, req.duration_secs.max(5)))
    }
}

async fn seed_crashed_video_invocation(
    store_dir: &std::path::Path,
    prompt_path: &std::path::Path,
    vid_tool: Arc<dyn HostTool>,
    op_status: &str,
    user_turns_before: Option<i64>,
    remote_session_id: Option<&str>,
    batch_index: i64,
    batch_paths_json: Option<&str>,
    result_json: Option<&str>,
) -> String {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use sqlx::{Connection, SqliteConnection};
    use std::time::Duration;

    let gw = ScriptedGateway::new();
    let agent = AgentDefinition::new("media_agent", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Media creator")
        .with_prompt_path(prompt_path)
        .with_allowed_tools(["video_create"])
        .with_steps(4);
    let llm_cfg = LlmConfig {
        providers: vec![LlmProviderConfig {
            id: "test".to_string(),
            backend: LlmBackend::OpenAiCompatible,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            auth: ProviderAuth::ApiKey(SecretString::new("test-key")),
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
        }],
    };
    let orch = Orchestrator::builder(OrchestratorConfig::new(llm_cfg, vec![agent], store_dir))
        .with_gateway(gw.clone())
        .register_tool(vid_tool)
        .build()
        .await
        .unwrap();
    let caller = CallerContext::new("owner1", "ws1");
    let session_id = orch.create_session(&caller, "media_agent").await.unwrap();
    gw.push_text("bootstrap");
    let boot = orch
        .submit(&caller, &session_id, "bootstrap".to_string())
        .await
        .unwrap();
    for _ in 0..100 {
        let r = orch
            .result(&caller, &session_id, &boot.turn_id)
            .await
            .unwrap();
        if r.status.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    orch.shutdown().await.unwrap();

    let db_path = store_dir.join("orchestrator.db");
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .unwrap();
    let fp: String = sqlx::query_scalar(
        "SELECT provider_config_fingerprint FROM agent_runs WHERE session_id = ?1 LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    sqlx::query("UPDATE agent_runs SET final_message_id = NULL WHERE session_id = ?1")
        .bind(&session_id)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM messages WHERE session_id = ?1")
        .bind(&session_id)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM llm_attempts WHERE run_id IN (SELECT id FROM agent_runs WHERE session_id = ?1)",
    )
    .bind(&session_id)
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query("DELETE FROM agent_runs WHERE session_id = ?1")
        .bind(&session_id)
        .execute(&mut conn)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO messages(id, session_id, sequence, role, content_json) \
         VALUES('msg-u1', ?1, 1, 'user', '{\"text\":\"make video\"}')",
    )
    .bind(&session_id)
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_runs(id, session_id, agent_id, turn_id, resume_attempt, \
         provider_id, model_id, provider_config_fingerprint, status, steps) \
         VALUES('run-crash-0', ?1, 'media_agent', 'turn-crash', 0, 'test', 'gpt-test', ?2, 'running', 1)",
    )
    .bind(&session_id)
    .bind(&fp)
    .execute(&mut conn)
    .await
    .unwrap();
    let args_json = json!({
        "filename": "durable.mp4",
        "prompt": "durable test scene",
        "duration_secs": 10
    })
    .to_string();
    let msg_a_json = json!({
        "text": "",
        "tool_calls": [{
            "id": "call-vid-1",
            "name": "video_create",
            "arguments": {
                "filename": "durable.mp4",
                "prompt": "durable test scene",
                "duration_secs": 10
            }
        }]
    })
    .to_string();
    sqlx::query(
        "INSERT INTO messages(id, session_id, sequence, role, content_json, run_id) \
         VALUES('msg-a1', ?1, 2, 'assistant', ?2, 'run-crash-0')",
    )
    .bind(&session_id)
    .bind(&msg_a_json)
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tool_invocations(id, session_id, run_id, tool_call_id, assistant_message_id, \
         tool_name, arguments_json, status, started_at) \
         VALUES('inv-vid-1', ?1, 'run-crash-0', 'call-vid-1', 'msg-a1', 'video_create', ?2, 'running', unixepoch())",
    )
    .bind(&session_id)
    .bind(&args_json)
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query("UPDATE sessions SET next_sequence = 3 WHERE id = ?1")
        .bind(&session_id)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tool_operations(id, invocation_id, provider_ref, remote_session_id, \
         continuation_key, batch_index, batch_paths_json, user_turns_before, status, result_json, deadline_at) \
         VALUES('op-vid-1', 'inv-vid-1', 'muse', ?1, ?2, ?3, ?4, ?5, ?6, ?7, 9999999999)",
    )
    .bind(remote_session_id)
    .bind(&session_id)
    .bind(batch_index)
    .bind(batch_paths_json)
    .bind(user_turns_before)
    .bind(op_status)
    .bind(result_json)
    .execute(&mut conn)
    .await
    .unwrap();

    session_id
}

async fn reopen_and_wait_turn(
    store_dir: &std::path::Path,
    prompt_path: &std::path::Path,
    vid_tool: Arc<dyn HostTool>,
    session_id: &str,
) -> (
    orchestrator_agent_rs::Orchestrator,
    orchestrator_agent_rs::AgentResult,
    Vec<orchestrator_agent_rs::ArtifactRef>,
) {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, PageRequest, ProviderAuth, RunStatus, SecretString,
    };
    use std::time::Duration;

    let gw = ScriptedGateway::new();
    gw.push_text("recovered and completed");
    let agent = AgentDefinition::new("media_agent", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Media creator")
        .with_prompt_path(prompt_path)
        .with_allowed_tools(["video_create"])
        .with_steps(4);
    let llm_cfg = LlmConfig {
        providers: vec![LlmProviderConfig {
            id: "test".to_string(),
            backend: LlmBackend::OpenAiCompatible,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            auth: ProviderAuth::ApiKey(SecretString::new("test-key")),
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
        }],
    };
    let orch = Orchestrator::builder(OrchestratorConfig::new(llm_cfg, vec![agent], store_dir))
        .with_gateway(gw)
        .register_tool(vid_tool)
        .build()
        .await
        .unwrap();
    let caller = CallerContext::new("owner1", "ws1");
    let mut final_res = None;
    for _ in 0..100 {
        let r = orch
            .result(&caller, session_id, "turn-crash")
            .await
            .unwrap();
        if r.status != RunStatus::Running {
            final_res = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let res = final_res.expect("turn-crash settled");
    let arts = orch
        .list_artifacts(&caller, session_id, "turn-crash", PageRequest::first(20))
        .await
        .unwrap()
        .items;
    (orch, res, arts)
}

#[tokio::test]
async fn kill_while_running() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let store_dir = dir.path().join("orch_store");

    let backend = Arc::new(DurablePhase4Backend::new(
        SubmittingReconciliation::Unreadable,
    ));
    let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
    let session_id = seed_crashed_video_invocation(
        &store_dir,
        &prompt_path,
        vid_tool.clone(),
        "running",
        Some(0),
        Some("sess-run-1"),
        0,
        None,
        None,
    )
    .await;

    let (orch, res, arts) =
        reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
    assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
    assert_eq!(
        backend.submit_calls.load(Ordering::SeqCst),
        0,
        "must not submit a second time when op is already running"
    );
    assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 1);
    assert_eq!(arts.len(), 1);
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn kill_while_downloading() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let store_dir = dir.path().join("orch_store");

    let backend = Arc::new(DurablePhase4Backend::new(
        SubmittingReconciliation::Unreadable,
    ));
    let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
    let session_id = seed_crashed_video_invocation(
        &store_dir,
        &prompt_path,
        vid_tool.clone(),
        "downloading",
        Some(0),
        Some("sess-dl-1"),
        0,
        None,
        None,
    )
    .await;

    let (orch, res, arts) =
        reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
    assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
    assert_eq!(backend.submit_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.download_calls.load(Ordering::SeqCst), 1);
    assert_eq!(arts.len(), 1, "must produce exactly one artifact");
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn kill_while_submitting_sent() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let store_dir = dir.path().join("orch_store");

    // Row 1: remote user turns (1) > user_turns_before (0) -> transitions to running and polls
    let backend = Arc::new(DurablePhase4Backend::new(
        SubmittingReconciliation::RemoteUserTurns(1),
    ));
    let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
    let session_id = seed_crashed_video_invocation(
        &store_dir,
        &prompt_path,
        vid_tool.clone(),
        "submitting",
        Some(0),
        Some("sess-sub-sent"),
        0,
        None,
        None,
    )
    .await;

    let (orch, res, arts) =
        reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
    assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
    assert_eq!(
        backend.submit_calls.load(Ordering::SeqCst),
        0,
        "already sent turn must transition to running without re-submitting"
    );
    assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 1);
    assert_eq!(arts.len(), 1);
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn kill_while_submitting_not_sent() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let store_dir = dir.path().join("orch_store");

    // Row 2: remote user turns (0) == user_turns_before (0) -> re-submit with the same session
    let backend = Arc::new(DurablePhase4Backend::new(
        SubmittingReconciliation::RemoteUserTurns(0),
    ));
    let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
    let session_id = seed_crashed_video_invocation(
        &store_dir,
        &prompt_path,
        vid_tool.clone(),
        "submitting",
        Some(0),
        Some("sess-sub-unsent"),
        0,
        None,
        None,
    )
    .await;

    let (orch, res, arts) =
        reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
    assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
    assert_eq!(backend.submit_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        backend
            .seen_submit_remote_session_ids
            .lock()
            .unwrap()
            .as_slice(),
        &[Some("sess-sub-unsent".to_string())]
    );
    assert_eq!(arts.len(), 1);
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn kill_while_submitting_unreadable() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let store_dir = dir.path().join("orch_store");

    // Row 3: unreadable history -> returns TOOL_OUTCOME_UNKNOWN -> run stays Interrupted
    let backend = Arc::new(DurablePhase4Backend::new(
        SubmittingReconciliation::Unreadable,
    ));
    let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
    let session_id = seed_crashed_video_invocation(
        &store_dir,
        &prompt_path,
        vid_tool.clone(),
        "submitting",
        Some(0),
        Some("sess-sub-unreadable"),
        0,
        None,
        None,
    )
    .await;

    let (orch, res, arts) =
        reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
    assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Interrupted);
    assert_eq!(
        res.error.as_ref().map(|e| e.code.as_str()),
        Some("TOOL_OUTCOME_UNKNOWN")
    );
    assert_eq!(backend.submit_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 0);
    assert!(arts.is_empty());
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn kill_after_committed() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let store_dir = dir.path().join("orch_store");

    let backend = Arc::new(DurablePhase4Backend::new(
        SubmittingReconciliation::Unreadable,
    ));
    let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();

    // First run a normal turn to get a real committed artifact in the store, or seed it:
    let saved_result = json!({
        "content": "Created video artifact 'durable.mp4'\nrequested=10s actual=10s complete=true next_action=none",
        "metadata": {
            "complete": true,
            "next_action": "none",
            "artifact_kind": "video"
        }
    })
    .to_string();
    let session_id = seed_crashed_video_invocation(
        &store_dir,
        &prompt_path,
        vid_tool.clone(),
        "committed",
        Some(0),
        Some("sess-committed"),
        0,
        None,
        Some(&saved_result),
    )
    .await;

    let (orch, res, arts) =
        reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
    assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
    assert_eq!(backend.submit_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.download_calls.load(Ordering::SeqCst), 0);
    assert!(
        arts.is_empty(),
        "must not re-download or create duplicate artifacts after committed"
    );
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn kill_between_batches() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();
    let store_dir = dir.path().join("orch_store");

    let backend = Arc::new(DurablePhase4Backend::new(
        SubmittingReconciliation::Unreadable,
    ));
    let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
    let batch_paths = json!(["/tmp/muse-batch-0.mp4"]).to_string();
    let session_id = seed_crashed_video_invocation(
        &store_dir,
        &prompt_path,
        vid_tool.clone(),
        "running",
        Some(0),
        Some("sess-batches"),
        1,
        Some(&batch_paths),
        None,
    )
    .await;

    let (orch, res, arts) =
        reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
    assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
    assert_eq!(backend.submit_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*backend.seen_resume_batch_index.lock().unwrap(), vec![1]);
    assert_eq!(
        *backend.seen_resume_batch_paths.lock().unwrap(),
        vec![Some(batch_paths)]
    );
    assert_eq!(arts.len(), 1);
    orch.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconcile_with_prior_turns() {
    use std::sync::atomic::Ordering;
    // Session already had 2 prior user turns (`user_turns_before = Some(2)`).
    // 1) When history still has 2 user turns -> not sent -> re-submits with same session.
    {
        let dir = tempfile::tempdir().unwrap();
        let prompt_path = dir.path().join("prompt.md");
        std::fs::write(&prompt_path, "You are a media creator.").unwrap();
        let store_dir = dir.path().join("orch_store");

        let backend = Arc::new(DurablePhase4Backend::new(
            SubmittingReconciliation::RemoteUserTurns(2),
        ));
        let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
        let session_id = seed_crashed_video_invocation(
            &store_dir,
            &prompt_path,
            vid_tool.clone(),
            "submitting",
            Some(2),
            Some("sess-prior-2"),
            0,
            None,
            None,
        )
        .await;

        let (orch, res, _arts) =
            reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
        assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
        assert_eq!(backend.submit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 0);
        orch.shutdown().await.unwrap();
    }

    // 2) When history has 3 user turns (> 2) -> sent -> transitions to running and resumes.
    {
        let dir = tempfile::tempdir().unwrap();
        let prompt_path = dir.path().join("prompt.md");
        std::fs::write(&prompt_path, "You are a media creator.").unwrap();
        let store_dir = dir.path().join("orch_store");

        let backend = Arc::new(DurablePhase4Backend::new(
            SubmittingReconciliation::RemoteUserTurns(3),
        ));
        let vid_tool = VideoCreateTool::new(backend.clone()).into_host_tool();
        let session_id = seed_crashed_video_invocation(
            &store_dir,
            &prompt_path,
            vid_tool.clone(),
            "submitting",
            Some(2),
            Some("sess-prior-3"),
            0,
            None,
            None,
        )
        .await;

        let (orch, res, _arts) =
            reopen_and_wait_turn(&store_dir, &prompt_path, vid_tool, &session_id).await;
        assert_eq!(res.status, orchestrator_agent_rs::RunStatus::Completed);
        assert_eq!(backend.submit_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.resume_calls.load(Ordering::SeqCst), 1);
        orch.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn planned_shutdown_resumes_video_create_without_cancelling_operation() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use std::time::Duration;
    struct CooperativeVideo {
        started: tokio::sync::Notify,
        resumed: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl VideoGeneratorBackend for CooperativeVideo {
        async fn generate_video(
            &self,
            _: &VideoGenerationRequest,
            ctx: &MediaCallContext,
        ) -> Result<GeneratedVideoPayload, MediaBackendError> {
            ctx.journal
                .as_ref()
                .unwrap()
                .advance(orchestrator_agent_rs::OperationUpdate {
                    status: Some(orchestrator_agent_rs::ToolOperationStatus::Running),
                    remote_session_id: Some("render-during-shutdown".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            self.started.notify_one();
            ctx.cancel.token().cancelled().await;
            Err(MediaBackendError::new(
                "CANCELLED",
                "planned shutdown",
                false,
            ))
        }
        async fn resume_video(
            &self,
            req: &VideoGenerationRequest,
            op: &orchestrator_agent_rs::ToolOperation,
            _: &MediaCallContext,
        ) -> Result<GeneratedVideoPayload, MediaBackendError> {
            assert_eq!(
                op.status,
                orchestrator_agent_rs::ToolOperationStatus::Running
            );
            self.resumed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(DurablePhase4Backend::make_payload(req, 5))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let prompt = dir.path().join("media.md");
    std::fs::write(&prompt, "Create videos").unwrap();
    let backend = Arc::new(CooperativeVideo {
        started: tokio::sync::Notify::new(),
        resumed: std::sync::atomic::AtomicUsize::new(0),
    });
    let tool = Arc::new(VideoCreateTool::new(backend.clone()));
    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "",
        vec![(
            "v1",
            "video_create",
            json!({"filename":"movie.mp4", "prompt":"story", "duration_secs":5}),
        )],
    );
    gw.push_text("video complete");
    let config = || {
        let agent = AgentDefinition::new("media_agent", "test", "model")
            .with_mode(AgentMode::Primary)
            .with_prompt_path(&prompt)
            .with_allowed_tools(["video_create"]);
        let llm = LlmConfig {
            providers: vec![LlmProviderConfig {
                id: "test".into(),
                backend: LlmBackend::OpenAiCompatible,
                endpoint: Some("http://127.0.0.1:9".into()),
                auth: ProviderAuth::ApiKey(SecretString::new("test")),
                limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
            }],
        };
        let mut config = OrchestratorConfig::new(llm, vec![agent], dir.path().join("store"));
        config.runtime.shutdown_drain = Duration::from_millis(10);
        config.runtime.shutdown_grace = Duration::from_secs(1);
        config
    };
    let o = Orchestrator::builder(config())
        .with_gateway(gw.clone())
        .register_tool(tool.clone())
        .build()
        .await
        .unwrap();
    let caller = CallerContext::new("owner", "workspace");
    let sid = o.create_session(&caller, "media_agent").await.unwrap();
    let turn = o.submit(&caller, &sid, "render".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), backend.started.notified())
        .await
        .unwrap();
    o.shutdown().await.unwrap();
    let restored = Orchestrator::builder(config())
        .with_gateway(gw)
        .register_tool(tool)
        .build()
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let r = restored.result(&caller, &sid, &turn.turn_id).await.unwrap();
            if r.status.is_terminal() {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        result.status,
        orchestrator_agent_rs::RunStatus::Completed,
        "{result:?}"
    );
    assert_eq!(backend.resumed.load(std::sync::atomic::Ordering::SeqCst), 1);
    let ops = restored.list_tool_operations().await.unwrap();
    assert_eq!(
        ops[0].operation.status,
        orchestrator_agent_rs::ToolOperationStatus::Committed
    );
    restored.shutdown().await.unwrap();
}
