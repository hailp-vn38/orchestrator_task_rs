//! Công cụ `tts_synthesize`: tổng hợp văn bản thành giọng nói (`audio/wav`) kèm ràng buộc
//! danh sách giọng Nam/Nữ động (`TtsProviderConstraints`) qua seam `TtsSynthesizerBackend`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use llm_wiki::slugify;
use orchestrator_agent_rs::{
    ArtifactInput, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::media_artifacts::{mirror_artifact_to_dir, parse_role, TTS_SYNTHESIZE_TIMEOUT};
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Thông tin giọng đọc và giới hạn theo provider hiện hành của công cụ `tts_synthesize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsProviderConstraints {
    /// Nhãn hiển thị của provider/adapter đang kích hoạt (ví dụ: `"VieNeu Local TTS (model: vieneu_v3_turbo_fp32)"`).
    pub provider_label: String,
    /// Mã giọng mặc định đang được cấu hình trên provider (ví dụ: `"hai_dang"`, `"vi-VN-HoaiMyNeural"`).
    pub default_voice: String,
    /// Danh sách mã giọng Nữ (`female`) được quảng bá cho Subagent trên provider hiện hành.
    pub female_voices: Vec<String>,
    /// Danh sách mã giọng Nam (`male`) được quảng bá cho Subagent trên provider hiện hành.
    pub male_voices: Vec<String>,
    /// Hướng dẫn chọn giọng cụ thể cho Subagent khi gọi `tts_synthesize`.
    pub provider_guidance: String,
}

impl Default for TtsProviderConstraints {
    fn default() -> Self {
        Self {
            provider_label: "builtin_wav_synthesizer".to_string(),
            default_voice: "vi-default".to_string(),
            female_voices: vec!["vi-female".to_string()],
            male_voices: vec!["vi-male".to_string()],
            provider_guidance:
                "Pass a female voice ID for female narration or a male voice ID for male narration."
                    .to_string(),
        }
    }
}

/// Yêu cầu tổng hợp giọng nói (TTS) gửi tới [`TtsSynthesizerBackend`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsSynthesisRequest {
    pub text: String,
    pub filename: String,
    pub voice: String,
    pub language: String,
    pub speed: f32,
}

/// Kết quả âm thanh sinh ra từ [`TtsSynthesizerBackend`].
#[derive(Debug, Clone)]
pub struct SynthesizedAudioPayload {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub sample_rate_hz: u32,
    pub duration_ms: u64,
    pub provider: String,
    /// Mã giọng thực tế đã sử dụng để tổng hợp (sau khi phân giải theo provider/giới tính).
    pub voice: String,
}

/// Trait mở rộng cho phép Host cắm bất kỳ engine TTS nào (VieNeu, ZeroTTS, Edge TTS, Kokoro, ChillAudio, OpenAI Speech, ...).
#[async_trait::async_trait]
pub trait TtsSynthesizerBackend: Send + Sync {
    fn tts_constraints(&self) -> TtsProviderConstraints {
        TtsProviderConstraints::default()
    }

    async fn synthesize_tts(
        &self,
        req: &TtsSynthesisRequest,
    ) -> Result<SynthesizedAudioPayload, String>;
}

/// Backend TTS mặc định: tổng hợp file âm thanh RIFF/WAVE PCM 16-bit mono (`audio/wav`) hợp lệ
/// với nhịp âm lượng điều chế theo chuỗi từ của văn bản đầu vào.
#[derive(Debug, Clone, Default)]
pub struct DefaultTtsSynthesizerBackend;

#[async_trait::async_trait]
impl TtsSynthesizerBackend for DefaultTtsSynthesizerBackend {
    async fn synthesize_tts(
        &self,
        req: &TtsSynthesisRequest,
    ) -> Result<SynthesizedAudioPayload, String> {
        let sample_rate_hz = 16_000u32;
        let word_count = req.text.split_whitespace().count().max(1);
        let speed = req.speed.clamp(0.5, 2.0);
        // Thời lượng tỉ lệ theo độ dài câu (giới hạn 250ms..=4000ms để vừa nhanh vừa đủ nghe kiểm thử)
        let raw_ms = ((word_count as f32) * 120.0 / speed) as u64;
        let duration_ms = raw_ms.clamp(250, 4_000);
        let wav_bytes = synthesize_pcm_wav(&req.text, sample_rate_hz, duration_ms);

        let stem = slugify(
            req.filename
                .trim_end_matches(".wav")
                .trim_end_matches(".mp3"),
            64,
        );
        let safe_name = format!("{}.wav", if stem.is_empty() { "tts-audio" } else { &stem });
        Ok(SynthesizedAudioPayload {
            filename: safe_name,
            mime_type: "audio/wav".to_string(),
            bytes: wav_bytes,
            sample_rate_hz,
            duration_ms,
            provider: "builtin_wav_synthesizer".to_string(),
            voice: req.voice.clone(),
        })
    }
}

/// Công cụ `tts_synthesize`: chuyển đổi văn bản thành giọng nói (`audio/wav` / `audio/*`)
/// và lưu trữ vào `ArtifactStore` của Task.
pub struct TtsSynthesizeTool {
    pub backend: Arc<dyn TtsSynthesizerBackend>,
    pub output_dir: Option<PathBuf>,
}

impl TtsSynthesizeTool {
    pub fn new(backend: Arc<dyn TtsSynthesizerBackend>) -> Self {
        Self {
            backend,
            output_dir: None,
        }
    }

    pub fn with_output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

impl Default for TtsSynthesizeTool {
    fn default() -> Self {
        Self::new(Arc::new(DefaultTtsSynthesizerBackend))
    }
}

#[async_trait::async_trait]
impl HostTool for TtsSynthesizeTool {
    fn descriptor(&self) -> ToolDescriptor {
        let c = self.backend.tts_constraints();
        let female_list = if c.female_voices.is_empty() {
            "none".to_string()
        } else {
            c.female_voices.join(", ")
        };
        let male_list = if c.male_voices.is_empty() {
            "none".to_string()
        } else {
            c.male_voices.join(", ")
        };

        ToolDescriptor {
            name: "tts_synthesize".to_string(),
            description: format!(
                "Synthesize standalone spoken audio (TTS WAV) from text when the user requests an audio-only artifact. \
                 Do NOT call `tts_synthesize` to create audio for a video (`video_create` already generates video with integrated voiceover audio and subtitles). \
                 [Active provider: {label} | default_voice='{default_voice}' | Female voices: [{female_list}] | Male voices: [{male_list}]. {guidance}]",
                label = c.provider_label,
                default_voice = c.default_voice,
                guidance = c.provider_guidance,
            ),
            input_schema: json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The text script to synthesize into spoken audio."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Output audio filename (e.g. 'summary-tts.wav'). Defaults to 'tts-output.wav'."
                    },
                    "voice": {
                        "type": "string",
                        "description": format!(
                            "Voice identifier for active provider '{}' (default: '{}'). \
                             For a FEMALE voice, choose one of: [{}] (or pass 'female'). \
                             For a MALE voice, choose one of: [{}] (or pass 'male').",
                            c.provider_label,
                            c.default_voice,
                            female_list,
                            male_list,
                        )
                    },
                    "language": {
                        "type": "string",
                        "description": "BCP-47 language code (default: 'vi-VN')."
                    },
                    "speed": {
                        "type": "number",
                        "description": "Playback speed multiplier (0.5 to 2.0, default: 1.0)."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["primary", "intermediate"],
                        "description": "Artifact visibility role (default: 'primary')."
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::Mutating,
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(TTS_SYNTHESIZE_TIMEOUT)
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing text for TTS synthesis"))?;
        let filename = args
            .get("filename")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("tts-output.wav")
            .to_string();
        let voice = args
            .get("voice")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.backend.tts_constraints().default_voice);
        let language = args
            .get("language")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("vi-VN")
            .to_string();
        let speed = args
            .get("speed")
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .unwrap_or(1.0);
        let role = parse_role(args.get("role").and_then(Value::as_str));

        let req = TtsSynthesisRequest {
            text: text.to_string(),
            filename,
            voice: voice.clone(),
            language: language.clone(),
            speed,
        };

        let synthesized = self
            .backend
            .synthesize_tts(&req)
            .await
            .map_err(|e| host_tool_err("TTS_SYNTHESIS_FAILED", e))?;

        let effective_voice = if synthesized.voice.trim().is_empty() {
            voice
        } else {
            synthesized.voice.clone()
        };

        let mirrored_path = mirror_artifact_to_dir(
            self.output_dir.as_ref(),
            &synthesized.filename,
            &synthesized.bytes,
        );

        let artifact = ctx
            .artifacts()
            .put(ArtifactInput::from_bytes(
                &synthesized.filename,
                &synthesized.mime_type,
                role,
                synthesized.bytes,
            ))
            .await?;

        Ok(host_tool_outcome(
            &ctx,
            format!(
                "Synthesized TTS audio artifact '{}' (id={}, mime={}, voice={}, duration_ms={}, bytes={})",
                artifact.name,
                artifact.id,
                artifact.mime_type,
                effective_voice,
                synthesized.duration_ms,
                artifact.size_bytes
            ),
            json!({
                "artifact_id": artifact.id,
                "artifact_ids": [artifact.id],
                "artifact_kind": "audio",
                "filename": artifact.name,
                "mime_type": artifact.mime_type,
                "size_bytes": artifact.size_bytes,
                "sha256": artifact.sha256,
                "duration_ms": synthesized.duration_ms,
                "sample_rate_hz": synthesized.sample_rate_hz,
                "voice": effective_voice,
                "language": language,
                "provider": synthesized.provider,
                "path": mirrored_path
            }),
        ))
    }
}

/// Tạo file âm thanh RIFF/WAVE PCM 16-bit mono hợp lệ.
pub(crate) fn synthesize_pcm_wav(text: &str, sample_rate: u32, duration_ms: u64) -> Vec<u8> {
    let num_samples = ((u64::from(sample_rate) * duration_ms) / 1000).max(160) as usize;
    let data_bytes_len = (num_samples * 2) as u32;
    let riff_size = 36u32.saturating_add(data_bytes_len);

    let mut wav = Vec::with_capacity(44 + data_bytes_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt subchunk (16 bytes for PCM)
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // AudioFormat = 1 (PCM)
    wav.extend_from_slice(&1u16.to_le_bytes()); // NumChannels = 1 (Mono)
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * 2;
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // BlockAlign = 2
    wav.extend_from_slice(&16u16.to_le_bytes()); // BitsPerSample = 16

    // data subchunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes_len.to_le_bytes());

    let text_bytes = text.as_bytes();
    for i in 0..num_samples {
        let t = (i as f32) / (sample_rate as f32);
        let char_idx = (i * text_bytes.len().max(1)) / num_samples.max(1);
        let ch_val = text_bytes.get(char_idx).copied().unwrap_or(b'a');
        let base_freq = 180.0 + f32::from(ch_val % 80) * 2.5;
        // Điều chế biên độ theo nhịp âm tiết (~5 Hz)
        let envelope = ((t * 5.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5).max(0.05);
        let carrier = (t * base_freq * std::f32::consts::TAU).sin();
        let sample = (carrier * envelope * 8_000.0) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    wav
}

#[cfg(test)]
#[path = "media_tts_tests.rs"]
mod tests;
