//! Bộ công cụ `HostTool` đa phương tiện & đa định dạng cho Subagent (`image_create`, `video_create`, `video_concat`, `tts_synthesize`, `artifact_create`)
//! xuất bản trực tiếp vào `ArtifactStore` (`ctx.artifacts().put(...)`) của `Orchestrator` và hiển thị
//! dưới dạng [`TaskArtifactView`] trong `TaskManager`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use llm_wiki::slugify;
use orchestrator_agent_rs::{
    ArtifactInput, ArtifactRole, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError,
    ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};
use crate::types::TaskArtifactKind;

/// Chiều rộng mặc định cho ảnh raster PNG (`1024` px — tương thích chuẩn OpenAI Images / DALL-E / Flux).
pub const DEFAULT_PNG_WIDTH: u32 = 1024;
/// Chiều cao mặc định cho ảnh raster PNG (`1024` px).
pub const DEFAULT_PNG_HEIGHT: u32 = 1024;
/// Chiều rộng mặc định cho sơ đồ vector SVG (`1200` px — tỷ lệ 16:9 widescreen).
pub const DEFAULT_SVG_WIDTH: u32 = 1200;
/// Chiều cao mặc định cho sơ đồ vector SVG (`675` px — tỷ lệ 16:9 widescreen).
pub const DEFAULT_SVG_HEIGHT: u32 = 675;
/// Chiều rộng mặc định cho video clip MP4 (`640` px).
pub const DEFAULT_VIDEO_WIDTH: u32 = 640;
/// Chiều cao mặc định cho video clip MP4 (`360` px).
pub const DEFAULT_VIDEO_HEIGHT: u32 = 360;
/// Số lượng ảnh tham chiếu tối đa cho một lệnh `image_create`.
pub const MAX_IMAGE_REFERENCES: usize = 4;
/// Giới hạn kích thước mỗi ảnh tham chiếu (10 MiB).
pub const MAX_REFERENCE_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Timeout cho `image_create` (10 phút — lớn hơn timeout nội bộ + failover của backend).
pub const IMAGE_CREATE_TIMEOUT: Duration = Duration::from_secs(600);
/// Timeout cho `video_create` (30 phút — lớn hơn thời gian tạo/ghép đa phân cảnh + overrun + failover của Muse).
pub const VIDEO_CREATE_TIMEOUT: Duration = Duration::from_secs(1800);
/// Timeout cho `video_concat` (10 phút — đủ cho chuẩn hóa/ghép tối đa 24 clip qua ffmpeg).
pub const VIDEO_CONCAT_TIMEOUT: Duration = Duration::from_secs(600);
/// Timeout cho `tts_synthesize` (10 phút).
pub const TTS_SYNTHESIZE_TIMEOUT: Duration = Duration::from_secs(600);

#[cfg(test)]
pub(crate) use crate::tools::media_image::{
    encode_base64, encode_gradient_png, resolve_first_frame_from_args,
    resolve_image_references_from_args,
};
pub use crate::tools::media_image::{
    DefaultImageGeneratorBackend, GeneratedImagePayload, ImageCreateTool, ImageGenerationRequest,
    ImageGeneratorBackend, ReferenceImageInput,
};
pub use crate::tools::media_video::{
    normalize_video_stem, ConcatenatedVideoPayload, DefaultVideoConcatBackend,
    DefaultVideoGeneratorBackend, GeneratedVideoClip, GeneratedVideoPayload, VideoConcatBackend,
    VideoConcatInputClip, VideoConcatRequest, VideoConcatTool, VideoCreateTool,
    VideoGenerationRequest, VideoGeneratorBackend, VideoProviderConstraints, VIDEO_CONCAT_TOOL,
};
#[cfg(test)]
pub(crate) use crate::tools::media_video_concat::encode_minimal_mp4;
pub(crate) use crate::tools::media_video_concat::find_iso_bmff_box;
pub use crate::tools::media_video_concat::inspect_mp4_duration_secs;

pub(crate) fn mirror_artifact_to_dir(
    output_dir: Option<&PathBuf>,
    filename: &str,
    bytes: &[u8],
) -> Option<String> {
    let dir = output_dir?;
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(filename);
    std::fs::write(&path, bytes).ok()?;
    Some(path.display().to_string())
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
}

/// Trait mở rộng cho phép Host cắm bất kỳ engine TTS nào (Kokoro, Piper, VietTTS, OpenAI Speech, ElevenLabs, ...).
#[async_trait::async_trait]
pub trait TtsSynthesizerBackend: Send + Sync {
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
        ToolDescriptor {
            name: "tts_synthesize".to_string(),
            description:
                "Synthesize standalone spoken audio (TTS WAV) from text when the user requests an audio-only artifact. Do NOT call `tts_synthesize` to create audio for a video (`video_create` already generates video with integrated voiceover audio and subtitles)."
                    .to_string(),
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
                        "description": "Voice identifier (e.g. 'vi-female', 'alloy', 'default')."
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
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("vi-default")
            .to_string();
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
                "Synthesized TTS audio artifact '{}' (id={}, mime={}, duration_ms={}, bytes={})",
                artifact.name,
                artifact.id,
                artifact.mime_type,
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
                "voice": voice,
                "language": language,
                "provider": synthesized.provider,
                "path": mirrored_path
            }),
        ))
    }
}

/// Công cụ `artifact_create`: cho phép Subagent lưu trữ bất kỳ loại Artifact nào
/// (JSON data, CSV, HTML, Markdown, SVG, hoặc dữ liệu nhị phân mã hóa hex) vào `ArtifactStore`.
#[derive(Debug, Clone, Default)]
pub struct ArtifactCreateTool {
    pub output_dir: Option<PathBuf>,
}

impl ArtifactCreateTool {
    pub fn new() -> Self {
        Self { output_dir: None }
    }

    pub fn with_output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

#[async_trait::async_trait]
impl HostTool for ArtifactCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "artifact_create".to_string(),
            description: "Publish a multi-format Task Artifact (Markdown, JSON, CSV, HTML, SVG, or hex-encoded binary) into the Task's ArtifactStore.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filename", "mime_type"],
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Artifact filename with extension (e.g. 'metrics.json', 'table.csv', 'diagram.svg')."
                    },
                    "mime_type": {
                        "type": "string",
                        "description": "MIME type (e.g. 'application/json', 'text/csv', 'text/markdown; charset=utf-8', 'image/svg+xml')."
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 text content of the artifact."
                    },
                    "hex_bytes": {
                        "type": "string",
                        "description": "Optional hex-encoded binary payload (used when publishing binary files instead of UTF-8 'content')."
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

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let filename = args
            .get("filename")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let mime_type = args
            .get("mime_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing mime_type"))?;
        let role = parse_role(args.get("role").and_then(Value::as_str));

        let payload_bytes = if let Some(hex_str) = args
            .get("hex_bytes")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            decode_hex(hex_str).map_err(|e| host_tool_err("INVALID_ARGUMENT", e))?
        } else if let Some(content) = args.get("content").and_then(Value::as_str) {
            content.as_bytes().to_vec()
        } else {
            return Err(host_tool_err(
                "INVALID_ARGUMENT",
                "Either 'content' or 'hex_bytes' must be provided",
            ));
        };

        let kind = TaskArtifactKind::from_mime_and_name(mime_type, filename);
        let mirrored_path =
            mirror_artifact_to_dir(self.output_dir.as_ref(), filename, &payload_bytes);
        let artifact = ctx
            .artifacts()
            .put(ArtifactInput::from_bytes(
                filename,
                mime_type,
                role,
                payload_bytes,
            ))
            .await?;

        Ok(host_tool_outcome(
            &ctx,
            format!(
                "Published {} artifact '{}' (id={}, mime={}, bytes={})",
                kind.as_str(),
                artifact.name,
                artifact.id,
                artifact.mime_type,
                artifact.size_bytes
            ),
            json!({
                "artifact_id": artifact.id,
                "artifact_ids": [artifact.id],
                "artifact_kind": kind.as_str(),
                "filename": artifact.name,
                "mime_type": artifact.mime_type,
                "size_bytes": artifact.size_bytes,
                "sha256": artifact.sha256,
                "path": mirrored_path
            }),
        ))
    }
}

/// Xuất bộ 4 `HostTool` đa phương tiện & artifact (`image_create`, `video_create`, `tts_synthesize`, `artifact_create`)
/// với `DefaultVideoGeneratorBackend` mặc định.
pub fn media_host_tools(
    image_backend: Arc<dyn ImageGeneratorBackend>,
    tts_backend: Arc<dyn TtsSynthesizerBackend>,
    output_dir: Option<PathBuf>,
) -> Vec<Arc<dyn HostTool>> {
    media_host_tools_with_video(
        image_backend,
        Arc::new(DefaultVideoGeneratorBackend),
        tts_backend,
        output_dir,
    )
}

/// Xuất bộ 5 `HostTool` đa phương tiện & artifact (`image_create`, `video_create`, `video_concat`, `tts_synthesize`, `artifact_create`)
/// với đầy đủ các backend tùy chỉnh cho Image, Video và TTS.
pub fn media_host_tools_with_video(
    image_backend: Arc<dyn ImageGeneratorBackend>,
    video_backend: Arc<dyn VideoGeneratorBackend>,
    tts_backend: Arc<dyn TtsSynthesizerBackend>,
    output_dir: Option<PathBuf>,
) -> Vec<Arc<dyn HostTool>> {
    let mut img = ImageCreateTool::new(image_backend);
    let mut vid = VideoCreateTool::new(video_backend);
    let mut vid_concat = VideoConcatTool::with_default_backend();
    let mut tts = TtsSynthesizeTool::new(tts_backend);
    let mut gen = ArtifactCreateTool::new();
    if let Some(dir) = output_dir {
        img = img.with_output_dir(dir.join("images"));
        vid = vid.with_output_dir(dir.join("videos"));
        vid_concat = vid_concat.with_output_dir(dir.join("videos"));
        tts = tts.with_output_dir(dir.join("tts"));
        gen = gen.with_output_dir(dir.join("files"));
    }
    vec![
        Arc::new(img),
        Arc::new(vid),
        Arc::new(vid_concat),
        Arc::new(tts),
        Arc::new(gen),
    ]
}

pub(crate) fn parse_role(raw: Option<&str>) -> ArtifactRole {
    match raw.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("intermediate") => ArtifactRole::Intermediate,
        _ => ArtifactRole::Primary,
    }
}

fn decode_hex(hex_str: &str) -> Result<Vec<u8>, String> {
    let clean: String = hex_str.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err("hex_bytes length must be even".to_string());
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    let bytes = clean.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = (bytes[i] as char)
            .to_digit(16)
            .ok_or_else(|| "invalid hex character".to_string())?;
        let lo = (bytes[i + 1] as char)
            .to_digit(16)
            .ok_or_else(|| "invalid hex character".to_string())?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
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
#[path = "media_artifacts_tests.rs"]
mod tests;
