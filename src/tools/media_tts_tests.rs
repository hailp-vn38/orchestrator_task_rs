use super::*;

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
            provider_guidance:
                "Choose ngoc_huyen for female narration or hai_dang for male narration.".to_string(),
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
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
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
