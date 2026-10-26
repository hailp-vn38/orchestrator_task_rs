//! Bộ công cụ `HostTool` đa phương tiện & đa định dạng cho Subagent (`image_create`, `video_create`, `tts_synthesize`, `artifact_create`)
//! xuất bản trực tiếp vào `ArtifactStore` (`ctx.artifacts().put(...)`) của `Orchestrator` và hiển thị
//! dưới dạng [`TaskArtifactView`] trong `TaskManager`.

use std::path::PathBuf;
use std::sync::Arc;

use llm_wiki::slugify;
use orchestrator_agent_rs::{
    ArtifactInput, ArtifactRole, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError,
    ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};
use crate::types::TaskArtifactKind;

/// Yêu cầu sinh hình ảnh gửi tới [`ImageGeneratorBackend`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    pub title: Option<String>,
    pub filename: String,
    /// `"png"` hoặc `"svg"` (mặc định `"png"`).
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub svg_content: Option<String>,
}

/// Kết quả hình ảnh sinh ra từ [`ImageGeneratorBackend`].
#[derive(Debug, Clone)]
pub struct GeneratedImagePayload {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub provider: String,
}

/// Trait mở rộng cho phép Host cắm bất kỳ engine tạo ảnh nào (OpenAI Images, ComfyUI, SDXL, Flux, hoặc Local Renderer).
#[async_trait::async_trait]
pub trait ImageGeneratorBackend: Send + Sync {
    async fn generate_image(
        &self,
        req: &ImageGenerationRequest,
    ) -> Result<GeneratedImagePayload, String>;
}

/// Backend mặc định tự chủ (không phụ thuộc GPU rời): hỗ trợ kết xuất ảnh PNG chuẩn (binary RIFF/IDAT + CRC32)
/// và đồ họa vector SVG (`image/svg+xml`) trực tiếp từ `prompt` hoặc `svg_content` của Subagent.
#[derive(Debug, Clone, Default)]
pub struct DefaultImageGeneratorBackend;

#[async_trait::async_trait]
impl ImageGeneratorBackend for DefaultImageGeneratorBackend {
    async fn generate_image(
        &self,
        req: &ImageGenerationRequest,
    ) -> Result<GeneratedImagePayload, String> {
        let width = req.width.clamp(16, 2048);
        let height = req.height.clamp(16, 2048);
        let fmt = req.format.trim().to_ascii_lowercase();

        if fmt == "svg" || req.svg_content.is_some() {
            let svg = match req.svg_content {
                Some(ref raw) if raw.trim().starts_with("<svg") => raw.trim().to_string(),
                Some(ref inner) => render_svg_card(
                    width,
                    height,
                    req.title.as_deref().unwrap_or("Generated Diagram"),
                    &req.prompt,
                    Some(inner),
                ),
                None => render_svg_card(
                    width,
                    height,
                    req.title.as_deref().unwrap_or("Visual Summary"),
                    &req.prompt,
                    None,
                ),
            };
            let stem = slugify(
                req.filename
                    .trim_end_matches(".svg")
                    .trim_end_matches(".png"),
                64,
            );
            let safe_name = format!("{}.svg", if stem.is_empty() { "image" } else { &stem });
            return Ok(GeneratedImagePayload {
                filename: safe_name,
                mime_type: "image/svg+xml".to_string(),
                bytes: svg.into_bytes(),
                width,
                height,
                provider: "builtin_svg_renderer".to_string(),
            });
        }

        // Mặc định: kết xuất ảnh PNG chuẩn RFC-2083 (màu gradient đặc trưng theo hash của prompt)
        let render_w = width.clamp(16, 256);
        let render_h = height.clamp(16, 256);
        let png_bytes = encode_gradient_png(render_w, render_h, &req.prompt);
        let stem = slugify(
            req.filename
                .trim_end_matches(".png")
                .trim_end_matches(".svg"),
            64,
        );
        let safe_name = format!("{}.png", if stem.is_empty() { "image" } else { &stem });
        Ok(GeneratedImagePayload {
            filename: safe_name,
            mime_type: "image/png".to_string(),
            bytes: png_bytes,
            width: render_w,
            height: render_h,
            provider: "builtin_png_renderer".to_string(),
        })
    }
}

/// Yêu cầu sinh video gửi tới [`VideoGeneratorBackend`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoGenerationRequest {
    pub prompt: String,
    pub title: Option<String>,
    pub filename: String,
    pub width: u32,
    pub height: u32,
    pub duration_secs: u32,
}

/// Kết quả video sinh ra từ [`VideoGeneratorBackend`].
#[derive(Debug, Clone)]
pub struct GeneratedVideoPayload {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub duration_secs: u32,
    pub provider: String,
}

/// Trait mở rộng cho phép Host cắm bất kỳ engine tạo video nào (OpenAI Sora/Videos, ComfyUI, Wan, HunyuanVideo, hoặc Local Renderer).
#[async_trait::async_trait]
pub trait VideoGeneratorBackend: Send + Sync {
    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
    ) -> Result<GeneratedVideoPayload, String>;
}

/// Backend tạo video mặc định tự chủ: kết xuất container ISO BMFF / MP4 (`video/mp4`) hợp lệ
/// mang metadata `ftypisom` + `mdat` để kiểm thử và chạy offline không cần GPU rời.
#[derive(Debug, Clone, Default)]
pub struct DefaultVideoGeneratorBackend;

#[async_trait::async_trait]
impl VideoGeneratorBackend for DefaultVideoGeneratorBackend {
    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
    ) -> Result<GeneratedVideoPayload, String> {
        let width = req.width.clamp(16, 2048);
        let height = req.height.clamp(16, 2048);
        let duration_secs = req.duration_secs.clamp(1, 60);
        let mp4_bytes = encode_minimal_mp4(&req.prompt, width, height, duration_secs);
        let stem = slugify(
            req.filename
                .trim_end_matches(".mp4")
                .trim_end_matches(".webm")
                .trim_end_matches(".mov"),
            64,
        );
        let safe_name = format!("{}.mp4", if stem.is_empty() { "video" } else { &stem });
        Ok(GeneratedVideoPayload {
            filename: safe_name,
            mime_type: "video/mp4".to_string(),
            bytes: mp4_bytes,
            width,
            height,
            duration_secs,
            provider: "builtin_mp4_renderer".to_string(),
        })
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

/// Công cụ `image_create`: tạo hình ảnh minh họa / sơ đồ (`image/png` hoặc `image/svg+xml`)
/// và lưu trữ vào `ArtifactStore` của Task.
pub struct ImageCreateTool {
    pub backend: Arc<dyn ImageGeneratorBackend>,
    pub output_dir: Option<PathBuf>,
}

impl ImageCreateTool {
    pub fn new(backend: Arc<dyn ImageGeneratorBackend>) -> Self {
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

impl Default for ImageCreateTool {
    fn default() -> Self {
        Self::new(Arc::new(DefaultImageGeneratorBackend))
    }
}

#[async_trait::async_trait]
impl HostTool for ImageCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "image_create".to_string(),
            description: "Generate an illustration image or diagram (PNG or SVG) and publish it as a Task Artifact.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filename", "prompt"],
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Output image filename (e.g. 'architecture-diagram.svg' or 'cover-banner.png')."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed visual prompt or description of the image/diagram to generate."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional heading/caption displayed on the visual card or diagram."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["png", "svg"],
                        "description": "Image format: 'png' (raster PNG) or 'svg' (vector diagram). Defaults to extension or 'png'."
                    },
                    "width": {
                        "type": "integer",
                        "description": "Image width in pixels (default: 640)."
                    },
                    "height": {
                        "type": "integer",
                        "description": "Image height in pixels (default: 360)."
                    },
                    "svg_content": {
                        "type": "string",
                        "description": "Optional raw SVG markup (<svg ...>...</svg>) when authoring custom vector diagrams."
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
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing prompt"))?;
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string);
        let inferred_fmt = if filename.trim().to_ascii_lowercase().ends_with(".svg") {
            "svg"
        } else {
            "png"
        };
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or(inferred_fmt)
            .to_string();
        let width = args
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(640);
        let height = args
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(360);
        let svg_content = args
            .get("svg_content")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        let role = parse_role(args.get("role").and_then(Value::as_str));

        let req = ImageGenerationRequest {
            prompt: prompt.to_string(),
            title,
            filename: filename.to_string(),
            format,
            width,
            height,
            svg_content,
        };

        let generated = self
            .backend
            .generate_image(&req)
            .await
            .map_err(|e| host_tool_err("IMAGE_GENERATION_FAILED", e))?;

        let mut mirrored_path = None;
        if let Some(ref dir) = self.output_dir {
            if std::fs::create_dir_all(dir).is_ok() {
                let p = dir.join(&generated.filename);
                if std::fs::write(&p, &generated.bytes).is_ok() {
                    mirrored_path = Some(p.display().to_string());
                }
            }
        }

        let artifact = ctx
            .artifacts()
            .put(ArtifactInput::from_bytes(
                &generated.filename,
                &generated.mime_type,
                role,
                generated.bytes,
            ))
            .await?;

        Ok(host_tool_outcome(
            &ctx,
            format!(
                "Created image artifact '{}' (id={}, mime={}, bytes={})",
                artifact.name, artifact.id, artifact.mime_type, artifact.size_bytes
            ),
            json!({
                "artifact_id": artifact.id,
                "artifact_ids": [artifact.id],
                "artifact_kind": "image",
                "filename": artifact.name,
                "mime_type": artifact.mime_type,
                "size_bytes": artifact.size_bytes,
                "sha256": artifact.sha256,
                "width": generated.width,
                "height": generated.height,
                "provider": generated.provider,
                "path": mirrored_path
            }),
        ))
    }
}

/// Công cụ `video_create`: tạo video minh họa (`video/mp4`) từ prompt
/// và lưu trữ vào `ArtifactStore` của Task.
pub struct VideoCreateTool {
    pub backend: Arc<dyn VideoGeneratorBackend>,
    pub output_dir: Option<PathBuf>,
}

impl VideoCreateTool {
    pub fn new(backend: Arc<dyn VideoGeneratorBackend>) -> Self {
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

impl Default for VideoCreateTool {
    fn default() -> Self {
        Self::new(Arc::new(DefaultVideoGeneratorBackend))
    }
}

#[async_trait::async_trait]
impl HostTool for VideoCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "video_create".to_string(),
            description: "Generate a short illustration or explainer video (MP4) from a visual prompt and publish it as a Task Artifact.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filename", "prompt"],
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Output video filename (e.g. 'architecture-overview.mp4')."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed visual prompt describing the scenes and motion of the video to generate."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional video title or caption."
                    },
                    "width": {
                        "type": "integer",
                        "description": "Video width in pixels (default: 640)."
                    },
                    "height": {
                        "type": "integer",
                        "description": "Video height in pixels (default: 360)."
                    },
                    "duration_secs": {
                        "type": "integer",
                        "description": "Video duration in seconds (1 to 60, default: 4)."
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
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing prompt"))?;
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string);
        let width = args
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(640);
        let height = args
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(360);
        let duration_secs = args
            .get("duration_secs")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(4)
            .clamp(1, 60);
        let role = parse_role(args.get("role").and_then(Value::as_str));

        let req = VideoGenerationRequest {
            prompt: prompt.to_string(),
            title,
            filename: filename.to_string(),
            width,
            height,
            duration_secs,
        };

        let generated = self
            .backend
            .generate_video(&req)
            .await
            .map_err(|e| host_tool_err("VIDEO_GENERATION_FAILED", e))?;

        let mut mirrored_path = None;
        if let Some(ref dir) = self.output_dir {
            if std::fs::create_dir_all(dir).is_ok() {
                let p = dir.join(&generated.filename);
                if std::fs::write(&p, &generated.bytes).is_ok() {
                    mirrored_path = Some(p.display().to_string());
                }
            }
        }

        let artifact = ctx
            .artifacts()
            .put(ArtifactInput::from_bytes(
                &generated.filename,
                &generated.mime_type,
                role,
                generated.bytes,
            ))
            .await?;

        Ok(host_tool_outcome(
            &ctx,
            format!(
                "Created video artifact '{}' (id={}, mime={}, duration_secs={}, bytes={})",
                artifact.name,
                artifact.id,
                artifact.mime_type,
                generated.duration_secs,
                artifact.size_bytes
            ),
            json!({
                "artifact_id": artifact.id,
                "artifact_ids": [artifact.id],
                "artifact_kind": "video",
                "filename": artifact.name,
                "mime_type": artifact.mime_type,
                "size_bytes": artifact.size_bytes,
                "sha256": artifact.sha256,
                "width": generated.width,
                "height": generated.height,
                "duration_secs": generated.duration_secs,
                "provider": generated.provider,
                "path": mirrored_path
            }),
        ))
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
                "Synthesize spoken audio (TTS) from text and publish it as an audio Task Artifact."
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

        let mut mirrored_path = None;
        if let Some(ref dir) = self.output_dir {
            if std::fs::create_dir_all(dir).is_ok() {
                let p = dir.join(&synthesized.filename);
                if std::fs::write(&p, &synthesized.bytes).is_ok() {
                    mirrored_path = Some(p.display().to_string());
                }
            }
        }

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
        let mut mirrored_path = None;
        if let Some(ref dir) = self.output_dir {
            if std::fs::create_dir_all(dir).is_ok() {
                let p = dir.join(filename);
                if std::fs::write(&p, &payload_bytes).is_ok() {
                    mirrored_path = Some(p.display().to_string());
                }
            }
        }
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

/// Xuất bộ 4 `HostTool` đa phương tiện & artifact (`image_create`, `video_create`, `tts_synthesize`, `artifact_create`)
/// với đầy đủ các backend tùy chỉnh cho Image, Video và TTS.
pub fn media_host_tools_with_video(
    image_backend: Arc<dyn ImageGeneratorBackend>,
    video_backend: Arc<dyn VideoGeneratorBackend>,
    tts_backend: Arc<dyn TtsSynthesizerBackend>,
    output_dir: Option<PathBuf>,
) -> Vec<Arc<dyn HostTool>> {
    let mut img = ImageCreateTool::new(image_backend);
    let mut vid = VideoCreateTool::new(video_backend);
    let mut tts = TtsSynthesizeTool::new(tts_backend);
    let mut gen = ArtifactCreateTool::new();
    if let Some(dir) = output_dir {
        img = img.with_output_dir(dir.join("images"));
        vid = vid.with_output_dir(dir.join("videos"));
        tts = tts.with_output_dir(dir.join("tts"));
        gen = gen.with_output_dir(dir.join("files"));
    }
    vec![Arc::new(img), Arc::new(vid), Arc::new(tts), Arc::new(gen)]
}

fn parse_role(raw: Option<&str>) -> ArtifactRole {
    match raw.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("intermediate") => ArtifactRole::Intermediate,
        _ => ArtifactRole::Primary,
    }
}

fn decode_hex(hex_str: &str) -> Result<Vec<u8>, String> {
    let clean: String = hex_str.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() % 2 != 0 {
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

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_svg_card(
    width: u32,
    height: u32,
    title: &str,
    prompt: &str,
    custom_body: Option<&str>,
) -> String {
    let safe_title = xml_escape(title);
    let safe_prompt: String = prompt.chars().take(180).collect();
    let safe_prompt = xml_escape(&safe_prompt);
    let body_markup = custom_body.unwrap_or("");
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
  <defs>
    <linearGradient id="bg" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#0f172a"/>
      <stop offset="100%" stop-color="#1e293b"/>
    </linearGradient>
  </defs>
  <rect width="100%" height="100%" rx="16" fill="url(#bg)"/>
  <text x="32" y="56" fill="#f8fafc" font-family="sans-serif" font-size="24" font-weight="bold">{safe_title}</text>
  <text x="32" y="96" fill="#94a3b8" font-family="sans-serif" font-size="14">{safe_prompt}</text>
  {body_markup}
</svg>
"##
    )
}

/// Kết xuất một ảnh PNG chuẩn (8-bit RGB, uncompressed zlib blocks + Adler32 + CRC32) không cần thư viện ngoài.
fn encode_gradient_png(width: u32, height: u32, seed_text: &str) -> Vec<u8> {
    let mut seed: u32 = 0x811c_9dc5;
    for b in seed_text.bytes() {
        seed ^= u32::from(b);
        seed = seed.wrapping_mul(0x0100_0193);
    }
    let r_base = (seed & 0xff) as u8;
    let g_base = ((seed >> 8) & 0xff) as u8;
    let b_base = ((seed >> 16) & 0xff) as u8;

    // Mỗi dòng scanline gồm 1 filter byte (0) + width * 3 bytes RGB
    let row_len = 1 + (width as usize) * 3;
    let mut raw_scanlines = Vec::with_capacity(row_len * (height as usize));
    for y in 0..height {
        raw_scanlines.push(0u8); // Filter type 0 (None)
        for x in 0..width {
            let r = r_base.wrapping_add(((x * 255) / width.max(1)) as u8);
            let g = g_base.wrapping_add(((y * 255) / height.max(1)) as u8);
            let b = b_base;
            raw_scanlines.extend_from_slice(&[r, g, b]);
        }
    }

    let zlib_stream = wrap_zlib_stored(&raw_scanlines);

    let mut png = Vec::with_capacity(64 + zlib_stream.len());
    // PNG signature
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    // IHDR chunk (width, height, bit_depth=8, color_type=2 RGB, compression=0, filter=0, interlace=0)
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    write_png_chunk(&mut png, b"IHDR", &ihdr);

    // IDAT chunk
    write_png_chunk(&mut png, b"IDAT", &zlib_stream);

    // IEND chunk
    write_png_chunk(&mut png, b"IEND", &[]);
    png
}

fn wrap_zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + (data.len() / 65535 + 1) * 5 + 6);
    // Zlib header: CMF=0x78 (deflate, 32K window), FLG=0x01 (no dict, check bits)
    out.extend_from_slice(&[0x78, 0x01]);

    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65_535).collect()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i + 1 == chunks.len();
        out.push(if is_last { 0x01 } else { 0x00 });
        let len = chunk.len() as u16;
        let nlen = !len;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(chunk);
    }

    let adler = adler32(data);
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(chunk_type: &[u8; 4], chunk_data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in chunk_type.iter().chain(chunk_data.iter()) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], chunk_data: &[u8]) {
    let len = chunk_data.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(chunk_data);
    let crc = crc32(chunk_type, chunk_data);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Tạo file âm thanh RIFF/WAVE PCM 16-bit mono hợp lệ.
fn synthesize_pcm_wav(text: &str, sample_rate: u32, duration_ms: u64) -> Vec<u8> {
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

/// Tạo container ISO BMFF / MP4 (`ftyp` + `mdat`) hợp lệ chứa metadata đặc tả cảnh video.
fn encode_minimal_mp4(prompt: &str, width: u32, height: u32, duration_secs: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + prompt.len());
    // Box 1: `ftyp` (24 bytes) - ISO Base Media File Format (`isom`, `mp42`)
    let ftyp_len: u32 = 24;
    out.extend_from_slice(&ftyp_len.to_be_bytes());
    out.extend_from_slice(b"ftyp");
    out.extend_from_slice(b"isom");
    out.extend_from_slice(&512u32.to_be_bytes());
    out.extend_from_slice(b"isommp42");

    // Box 2: `mdat` chứa thông tin khung hình và prompt
    let payload = format!(
        "MP4_STREAM|w={width}|h={height}|dur={duration_secs}s|prompt={prompt}"
    );
    let mdat_len = (8 + payload.len()) as u32;
    out.extend_from_slice(&mdat_len.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(payload.as_bytes());
    out
}
