//! Công cụ `HostTool` `asr_transcribe` và Seam [`AsrTranscriberBackend`] cho phép Subagent
//! (`media_creator`) phiên âm các file âm thanh `.wav` hoặc video `.mp4` trong `ArtifactStore` của Task.

use std::sync::Arc;

use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

pub use crate::tools::asr_audio_io::{
    extract_wav_from_mp4_bytes, format_segments_as_srt, format_srt_timestamp, inspect_mp4_bytes,
    inspect_wav_bytes, WavHeaderInfo, WavSampleFormat,
};

/// Tên công cụ phiên âm file `.wav` hoặc `.mp4` trong `ArtifactStore`.
pub const ASR_TRANSCRIBE_TOOL: &str = "asr_transcribe";

/// Mã lỗi ổn định khi artifact âm thanh/video không phải định dạng `.wav` hoặc `.mp4` hợp lệ.
pub const CODE_ASR_UNSUPPORTED_AUDIO: &str = "ASR_UNSUPPORTED_AUDIO";

/// Yêu cầu phiên âm gửi tới [`AsrTranscriberBackend`].
#[derive(Debug, Clone)]
pub struct AsrTranscriptionRequest {
    pub wav_bytes: Vec<u8>,
    pub header: WavHeaderInfo,
    pub language: Option<String>,
}

/// Một phân đoạn phụ đề có mốc thời gian chuẩn (mili-giây) từ đầu file audio/video.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscribedAudioSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Kết quả phiên âm trả về từ [`AsrTranscriberBackend`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscribedAudioPayload {
    pub text: String,
    pub language: Option<String>,
    pub duration_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<TranscribedAudioSegment>,
}

/// Seam cho phép Host cắm engine ASR thực tế (Sherpa-ONNX Zipformer/Gipformer hoặc Cloud Audio ASR).
#[async_trait::async_trait]
pub trait AsrTranscriberBackend: Send + Sync {
    async fn transcribe_asr(
        &self,
        request: &AsrTranscriptionRequest,
    ) -> Result<TranscribedAudioPayload, String>;
}

/// Adapter 1 (mặc định cho kiểm thử độc lập tại `orchestrator_task_rs`):
/// trả về [`TranscribedAudioPayload`] tất định dựa trên [`WavHeaderInfo`].
#[derive(Debug, Clone, Default)]
pub struct DefaultAsrTranscriberBackend;

#[async_trait::async_trait]
impl AsrTranscriberBackend for DefaultAsrTranscriberBackend {
    async fn transcribe_asr(
        &self,
        request: &AsrTranscriptionRequest,
    ) -> Result<TranscribedAudioPayload, String> {
        let lang = request
            .language
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("vi")
            .to_string();
        let text = format!(
            "[wav_transcript: {}Hz, {}ch, {}bit, {}ms, lang={}]",
            request.header.sample_rate,
            request.header.channels,
            request.header.bits_per_sample,
            request.header.duration_ms,
            lang
        );
        let dur = request.header.duration_ms.max(1);
        Ok(TranscribedAudioPayload {
            text: text.clone(),
            language: Some(lang),
            duration_ms: request.header.duration_ms,
            sample_rate: request.header.sample_rate,
            channels: request.header.channels,
            provider_id: Some("builtin_wav_inspector".to_string()),
            model: Some("deterministic-wav-v1".to_string()),
            segments: vec![TranscribedAudioSegment {
                start_ms: 0,
                end_ms: dur,
                text,
            }],
        })
    }
}

/// Deep Module `AsrTranscribeTool` (`HostTool` tên `"asr_transcribe"`).
pub struct AsrTranscribeTool {
    pub backend: Arc<dyn AsrTranscriberBackend>,
}

impl AsrTranscribeTool {
    pub fn new(backend: Arc<dyn AsrTranscriberBackend>) -> Self {
        Self { backend }
    }

    pub fn with_backend(backend: Arc<dyn AsrTranscriberBackend>) -> Self {
        Self::new(backend)
    }

    pub fn with_default_backend() -> Self {
        Self::new(Arc::new(DefaultAsrTranscriberBackend))
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

impl Default for AsrTranscribeTool {
    fn default() -> Self {
        Self::with_default_backend()
    }
}

#[async_trait::async_trait]
impl HostTool for AsrTranscribeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ASR_TRANSCRIBE_TOOL.to_string(),
            description: "Transcribe spoken audio from a `.wav` audio or `.mp4` video Task Artifact (`artifact_id`) in the current Task's ArtifactStore into text, timed subtitle `segments` (`index`, `start_ms`, `end_ms`, `text`), and SubRip `srt` using the configured ASR provider.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["artifact_id"],
                "additionalProperties": false,
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Artifact ID (e.g. 'art-...') of the .wav audio or .mp4 video file in the current Task's ArtifactStore."
                    },
                    "language": {
                        "type": "string",
                        "description": "Optional ISO language code hint (e.g. 'vi', 'en')."
                    }
                }
            }),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let artifact_id = args
            .get("artifact_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing or empty artifact_id"))?;
        let language = args
            .get("language")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let reader = ctx.artifacts().open(artifact_id).await?;
        let raw_bytes = reader
            .read_all()
            .await
            .map_err(|e| host_tool_err("ARTIFACT_INTEGRITY", e.to_string()))?;

        let (wav_bytes, header) = match inspect_wav_bytes(&raw_bytes) {
            Ok(h) => (raw_bytes, h),
            Err(wav_err) => {
                if inspect_mp4_bytes(&raw_bytes).is_ok() {
                    let extracted = extract_wav_from_mp4_bytes(&raw_bytes).await.map_err(|e| {
                        host_tool_err(
                            CODE_ASR_UNSUPPORTED_AUDIO,
                            format!(
                                "Artifact '{artifact_id}' is an .mp4 file without a readable audio track: {e}"
                            ),
                        )
                    })?;
                    let h = inspect_wav_bytes(&extracted).map_err(|e| {
                        host_tool_err(
                            CODE_ASR_UNSUPPORTED_AUDIO,
                            format!(
                                "Extracted WAV from MP4 artifact '{artifact_id}' is invalid: {e}"
                            ),
                        )
                    })?;
                    (extracted, h)
                } else {
                    return Err(host_tool_err(
                        CODE_ASR_UNSUPPORTED_AUDIO,
                        format!(
                            "Artifact '{artifact_id}' is not a valid .wav audio or .mp4 video file: {wav_err}"
                        ),
                    ));
                }
            }
        };

        let req = AsrTranscriptionRequest {
            wav_bytes,
            header,
            language: language.clone(),
        };
        let transcribed = self
            .backend
            .transcribe_asr(&req)
            .await
            .map_err(|e| host_tool_err("ASR_TRANSCRIPTION_FAILED", e))?;

        let effective_segments: Vec<TranscribedAudioSegment> = if !transcribed.segments.is_empty() {
            transcribed
                .segments
                .into_iter()
                .filter(|s| !s.text.trim().is_empty())
                .collect()
        } else if !transcribed.text.trim().is_empty() {
            vec![TranscribedAudioSegment {
                start_ms: 0,
                end_ms: transcribed.duration_ms.max(1),
                text: transcribed.text.trim().to_string(),
            }]
        } else {
            Vec::new()
        };

        let indexed_segments: Vec<Value> = effective_segments
            .iter()
            .enumerate()
            .map(|(idx, seg)| {
                json!({
                    "index": idx + 1,
                    "start_ms": seg.start_ms,
                    "end_ms": seg.end_ms.max(seg.start_ms + 1),
                    "text": seg.text.trim(),
                })
            })
            .collect();
        let srt = format_segments_as_srt(&effective_segments);

        let payload = json!({
            "status": "ok",
            "artifact_id": artifact_id,
            "transcript_text": transcribed.text,
            "language": transcribed.language.or(language),
            "duration_ms": transcribed.duration_ms,
            "sample_rate": transcribed.sample_rate,
            "channels": transcribed.channels,
            "provider_id": transcribed.provider_id,
            "model": transcribed.model,
            "segments": indexed_segments,
            "srt": srt,
        });
        let content = serde_json::to_string(&payload).unwrap_or_default();

        Ok(host_tool_outcome(&ctx, content, payload))
    }
}

#[cfg(test)]
#[path = "asr_transcribe_tests.rs"]
mod tests;
