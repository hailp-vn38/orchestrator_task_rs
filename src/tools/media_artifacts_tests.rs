use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingMediaBackend {
    image_requests: Mutex<Vec<ImageGenerationRequest>>,
    video_requests: Mutex<Vec<VideoGenerationRequest>>,
}

#[async_trait::async_trait]
impl ImageGeneratorBackend for RecordingMediaBackend {
    async fn generate_image(
        &self,
        req: &ImageGenerationRequest,
    ) -> Result<GeneratedImagePayload, String> {
        self.image_requests.lock().unwrap().push(req.clone());
        DefaultImageGeneratorBackend.generate_image(req).await
    }
}

#[async_trait::async_trait]
impl VideoGeneratorBackend for RecordingMediaBackend {
    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
    ) -> Result<GeneratedVideoPayload, String> {
        self.video_requests.lock().unwrap().push(req.clone());
        DefaultVideoGeneratorBackend.generate_video(req).await
    }
}

#[tokio::test]
async fn default_image_and_video_backends_support_both_default_and_agent_custom_dimensions() {
    let img_backend = DefaultImageGeneratorBackend;

    // 1. Default PNG dimensions (0 -> 1024x1024) + verify PNG IHDR binary header matches payload
    let default_png = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Futuristic AI data center".into(),
            title: None,
            filename: "banner.png".into(),
            format: "png".into(),
            width: 0,
            height: 0,
            svg_content: None,
            reference_images: Vec::new(),
            transparent_background: false,
        })
        .await
        .unwrap();
    assert_eq!(default_png.width, DEFAULT_PNG_WIDTH);
    assert_eq!(default_png.height, DEFAULT_PNG_HEIGHT);
    assert_eq!(default_png.mime_type, "image/png");
    let ihdr_w = u32::from_be_bytes(default_png.bytes[16..20].try_into().unwrap());
    let ihdr_h = u32::from_be_bytes(default_png.bytes[20..24].try_into().unwrap());
    assert_eq!(ihdr_w, DEFAULT_PNG_WIDTH);
    assert_eq!(ihdr_h, DEFAULT_PNG_HEIGHT);

    // 2. Agent-set custom PNG dimensions (1792x1024) + verify PNG IHDR matches
    let custom_png = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Widescreen concept art".into(),
            title: None,
            filename: "wide.png".into(),
            format: "png".into(),
            width: 1792,
            height: 1024,
            svg_content: None,
            reference_images: Vec::new(),
            transparent_background: false,
        })
        .await
        .unwrap();
    assert_eq!(custom_png.width, 1792);
    assert_eq!(custom_png.height, 1024);
    let custom_ihdr_w = u32::from_be_bytes(custom_png.bytes[16..20].try_into().unwrap());
    let custom_ihdr_h = u32::from_be_bytes(custom_png.bytes[20..24].try_into().unwrap());
    assert_eq!(custom_ihdr_w, 1792);
    assert_eq!(custom_ihdr_h, 1024);

    // 3. Default SVG dimensions (0 -> 1200x675) with multi-line wrapped card fallback
    let default_svg = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Step 1: Collect -> Step 2: Analyze -> Step 3: Visualize".into(),
            title: Some("Pipeline Flow".into()),
            filename: "flow.svg".into(),
            format: "svg".into(),
            width: 0,
            height: 0,
            svg_content: None,
            reference_images: Vec::new(),
            transparent_background: false,
        })
        .await
        .unwrap();
    assert_eq!(default_svg.width, DEFAULT_SVG_WIDTH);
    assert_eq!(default_svg.height, DEFAULT_SVG_HEIGHT);
    assert_eq!(default_svg.mime_type, "image/svg+xml");
    let svg_str = String::from_utf8(default_svg.bytes).unwrap();
    assert!(svg_str.contains("width=\"1200\" height=\"675\""));
    assert!(svg_str.contains("<tspan"));

    // 4. Full <svg> markup in svg_content syncs its root dimensions into GeneratedImagePayload
    let custom_svg = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Architecture diagram".into(),
            title: None,
            filename: "arch.svg".into(),
            format: "svg".into(),
            width: 0,
            height: 0,
            svg_content: Some(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 960 540" width="960" height="540"><rect width="960" height="540"/></svg>"#
                    .into(),
            ),
            reference_images: Vec::new(),
            transparent_background: false,
        })
        .await
        .unwrap();
    assert_eq!(custom_svg.width, 960);
    assert_eq!(custom_svg.height, 540);

    // 5. Default & custom video dimensions
    let vid_backend = DefaultVideoGeneratorBackend;
    let default_vid = vid_backend
        .generate_video(&VideoGenerationRequest {
            prompt: "Overview animation".into(),
            title: None,
            filename: "clip.mp4".into(),
            width: 0,
            height: 0,
            duration_secs: 4,
            first_frame: None,
        })
        .await
        .unwrap();
    assert_eq!(default_vid.width, DEFAULT_VIDEO_WIDTH);
    assert_eq!(default_vid.height, DEFAULT_VIDEO_HEIGHT);
}

#[tokio::test]
async fn image_create_and_video_create_resolve_data_url_and_enforce_max_references() {
    // 1x1 valid PNG header bytes encoded as base64 data URL
    let sample_png = encode_gradient_png(16, 16, "ref");
    let b64 = encode_base64(&sample_png);
    let data_url = format!("data:image/png;base64,{b64}");

    let resolved_refs = resolve_image_references_from_args(
        None,
        &json!({
            "reference_images": [data_url.clone(), data_url.clone()],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resolved_refs.len(), 2);
    assert_eq!(resolved_refs[0].mime_type, "image/png");
    assert_eq!(resolved_refs[0].bytes, sample_png);

    // Exceeding MAX_IMAGE_REFERENCES (4) returns TOO_MANY_REFERENCE_IMAGES
    let too_many_err = resolve_image_references_from_args(
        None,
        &json!({
            "reference_images": [
                data_url.clone(),
                data_url.clone(),
                data_url.clone(),
                data_url.clone(),
                data_url.clone()
            ]
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(too_many_err.code, "TOO_MANY_REFERENCE_IMAGES");

    // Video first_frame_image data URL
    let first_frame = resolve_first_frame_from_args(
        None,
        &json!({
            "first_frame_image": data_url,
        }),
    )
    .await
    .unwrap()
    .expect("first_frame resolved");
    assert_eq!(first_frame.mime_type, "image/png");
    assert_eq!(first_frame.bytes, sample_png);
}

#[tokio::test]
async fn image_create_and_video_create_tools_resolve_artifact_references_end_to_end() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, PageRequest, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media creator.").unwrap();

    let recording = Arc::new(RecordingMediaBackend::default());
    let img_tool = ImageCreateTool::new(recording.clone()).into_host_tool();
    let vid_tool = VideoCreateTool::new(recording.clone()).into_host_tool();

    let gw = ScriptedGateway::new();
    // Turn 1: create base image artifact
    gw.push_tool_calls(
        "Creating base reference image",
        vec![(
            "call_base_img",
            "image_create",
            json!({
                "filename": "base-ref.png",
                "prompt": "Base reference portrait",
                "width": 32,
                "height": 32
            }),
        )],
    );
    gw.push_text("Base image created.");

    let agent = AgentDefinition::new("media_agent", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Media creator")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["image_create", "video_create"])
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
    .register_tool(img_tool)
    .register_tool(vid_tool)
    .build()
    .await
    .unwrap();

    let caller = CallerContext::new("owner1", "ws1");
    let session_id = orch.create_session(&caller, "media_agent").await.unwrap();
    let accepted1 = orch
        .submit(&caller, &session_id, "Create base image".to_string())
        .await
        .unwrap();

    let mut arts = Vec::new();
    for _ in 0..50 {
        let page = orch
            .list_artifacts(
                &caller,
                &session_id,
                &accepted1.turn_id,
                PageRequest::default(),
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
            arts = page.items;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(arts.len(), 1);
    let base_art = &arts[0];
    let base_bytes = orch
        .open_artifact(&caller, &base_art.id)
        .await
        .unwrap()
        .read_all()
        .await
        .unwrap();

    // Turn 2: invoke image_create with reference_artifact_ids + transparent_background
    // and video_create with first_frame_artifact_id
    gw.push_tool_calls(
        "Creating edited image and video from base artifact",
        vec![
            (
                "call_ref_img",
                "image_create",
                json!({
                    "filename": "edited.png",
                    "prompt": "Add sunglasses",
                    "width": 64,
                    "height": 64,
                    "reference_artifact_ids": [base_art.id],
                    "transparent_background": true
                }),
            ),
            (
                "call_ref_vid",
                "video_create",
                json!({
                    "filename": "animated.mp4",
                    "prompt": "Make the portrait smile",
                    "width": 640,
                    "height": 360,
                    "duration_secs": 5,
                    "first_frame_artifact_id": base_art.id
                }),
            ),
        ],
    );
    gw.push_text("Created edited image and video.");

    let _accepted2 = orch
        .submit(&caller, &session_id, "Edit and animate".to_string())
        .await
        .unwrap();
    for _ in 0..50 {
        if !orch
            .snapshot(&caller, &session_id)
            .await
            .unwrap()
            .has_active_work()
            && recording.video_requests.lock().unwrap().len() == 1
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    {
        let img_reqs = recording.image_requests.lock().unwrap();
        assert_eq!(img_reqs.len(), 2);
        assert_eq!(img_reqs[1].prompt, "Add sunglasses");
        assert!(img_reqs[1].transparent_background);
        assert_eq!(img_reqs[1].reference_images.len(), 1);
        assert_eq!(img_reqs[1].reference_images[0].mime_type, "image/png");
        assert_eq!(img_reqs[1].reference_images[0].bytes, base_bytes);
    }

    {
        let vid_reqs = recording.video_requests.lock().unwrap();
        assert_eq!(vid_reqs.len(), 1);
        assert_eq!(vid_reqs[0].prompt, "Make the portrait smile");
        let first_frame = vid_reqs[0]
            .first_frame
            .as_ref()
            .expect("first_frame present");
        assert_eq!(first_frame.mime_type, "image/png");
        assert_eq!(first_frame.bytes, base_bytes);
    }

    orch.shutdown().await.unwrap();
}

#[test]
fn media_tools_declare_explicit_timeouts_above_default_runtime_limit() {
    assert_eq!(
        ImageCreateTool::default().timeout(),
        Some(IMAGE_CREATE_TIMEOUT)
    );
    assert_eq!(
        VideoCreateTool::default().timeout(),
        Some(VIDEO_CREATE_TIMEOUT)
    );
    assert_eq!(
        VideoConcatTool::default().timeout(),
        Some(VIDEO_CONCAT_TIMEOUT)
    );
    assert_eq!(
        TtsSynthesizeTool::default().timeout(),
        Some(TTS_SYNTHESIZE_TIMEOUT)
    );
    assert!(VIDEO_CREATE_TIMEOUT >= Duration::from_secs(900));
}

struct MockDynamicTtsBackend {
    constraints: TtsProviderConstraints,
    resolved_voice: String,
    seen_voices: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl TtsSynthesizerBackend for MockDynamicTtsBackend {
    fn tts_constraints(&self) -> TtsProviderConstraints {
        self.constraints.clone()
    }

    async fn synthesize_tts(
        &self,
        req: &TtsSynthesisRequest,
    ) -> Result<SynthesizedAudioPayload, String> {
        self.seen_voices.lock().unwrap().push(req.voice.clone());
        let wav_bytes = synthesize_pcm_wav(&req.text, 24_000, 300);
        Ok(SynthesizedAudioPayload {
            filename: req.filename.clone(),
            mime_type: "audio/wav".to_string(),
            bytes: wav_bytes,
            sample_rate_hz: 24_000,
            duration_ms: 300,
            provider: "vieneu_local".to_string(),
            voice: self.resolved_voice.clone(),
        })
    }
}

#[test]
fn tts_synthesize_descriptor_reflects_dynamic_provider_voice_constraints() {
    let backend = Arc::new(MockDynamicTtsBackend {
        constraints: TtsProviderConstraints {
            provider_label: "VieNeu Local TTS (model: vieneu_v3_turbo_fp32)".to_string(),
            default_voice: "hai_dang".to_string(),
            female_voices: vec![
                "ngoc_huyen".to_string(),
                "mai_anh".to_string(),
                "truc_ly".to_string(),
            ],
            male_voices: vec!["hai_dang".to_string(), "thien_minh".to_string()],
            provider_guidance: "Choose ngoc_huyen for female narration or hai_dang for male narration."
                .to_string(),
        },
        resolved_voice: "ngoc_huyen".to_string(),
        seen_voices: std::sync::Mutex::new(Vec::new()),
    });
    let tool = TtsSynthesizeTool::new(backend);
    let desc = tool.descriptor();
    assert_eq!(desc.name, "tts_synthesize");
    assert!(desc
        .description
        .contains("VieNeu Local TTS (model: vieneu_v3_turbo_fp32)"));
    assert!(desc.description.contains("default_voice='hai_dang'"));
    assert!(desc
        .description
        .contains("Female voices: [ngoc_huyen, mai_anh, truc_ly]"));
    assert!(desc
        .description
        .contains("Male voices: [hai_dang, thien_minh]"));
    let voice_desc = desc.input_schema["properties"]["voice"]["description"]
        .as_str()
        .unwrap();
    assert!(voice_desc.contains("ngoc_huyen, mai_anh, truc_ly"));
    assert!(voice_desc.contains("hai_dang, thien_minh"));
    assert_eq!(desc.input_schema["additionalProperties"], json!(false));
}

#[tokio::test]
async fn tts_synthesize_execute_reports_resolved_provider_voice_in_artifact_metadata() {
    use orchestrator_agent_rs::test_support::ScriptedGateway;
    use orchestrator_agent_rs::{
        AgentDefinition, AgentMode, CallerContext, LlmBackend, LlmConfig, LlmProviderConfig,
        Orchestrator, OrchestratorConfig, ProviderAuth, SecretString,
    };
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "You are a media processor.").unwrap();

    let backend = Arc::new(MockDynamicTtsBackend {
        constraints: TtsProviderConstraints {
            provider_label: "VieNeu Local TTS (model: vieneu_v3_turbo_fp32)".to_string(),
            default_voice: "hai_dang".to_string(),
            female_voices: vec!["ngoc_huyen".to_string(), "mai_anh".to_string()],
            male_voices: vec!["hai_dang".to_string()],
            provider_guidance: "Use ngoc_huyen for female voice.".to_string(),
        },
        resolved_voice: "ngoc_huyen".to_string(),
        seen_voices: std::sync::Mutex::new(Vec::new()),
    });
    let tts_tool = TtsSynthesizeTool::new(backend.clone()).into_host_tool();

    let gw = ScriptedGateway::new();
    gw.push_tool_calls(
        "Synthesizing female voiceover",
        vec![(
            "call_tts_1",
            "tts_synthesize",
            json!({
                "text": "Xin chào các bạn",
                "filename": "female-intro.wav",
                "voice": "female"
            }),
        )],
    );
    gw.push_text("Done.");

    let agent = AgentDefinition::new("media_processor", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Media processor")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["tts_synthesize"])
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
    .register_tool(tts_tool)
    .build()
    .await
    .unwrap();

    let caller = CallerContext::new("owner1", "ws1");
    let session_id = orch
        .create_session(&caller, "media_processor")
        .await
        .unwrap();
    let _accepted = orch
        .submit(&caller, &session_id, "Tạo âm thanh giọng nữ".to_string())
        .await
        .unwrap();

    for _ in 0..50 {
        if !orch
            .snapshot(&caller, &session_id)
            .await
            .unwrap()
            .has_active_work()
            && gw.agent_requests().len() >= 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        backend.seen_voices.lock().unwrap().as_slice(),
        &["female".to_string()]
    );
    let reqs = gw.agent_requests();
    assert!(reqs.len() >= 2);
    let followup_debug = format!("{:?}", reqs[1].messages);
    assert!(
        followup_debug.contains("voice=ngoc_huyen"),
        "expected resolved voice in tool output: {followup_debug}"
    );
    assert!(followup_debug.contains("ngoc_huyen"));

    orch.shutdown().await.unwrap();
}
