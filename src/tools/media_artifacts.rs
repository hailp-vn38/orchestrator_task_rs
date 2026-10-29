//! Bộ công cụ `HostTool` đa phương tiện & đa định dạng cho Subagent (`image_create`, `video_create`, `tts_synthesize`, `artifact_create`)
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

/// Ảnh tham chiếu đã được xác minh toàn vẹn để chuyển xuống Backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceImageInput {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

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
    #[serde(default)]
    pub reference_images: Vec<ReferenceImageInput>,
    #[serde(default)]
    pub transparent_background: bool,
}

impl ImageGenerationRequest {
    /// Kiểm tra yêu cầu này có xuất ra định dạng vector SVG (`image/svg+xml`) hay không.
    pub fn is_svg(&self) -> bool {
        self.format.trim().eq_ignore_ascii_case("svg")
            || self
                .svg_content
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }

    /// Trả về kích thước hiệu lực `(width, height)` đã chuẩn hóa trong khoảng `16..=2048`:
    /// - Nếu `width` / `height` khác `0`, giữ giá trị do Agent chỉ định (clamp `16..=2048`).
    /// - Nếu là SVG có truyền `<svg ...>` đầy đủ trong `svg_content` và `width == 0` hoặc `height == 0`,
    ///   ưu tiên trích xuất kích thước từ thuộc tính `width`/`height` hoặc `viewBox` của thẻ `<svg>` gốc.
    /// - Ngược lại, dùng mặc định theo định dạng (`1200x675` cho SVG, `1024x1024` cho PNG).
    pub fn effective_dimensions(&self) -> (u32, u32) {
        let is_svg = self.is_svg();
        let svg_dims = if is_svg {
            self.svg_content
                .as_deref()
                .and_then(extract_svg_root_dimensions)
        } else {
            None
        };
        let (default_w, default_h) = if is_svg {
            svg_dims.unwrap_or((DEFAULT_SVG_WIDTH, DEFAULT_SVG_HEIGHT))
        } else {
            (DEFAULT_PNG_WIDTH, DEFAULT_PNG_HEIGHT)
        };
        let width = if self.width == 0 {
            default_w
        } else {
            self.width
        }
        .clamp(16, 2048);
        let height = if self.height == 0 {
            default_h
        } else {
            self.height
        }
        .clamp(16, 2048);
        (width, height)
    }
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
        let (width, height) = req.effective_dimensions();

        if req.is_svg() {
            let (svg, final_w, final_h) = match req.svg_content {
                Some(ref raw)
                    if raw
                        .trim()
                        .get(..4)
                        .is_some_and(|p| p.eq_ignore_ascii_case("<svg")) =>
                {
                    ensure_svg_root_dimensions(raw.trim(), width, height)
                }
                Some(ref inner) => (
                    render_svg_card(
                        width,
                        height,
                        req.title.as_deref().unwrap_or("Generated Diagram"),
                        &req.prompt,
                        Some(inner),
                    ),
                    width,
                    height,
                ),
                None => (
                    render_svg_card(
                        width,
                        height,
                        req.title.as_deref().unwrap_or("Visual Summary"),
                        &req.prompt,
                        None,
                    ),
                    width,
                    height,
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
                width: final_w,
                height: final_h,
                provider: "builtin_svg_renderer".to_string(),
            });
        }

        // Mặc định: kết xuất ảnh PNG chuẩn RFC-2083 với đúng kích thước (width, height) trong IHDR
        let png_bytes = encode_gradient_png(width, height, &req.prompt);
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
            width,
            height,
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
    #[serde(default)]
    pub first_frame: Option<ReferenceImageInput>,
}

impl VideoGenerationRequest {
    /// Trả về kích thước hiệu lực `(width, height)` đã chuẩn hóa trong khoảng `16..=2048`
    /// (mặc định `640x360` khi `width == 0` hoặc `height == 0`).
    pub fn effective_dimensions(&self) -> (u32, u32) {
        let width = if self.width == 0 {
            DEFAULT_VIDEO_WIDTH
        } else {
            self.width
        }
        .clamp(16, 2048);
        let height = if self.height == 0 {
            DEFAULT_VIDEO_HEIGHT
        } else {
            self.height
        }
        .clamp(16, 2048);
        (width, height)
    }
}

/// Giới hạn thời lượng và số lượng clip theo provider hiện hành của công cụ `video_create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoProviderConstraints {
    /// Nhãn hiển thị của provider/adapter đang kích hoạt (ví dụ: `"Muse Web Gateway (model: muse-video)"`).
    pub provider_label: String,
    /// Thời lượng tối đa (giây) của MỘT video clip đơn lẻ do provider tạo ra.
    pub max_clip_duration_secs: u32,
    /// Số lượng video clip tối đa mà provider có thể sinh ra trong MỘT lần gọi `video_create`.
    pub max_clips_per_call: u32,
    /// Tổng thời lượng tối đa (giây) có thể yêu cầu trong một lần gọi `video_create`.
    pub max_total_duration_secs: u32,
    /// Hướng dẫn điều phối cụ thể cho Agent khi dùng provider này.
    pub provider_guidance: String,
}

impl Default for VideoProviderConstraints {
    fn default() -> Self {
        Self {
            provider_label: "builtin_mp4_renderer".to_string(),
            max_clip_duration_secs: 60,
            max_clips_per_call: 1,
            max_total_duration_secs: 60,
            provider_guidance: "Creates a single MP4 clip up to 60 seconds per call.".to_string(),
        }
    }
}

/// Một phân cảnh video MP4 đơn lẻ do provider trả về.
#[derive(Debug, Clone)]
pub struct GeneratedVideoClip {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
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
    /// Danh sách toàn bộ các clip phân cảnh (`1..=N`) nếu provider trả về nhiều clip trong 1 lần gọi.
    pub clips: Vec<GeneratedVideoClip>,
    /// Toàn bộ tin nhắn kịch bản/phân cảnh do provider trả về kèm theo video.
    pub provider_message: Option<String>,
}

impl GeneratedVideoPayload {
    /// Tiêu thụ trực tiếp danh sách clip từ payload mà không sao chép lại các bộ đệm MP4 đa-MB.
    pub fn take_effective_clips(&mut self) -> Vec<GeneratedVideoClip> {
        if !self.clips.is_empty() {
            return std::mem::take(&mut self.clips);
        }
        let bytes = std::mem::take(&mut self.bytes);
        let duration_secs =
            inspect_mp4_duration_secs(&bytes).unwrap_or_else(|| self.duration_secs.max(1));
        vec![GeneratedVideoClip {
            filename: self.filename.clone(),
            mime_type: self.mime_type.clone(),
            bytes,
            width: self.width,
            height: self.height,
            duration_secs,
        }]
    }
}

/// Chuẩn hóa tên file gốc của video (cắt hậu tố `.mp4`/`.webm`/`.mov` và `slugify` tối đa 64 ký tự).
pub fn normalize_video_stem(filename: &str, fallback: &str) -> String {
    let stem = slugify(
        filename
            .trim_end_matches(".mp4")
            .trim_end_matches(".webm")
            .trim_end_matches(".mov"),
        64,
    );
    if stem.is_empty() {
        fallback.to_string()
    } else {
        stem
    }
}

fn mirror_artifact_to_dir(
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

/// Trait mở rộng cho phép Host cắm bất kỳ engine tạo video nào (OpenAI Sora/Videos, Muse, ComfyUI, Wan, HunyuanVideo, hoặc Local Renderer).
#[async_trait::async_trait]
pub trait VideoGeneratorBackend: Send + Sync {
    fn video_constraints(&self) -> VideoProviderConstraints {
        VideoProviderConstraints::default()
    }

    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
    ) -> Result<GeneratedVideoPayload, String>;
}

/// Backend tạo video mặc định tự chủ: kết xuất container ISO BMFF / MP4 (`video/mp4`) hợp lệ
/// mang metadata `ftypisom` + `moov/mvhd` + `mdat` để kiểm thử và chạy offline không cần GPU rời.
#[derive(Debug, Clone, Default)]
pub struct DefaultVideoGeneratorBackend;

#[async_trait::async_trait]
impl VideoGeneratorBackend for DefaultVideoGeneratorBackend {
    async fn generate_video(
        &self,
        req: &VideoGenerationRequest,
    ) -> Result<GeneratedVideoPayload, String> {
        let (width, height) = req.effective_dimensions();
        let duration_secs = req.duration_secs.clamp(1, 60);
        let mp4_bytes = encode_minimal_mp4(&req.prompt, width, height, duration_secs);
        let safe_name = format!("{}.mp4", normalize_video_stem(&req.filename, "video"));
        Ok(GeneratedVideoPayload {
            filename: safe_name,
            mime_type: "video/mp4".to_string(),
            bytes: mp4_bytes,
            width,
            height,
            duration_secs,
            provider: "builtin_mp4_renderer".to_string(),
            clips: Vec::new(),
            provider_message: None,
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
                        "description": "Optional image width in pixels (16..=2048). Agent may set custom width or omit to use format default (1024 for PNG, 1200 for SVG)."
                    },
                    "height": {
                        "type": "integer",
                        "description": "Optional image height in pixels (16..=2048). Agent may set custom height or omit to use format default (1024 for PNG, 675 for SVG)."
                    },
                    "svg_content": {
                        "type": "string",
                        "description": "Optional raw SVG markup (<svg ...>...</svg>) when authoring custom vector diagrams."
                    },
                    "reference_artifact_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": 4,
                        "description": "Optional list of up to 4 image Artifact IDs in the current task to use as visual references."
                    },
                    "reference_images": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": 4,
                        "description": "Optional list of up to 4 reference images as data:image/...;base64,... strings."
                    },
                    "transparent_background": {
                        "type": "boolean",
                        "description": "Optional flag to request an isolated subject with a transparent PNG background."
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
        Some(IMAGE_CREATE_TIMEOUT)
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
        let svg_content = args
            .get("svg_content")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        let inferred_fmt =
            if filename.trim().to_ascii_lowercase().ends_with(".svg") || svg_content.is_some() {
                "svg"
            } else {
                "png"
            };
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(inferred_fmt)
            .to_ascii_lowercase();
        let raw_width = args
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let raw_height = args
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let transparent_background = args
            .get("transparent_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let role = parse_role(args.get("role").and_then(Value::as_str));
        let reference_images = resolve_image_references_from_args(Some(&ctx), &args).await?;

        let mut req = ImageGenerationRequest {
            prompt: prompt.to_string(),
            title,
            filename: filename.to_string(),
            format,
            width: raw_width,
            height: raw_height,
            svg_content,
            reference_images,
            transparent_background,
        };
        let (width, height) = req.effective_dimensions();
        req.width = width;
        req.height = height;

        let generated = self
            .backend
            .generate_image(&req)
            .await
            .map_err(|e| host_tool_err("IMAGE_GENERATION_FAILED", e))?;

        let mirrored_path = mirror_artifact_to_dir(
            self.output_dir.as_ref(),
            &generated.filename,
            &generated.bytes,
        );

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
                "Created image artifact '{}' (id={}, mime={}, width={}, height={}, provider={}, bytes={})",
                artifact.name,
                artifact.id,
                artifact.mime_type,
                generated.width,
                generated.height,
                generated.provider,
                artifact.size_bytes
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
        let c = self.backend.video_constraints();
        let max_total = c.max_total_duration_secs.max(1);
        let max_clip = c.max_clip_duration_secs.max(1);
        let max_clips = c.max_clips_per_call.max(1);

        ToolDescriptor {
            name: "video_create".to_string(),
            description: format!(
                "Generate an illustration or explainer video (MP4) with built-in synchronized audio/voiceover narration and burned-in subtitles directly from `prompt` (do NOT call `tts_synthesize` for video requests). \
                 [Active provider: {label} | max_clip_duration_secs={max_clip}s | max_clips_per_call={max_clips} | max_total_duration_secs={max_total}s. {guidance}]",
                label = c.provider_label,
                guidance = c.provider_guidance,
            ),
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
                        "description": "Optional video width in pixels (16..=2048, default: 640)."
                    },
                    "height": {
                        "type": "integer",
                        "description": "Optional video height in pixels (16..=2048, default: 360)."
                    },
                    "duration_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": max_total,
                        "description": format!(
                            "Video duration in seconds (1 to {max_total}, default: 4; active provider '{}' generates up to {max_clip}s per single clip and up to {max_clips} clip(s) per call).",
                            c.provider_label
                        )
                    },
                    "first_frame_artifact_id": {
                        "type": "string",
                        "description": "Optional image Artifact ID in the current task to use as the starting frame of the video."
                    },
                    "first_frame_image": {
                        "type": "string",
                        "description": "Optional starting frame image as a data:image/...;base64,... string."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["primary", "intermediate"],
                        "description": "Artifact visibility role (default: 'primary' for single-clip output; multi-clip parts are stored as 'intermediate' until merged with `video_concat`)."
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::Mutating,
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(VIDEO_CREATE_TIMEOUT)
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
        let raw_width = args
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let raw_height = args
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let constraints = self.backend.video_constraints();
        let duration_secs = args
            .get("duration_secs")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(4)
            .clamp(1, constraints.max_total_duration_secs.max(1));
        let requested_role = parse_role(args.get("role").and_then(Value::as_str));
        let first_frame = resolve_first_frame_from_args(Some(&ctx), &args).await?;

        let mut req = VideoGenerationRequest {
            prompt: prompt.to_string(),
            title,
            filename: filename.to_string(),
            width: raw_width,
            height: raw_height,
            duration_secs,
            first_frame,
        };
        let (width, height) = req.effective_dimensions();
        req.width = width;
        req.height = height;

        let mut generated = self
            .backend
            .generate_video(&req)
            .await
            .map_err(|e| host_tool_err("VIDEO_GENERATION_FAILED", e))?;

        let clips = generated.take_effective_clips();
        let is_multi_clip = clips.len() > 1;
        let safe_stem = normalize_video_stem(filename, "video");

        let mut artifact_ids = Vec::with_capacity(clips.len());
        let mut clip_entries = Vec::with_capacity(clips.len());
        let mut total_duration_secs: u32 = 0;
        let mut first_artifact = None;
        let mut first_mirrored_path = None;

        for (idx, clip) in clips.into_iter().enumerate() {
            let clip_filename = if is_multi_clip {
                format!("{safe_stem}-part-{}.mp4", idx + 1)
            } else if clip.filename.trim().is_empty() {
                format!("{safe_stem}.mp4")
            } else {
                clip.filename
            };
            let clip_role = if is_multi_clip {
                ArtifactRole::Intermediate
            } else {
                requested_role
            };
            let clip_duration =
                inspect_mp4_duration_secs(&clip.bytes).unwrap_or_else(|| clip.duration_secs.max(1));
            total_duration_secs = total_duration_secs.saturating_add(clip_duration);

            let mirrored_path =
                mirror_artifact_to_dir(self.output_dir.as_ref(), &clip_filename, &clip.bytes);
            if idx == 0 {
                first_mirrored_path = mirrored_path.clone();
            }

            let artifact = ctx
                .artifacts()
                .put(ArtifactInput::from_bytes(
                    &clip_filename,
                    &clip.mime_type,
                    clip_role,
                    clip.bytes,
                ))
                .await?;

            artifact_ids.push(artifact.id.clone());
            clip_entries.push(json!({
                "index": idx + 1,
                "artifact_id": artifact.id,
                "filename": artifact.name,
                "mime_type": artifact.mime_type,
                "size_bytes": artifact.size_bytes,
                "sha256": artifact.sha256,
                "width": clip.width,
                "height": clip.height,
                "duration_secs": clip_duration,
                "role": if clip_role == ArtifactRole::Intermediate { "intermediate" } else { "primary" },
                "path": mirrored_path,
            }));
            if idx == 0 {
                first_artifact = Some(artifact);
            }
        }

        let primary = first_artifact
            .ok_or_else(|| host_tool_err("VIDEO_GENERATION_FAILED", "No video clips returned"))?;
        let msg_suffix = generated
            .provider_message
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|m| format!("\nProvider message:\n{m}"))
            .unwrap_or_default();
        let summary = if is_multi_clip {
            format!(
                "Created {} intermediate video clip artifacts ({}s total, requested {}s): {:?}. Call `video_concat` with `video_artifact_ids` in order to merge them into a single primary MP4 video artifact.{msg_suffix}",
                clip_entries.len(),
                total_duration_secs,
                duration_secs,
                artifact_ids
            )
        } else {
            format!(
                "Created video artifact '{}' (id={}, mime={}, duration_secs={}, bytes={}){msg_suffix}",
                primary.name,
                primary.id,
                primary.mime_type,
                total_duration_secs,
                primary.size_bytes
            )
        };

        Ok(host_tool_outcome(
            &ctx,
            summary,
            json!({
                "artifact_id": primary.id,
                "artifact_ids": artifact_ids,
                "artifact_kind": "video",
                "filename": primary.name,
                "mime_type": primary.mime_type,
                "size_bytes": primary.size_bytes,
                "sha256": primary.sha256,
                "width": generated.width,
                "height": generated.height,
                "duration_secs": total_duration_secs,
                "total_duration_secs": total_duration_secs,
                "requested_duration_secs": duration_secs,
                "clip_count": clip_entries.len(),
                "clips": clip_entries,
                "requires_concat": is_multi_clip,
                "provider_message": generated.provider_message,
                "provider": generated.provider,
                "path": first_mirrored_path
            }),
        ))
    }
}

/// Tên công cụ ghép nối các `Task Artifact` video MP4 thành một file MP4 liên tục.
pub const VIDEO_CONCAT_TOOL: &str = "video_concat";

/// Một clip video đầu vào đã được xác thực toàn vẹn SHA-256 từ `ArtifactStore` để chuyển xuống [`VideoConcatBackend`].
#[derive(Debug, Clone)]
pub struct VideoConcatInputClip {
    pub artifact_id: String,
    pub filename: String,
    pub bytes: Vec<u8>,
    pub duration_secs: u32,
}

/// Yêu cầu ghép nối nhiều clip video gửi tới [`VideoConcatBackend`].
#[derive(Debug, Clone)]
pub struct VideoConcatRequest {
    pub filename: String,
    pub title: Option<String>,
    pub clips: Vec<VideoConcatInputClip>,
}

/// Kết quả video MP4 sau khi ghép nối từ [`VideoConcatBackend`].
#[derive(Debug, Clone)]
pub struct ConcatenatedVideoPayload {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub duration_secs: u32,
}

/// Seam mở rộng cho phép ghép nối nhiều clip video MP4 thành một file MP4 hoàn chỉnh.
#[async_trait::async_trait]
pub trait VideoConcatBackend: Send + Sync {
    async fn concat_videos(
        &self,
        req: &VideoConcatRequest,
    ) -> Result<ConcatenatedVideoPayload, String>;
}

/// Thực thi mặc định cho [`VideoConcatBackend`]:
/// - Hỗ trợ ghép in-process cho container ISO BMFF `MP4_STREAM|` của [`DefaultVideoGeneratorBackend`].
/// - Sử dụng `ffmpeg -f concat -safe 0 -c copy -movflags +faststart` (kèm fallback chuẩn hóa codec) cho stream video H.264/AAC thực tế.
#[derive(Debug, Clone, Default)]
pub struct DefaultVideoConcatBackend;

struct TempConcatDir(PathBuf);

impl TempConcatDir {
    fn create() -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("orch_video_concat_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create temp concat dir: {e}"))?;
        Ok(Self(dir))
    }
}

impl Drop for TempConcatDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[async_trait::async_trait]
impl VideoConcatBackend for DefaultVideoConcatBackend {
    async fn concat_videos(
        &self,
        req: &VideoConcatRequest,
    ) -> Result<ConcatenatedVideoPayload, String> {
        if req.clips.len() < 2 {
            return Err("At least 2 video clips are required for concatenation".to_string());
        }
        let safe_name = format!(
            "{}.mp4",
            normalize_video_stem(&req.filename, "video-merged")
        );
        let sum_duration: u32 = req
            .clips
            .iter()
            .map(|c| c.duration_secs.max(1))
            .sum::<u32>()
            .max(1);

        // 1. Nhánh in-process cho container của `DefaultVideoGeneratorBackend` (`mdat` bắt đầu bằng `MP4_STREAM|`)
        let synthetic_payloads: Option<Vec<&str>> = req
            .clips
            .iter()
            .map(|c| {
                find_iso_bmff_box(&c.bytes, b"mdat")
                    .and_then(|mdat| std::str::from_utf8(mdat).ok())
                    .filter(|s| s.starts_with("MP4_STREAM|"))
            })
            .collect();

        if let Some(streams) = synthetic_payloads {
            let mut width = DEFAULT_VIDEO_WIDTH;
            let mut height = DEFAULT_VIDEO_HEIGHT;
            let mut scenes = Vec::with_capacity(streams.len());
            for (idx, stream) in streams.into_iter().enumerate() {
                for part in stream.split('|') {
                    if idx == 0 {
                        if let Some(w) = part.strip_prefix("w=").and_then(|v| v.parse::<u32>().ok())
                        {
                            width = w;
                        } else if let Some(h) =
                            part.strip_prefix("h=").and_then(|v| v.parse::<u32>().ok())
                        {
                            height = h;
                        }
                    }
                    if let Some(p) = part.strip_prefix("prompt=") {
                        scenes.push(p.to_string());
                    }
                }
            }
            let merged_prompt = if scenes.is_empty() {
                format!("concat({} clips)", req.clips.len())
            } else {
                scenes.join(" + ")
            };
            let bytes = encode_minimal_mp4(&merged_prompt, width, height, sum_duration);
            return Ok(ConcatenatedVideoPayload {
                filename: safe_name,
                mime_type: "video/mp4".to_string(),
                bytes,
                duration_secs: sum_duration,
            });
        }

        // 2. Nhánh video MP4 thực tế qua `ffmpeg`
        let temp_dir = TempConcatDir::create()?;
        let mut list_content = String::new();
        let mut clip_paths = Vec::with_capacity(req.clips.len());
        for (idx, clip) in req.clips.iter().enumerate() {
            let file_name = format!("clip_{idx:03}.mp4");
            let clip_path = temp_dir.0.join(&file_name);
            std::fs::write(&clip_path, &clip.bytes)
                .map_err(|e| format!("failed to write temp clip {file_name}: {e}"))?;
            list_content.push_str(&format!("file '{file_name}'\n"));
            clip_paths.push(clip_path);
        }
        let list_path = temp_dir.0.join("concat.txt");
        std::fs::write(&list_path, &list_content)
            .map_err(|e| format!("failed to write concat.txt: {e}"))?;
        let merged_path = temp_dir.0.join("merged.mp4");

        let copy_status = tokio::process::Command::new("ffmpeg")
            .current_dir(&temp_dir.0)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                "concat.txt",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "merged.mp4",
            ])
            .output()
            .await
            .map_err(|e| format!("failed to invoke ffmpeg for video_concat: {e}"))?;

        if !copy_status.status.success() || !merged_path.exists() {
            // Fallback: re-encode sang chuẩn H.264 + AAC đồng nhất nếu stream copy không tương thích
            let reencode = tokio::process::Command::new("ffmpeg")
                .current_dir(&temp_dir.0)
                .args([
                    "-y",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "concat",
                    "-safe",
                    "0",
                    "-i",
                    "concat.txt",
                    "-vf",
                    "scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1",
                    "-r",
                    "30",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-movflags",
                    "+faststart",
                    "merged.mp4",
                ])
                .output()
                .await
                .map_err(|e| format!("failed to invoke ffmpeg fallback for video_concat: {e}"))?;

            if !reencode.status.success() || !merged_path.exists() {
                let stderr = String::from_utf8_lossy(&reencode.stderr);
                return Err(format!("ffmpeg video_concat failed: {}", stderr.trim()));
            }
        }

        let merged_bytes = std::fs::read(&merged_path)
            .map_err(|e| format!("failed to read merged MP4 output: {e}"))?;
        let actual_duration = inspect_mp4_duration_secs(&merged_bytes).unwrap_or(sum_duration);

        Ok(ConcatenatedVideoPayload {
            filename: safe_name,
            mime_type: "video/mp4".to_string(),
            bytes: merged_bytes,
            duration_secs: actual_duration,
        })
    }
}

/// Công cụ `video_concat`: ghép nhiều `Task Artifact` video MP4 thành một file MP4 hoàn chỉnh (`role: "primary"`).
pub struct VideoConcatTool {
    pub backend: Arc<dyn VideoConcatBackend>,
    pub output_dir: Option<PathBuf>,
}

impl VideoConcatTool {
    pub fn new(backend: Arc<dyn VideoConcatBackend>) -> Self {
        Self {
            backend,
            output_dir: None,
        }
    }

    pub fn with_default_backend() -> Self {
        Self::new(Arc::new(DefaultVideoConcatBackend))
    }

    pub fn with_output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

impl Default for VideoConcatTool {
    fn default() -> Self {
        Self::with_default_backend()
    }
}

#[async_trait::async_trait]
impl HostTool for VideoConcatTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: VIDEO_CONCAT_TOOL.to_string(),
            description: "Concatenate 2 or more sequential MP4 video clip artifacts (`video_artifact_ids`) in order into a single continuous primary MP4 video artifact. Use `video_concat` when merging user-attached `.mp4` Input Artifacts or when `video_create` produces multiple clips (`clip_count > 1` / `requires_concat: true`).".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filename", "video_artifact_ids"],
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Output merged MP4 video filename (e.g. 'full-overview.mp4')."
                    },
                    "video_artifact_ids": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 24,
                        "items": { "type": "string" },
                        "description": "Ordered list of at least 2 MP4 video Artifact IDs in the current task to concatenate."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional video title or caption for the merged video."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["primary", "intermediate"],
                        "description": "Artifact visibility role for the merged video (default: 'primary')."
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::Mutating,
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(VIDEO_CONCAT_TIMEOUT)
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let filename = args
            .get("filename")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let raw_ids = args
            .get("video_artifact_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing video_artifact_ids array"))?;
        if raw_ids.len() < 2 {
            return Err(host_tool_err(
                "INVALID_ARGUMENT",
                "video_artifact_ids must contain at least 2 Artifact IDs",
            ));
        }
        if raw_ids.len() > 24 {
            return Err(host_tool_err(
                "INVALID_ARGUMENT",
                "video_artifact_ids cannot exceed 24 Artifact IDs",
            ));
        }
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string);
        let role = parse_role(args.get("role").and_then(Value::as_str));

        let mut clips = Vec::with_capacity(raw_ids.len());
        let mut source_ids = Vec::with_capacity(raw_ids.len());
        for item in raw_ids {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    host_tool_err(
                        "INVALID_ARGUMENT",
                        "video_artifact_ids entries must be non-empty strings",
                    )
                })?;
            let reader = ctx.artifacts().open(id).await?;
            let meta = reader.metadata();
            if !meta.mime_type.eq_ignore_ascii_case("video/mp4")
                && !meta.mime_type.to_ascii_lowercase().starts_with("video/")
            {
                return Err(host_tool_err(
                    "UNSUPPORTED_VIDEO_ARTIFACT",
                    format!(
                        "Artifact '{}' has mime_type '{}', expected 'video/mp4'",
                        id, meta.mime_type
                    ),
                ));
            }
            let clip_name = meta.name.clone();
            let bytes = reader
                .read_all()
                .await
                .map_err(|e| host_tool_err("ARTIFACT_INTEGRITY", e.to_string()))?;
            let duration_secs = inspect_mp4_duration_secs(&bytes).unwrap_or(5);
            source_ids.push(id.to_string());
            clips.push(VideoConcatInputClip {
                artifact_id: id.to_string(),
                filename: clip_name,
                bytes,
                duration_secs,
            });
        }

        let req = VideoConcatRequest {
            filename: filename.to_string(),
            title,
            clips,
        };
        let merged = self
            .backend
            .concat_videos(&req)
            .await
            .map_err(|e| host_tool_err("VIDEO_CONCAT_FAILED", e))?;

        let mirrored_path =
            mirror_artifact_to_dir(self.output_dir.as_ref(), &merged.filename, &merged.bytes);

        let artifact = ctx
            .artifacts()
            .put(ArtifactInput::from_bytes(
                &merged.filename,
                &merged.mime_type,
                role,
                merged.bytes,
            ))
            .await?;

        Ok(host_tool_outcome(
            &ctx,
            format!(
                "Concatenated {} video clips into '{}' (id={}, duration_secs={}, bytes={})",
                source_ids.len(),
                artifact.name,
                artifact.id,
                merged.duration_secs,
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
                "duration_secs": merged.duration_secs,
                "clip_count": source_ids.len(),
                "source_artifact_ids": source_ids,
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

fn parse_role(raw: Option<&str>) -> ArtifactRole {
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

pub(crate) async fn resolve_image_references_from_args(
    ctx: Option<&ToolContext>,
    args: &Value,
) -> Result<Vec<ReferenceImageInput>, ToolError> {
    let mut out = Vec::new();

    if let Some(arr) = args.get("reference_artifact_ids").and_then(Value::as_array) {
        if arr.len() > MAX_IMAGE_REFERENCES {
            return Err(host_tool_err(
                "TOO_MANY_REFERENCE_IMAGES",
                format!("At most {MAX_IMAGE_REFERENCES} reference images are allowed"),
            ));
        }
        let tool_ctx = ctx.ok_or_else(|| {
            host_tool_err(
                "ARTIFACT_UNAVAILABLE",
                "ToolContext is required to resolve reference_artifact_ids",
            )
        })?;
        for item in arr {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    host_tool_err(
                        "INVALID_ARGUMENT",
                        "reference_artifact_ids entries must be non-empty strings",
                    )
                })?;
            out.push(resolve_artifact_image(tool_ctx, id).await?);
        }
    }

    if let Some(arr) = args.get("reference_images").and_then(Value::as_array) {
        for item in arr {
            let raw = item
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    host_tool_err(
                        "INVALID_ARGUMENT",
                        "reference_images entries must be non-empty data URL or base64 strings",
                    )
                })?;
            out.push(decode_data_url_or_base64_image(raw)?);
        }
    }

    if out.len() > MAX_IMAGE_REFERENCES {
        return Err(host_tool_err(
            "TOO_MANY_REFERENCE_IMAGES",
            format!("At most {MAX_IMAGE_REFERENCES} reference images are allowed"),
        ));
    }

    Ok(out)
}

pub(crate) async fn resolve_first_frame_from_args(
    ctx: Option<&ToolContext>,
    args: &Value,
) -> Result<Option<ReferenceImageInput>, ToolError> {
    let artifact_id = args
        .get("first_frame_artifact_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let inline_img = args
        .get("first_frame_image")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if artifact_id.is_some() && inline_img.is_some() {
        return Err(host_tool_err(
            "INVALID_ARGUMENT",
            "Specify at most one of first_frame_artifact_id or first_frame_image",
        ));
    }

    if let Some(id) = artifact_id {
        let tool_ctx = ctx.ok_or_else(|| {
            host_tool_err(
                "ARTIFACT_UNAVAILABLE",
                "ToolContext is required to resolve first_frame_artifact_id",
            )
        })?;
        return Ok(Some(resolve_artifact_image(tool_ctx, id).await?));
    }

    if let Some(raw) = inline_img {
        return Ok(Some(decode_data_url_or_base64_image(raw)?));
    }

    Ok(None)
}

async fn resolve_artifact_image(
    ctx: &ToolContext,
    artifact_id: &str,
) -> Result<ReferenceImageInput, ToolError> {
    let reader = ctx.artifacts().open(artifact_id).await?;
    let meta = reader.metadata();
    if meta.size_bytes > MAX_REFERENCE_IMAGE_BYTES {
        return Err(host_tool_err(
            "REFERENCE_IMAGE_TOO_LARGE",
            format!(
                "Reference artifact '{}' exceeds limit of {} bytes",
                artifact_id, MAX_REFERENCE_IMAGE_BYTES
            ),
        ));
    }
    let declared_mime = meta.mime_type.clone();
    let bytes = reader
        .read_all()
        .await
        .map_err(|e| host_tool_err("ARTIFACT_INTEGRITY", e.to_string()))?;
    let mime_type = sniff_raster_image_mime(&bytes).ok_or_else(|| {
        host_tool_err(
            "UNSUPPORTED_REFERENCE_IMAGE",
            format!(
                "Artifact '{}' (mime={}) is not a supported raster image (png/jpeg/webp/gif)",
                artifact_id, declared_mime
            ),
        )
    })?;
    Ok(ReferenceImageInput {
        bytes,
        mime_type: mime_type.to_string(),
    })
}

fn decode_data_url_or_base64_image(raw: &str) -> Result<ReferenceImageInput, ToolError> {
    let trimmed = raw.trim();
    let (declared_mime, b64_part) = if let Some(rest) = trimmed.strip_prefix("data:") {
        let Some((header, payload)) = rest.split_once(',') else {
            return Err(host_tool_err("INVALID_ARGUMENT", "Malformed data URL"));
        };
        let mime = header
            .split(';')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        (mime, payload.trim())
    } else {
        (None, trimmed)
    };

    let bytes = decode_base64(b64_part)
        .map_err(|e| host_tool_err("INVALID_ARGUMENT", format!("Invalid base64 image: {e}")))?;
    if bytes.is_empty() {
        return Err(host_tool_err(
            "INVALID_ARGUMENT",
            "Reference image bytes cannot be empty",
        ));
    }
    if (bytes.len() as u64) > MAX_REFERENCE_IMAGE_BYTES {
        return Err(host_tool_err(
            "REFERENCE_IMAGE_TOO_LARGE",
            format!(
                "Reference image exceeds limit of {} bytes",
                MAX_REFERENCE_IMAGE_BYTES
            ),
        ));
    }
    let mime_type = sniff_raster_image_mime(&bytes).ok_or_else(|| {
        host_tool_err(
            "UNSUPPORTED_REFERENCE_IMAGE",
            "Reference image must be a valid raster image (png, jpeg, webp, or gif)",
        )
    })?;
    let _ = declared_mime;
    Ok(ReferenceImageInput {
        bytes,
        mime_type: mime_type.to_string(),
    })
}

fn sniff_raster_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

#[cfg(test)]
pub(crate) fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut sextets = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad += 1;
            continue;
        }
        if pad > 0 {
            return Err("unexpected character after base64 padding".to_string());
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err("invalid base64 character".to_string()),
        };
        sextets.push(val);
    }
    let mut out = Vec::with_capacity((sextets.len() * 3) / 4);
    let mut i = 0;
    while i + 4 <= sextets.len() {
        let s0 = sextets[i];
        let s1 = sextets[i + 1];
        let s2 = sextets[i + 2];
        let s3 = sextets[i + 3];
        out.push((s0 << 2) | (s1 >> 4));
        out.push((s1 << 4) | (s2 >> 2));
        out.push((s2 << 6) | s3);
        i += 4;
    }
    let rem = sextets.len() - i;
    if rem == 1 {
        return Err("invalid base64 length".to_string());
    } else if rem == 2 {
        let s0 = sextets[i];
        let s1 = sextets[i + 1];
        out.push((s0 << 2) | (s1 >> 4));
    } else if rem == 3 {
        let s0 = sextets[i];
        let s1 = sextets[i + 1];
        let s2 = sextets[i + 2];
        out.push((s0 << 2) | (s1 >> 4));
        out.push((s1 << 4) | (s2 >> 2));
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

fn extract_svg_root_dimensions(raw_svg: &str) -> Option<(u32, u32)> {
    let trimmed = raw_svg.trim();
    if !trimmed.get(..4)?.eq_ignore_ascii_case("<svg") {
        return None;
    }
    let tag_end = trimmed.find('>')?;
    let open_tag = &trimmed[4..tag_end];
    let parse_dim = |s: &str| -> Option<u32> {
        let digits: String = s
            .trim()
            .trim_end_matches("px")
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        digits
            .parse::<f64>()
            .ok()
            .filter(|&v| v >= 16.0)
            .map(|v| (v.round() as u32).clamp(16, 2048))
    };
    let attr_w = crate::tools::web::get_attr(open_tag, "width").and_then(|v| parse_dim(&v));
    let attr_h = crate::tools::web::get_attr(open_tag, "height").and_then(|v| parse_dim(&v));
    if let (Some(w), Some(h)) = (attr_w, attr_h) {
        return Some((w, h));
    }
    if let Some(vb) = crate::tools::web::get_attr(open_tag, "viewbox") {
        let nums: Vec<f64> = vb
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        if nums.len() == 4 && nums[2] >= 16.0 && nums[3] >= 16.0 {
            let w = attr_w.unwrap_or_else(|| (nums[2].round() as u32).clamp(16, 2048));
            let h = attr_h.unwrap_or_else(|| (nums[3].round() as u32).clamp(16, 2048));
            return Some((w, h));
        }
    }
    None
}

fn ensure_svg_root_dimensions(raw_svg: &str, width: u32, height: u32) -> (String, u32, u32) {
    if let Some((svg_w, svg_h)) = extract_svg_root_dimensions(raw_svg) {
        return (raw_svg.to_string(), svg_w, svg_h);
    }
    let trimmed = raw_svg.trim();
    if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("<svg") {
        let injected = format!(
            r#"<svg width="{width}" height="{height}" viewBox="0 0 {width} {height}"{}"#,
            &trimmed[4..]
        );
        return (injected, width, height);
    }
    (trimmed.to_string(), width, height)
}

fn wrap_prompt_lines(prompt: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in prompt.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let mut current = String::new();
        for word in para.split_whitespace() {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > max_chars
            {
                lines.push(xml_escape(&current));
                current.clear();
                if lines.len() >= max_lines {
                    return lines;
                }
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() && lines.len() < max_lines {
            lines.push(xml_escape(&current));
        }
        if lines.len() >= max_lines {
            break;
        }
    }
    lines
}

fn render_svg_card(
    width: u32,
    height: u32,
    title: &str,
    prompt: &str,
    custom_body: Option<&str>,
) -> String {
    let safe_title = xml_escape(title);
    let body_markup = match custom_body {
        Some(custom) if !custom.trim().is_empty() => {
            let safe_prompt = xml_escape(&prompt.chars().take(160).collect::<String>());
            format!(
                r##"  <text x="32" y="92" fill="#94a3b8" font-family="sans-serif" font-size="14">{safe_prompt}</text>
  {custom}"##
            )
        }
        _ => {
            let max_chars = ((width.saturating_sub(96)) / 9).clamp(32, 110) as usize;
            let lines = wrap_prompt_lines(prompt, max_chars, 10);
            let box_w = width.saturating_sub(64).max(64);
            let box_h = height.saturating_sub(116).max(64);
            let tspans: String = lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    format!(
                        "    <tspan x=\"52\" y=\"{}\">{line}</tspan>\n",
                        132 + (i as u32) * 24
                    )
                })
                .collect();
            format!(
                r##"  <rect x="32" y="88" width="{box_w}" height="{box_h}" rx="12" fill="#1e293b" stroke="#334155" stroke-width="1.5"/>
  <text fill="#e2e8f0" font-family="sans-serif" font-size="15">
{tspans}  </text>"##
            )
        }
    };
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

pub(crate) fn find_iso_bmff_box<'a>(mut data: &'a [u8], target: &[u8; 4]) -> Option<&'a [u8]> {
    while data.len() >= 8 {
        let size32 = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let kind = &data[4..8];
        let (box_len, header_len) = if size32 == 1 {
            if data.len() < 16 {
                return None;
            }
            let size64 = u64::from_be_bytes(data[8..16].try_into().ok()?) as usize;
            (size64, 16usize)
        } else if size32 == 0 {
            (data.len(), 8usize)
        } else {
            (size32, 8usize)
        };
        if box_len < header_len || box_len > data.len() {
            return None;
        }
        if kind == target {
            return Some(&data[header_len..box_len]);
        }
        data = &data[box_len..];
    }
    None
}

/// Đọc thời lượng thực tế (giây) từ container ISO BMFF / MP4 (`moov -> mvhd`, version 0 hoặc version 1),
/// kèm nhánh fallback đọc `mdat` (`dur=Ns`) cho các synthetic stream cũ.
pub fn inspect_mp4_duration_secs(bytes: &[u8]) -> Option<u32> {
    if let Some(moov) = find_iso_bmff_box(bytes, b"moov") {
        if let Some(mvhd) = find_iso_bmff_box(moov, b"mvhd") {
            if !mvhd.is_empty() {
                let version = mvhd[0];
                let (timescale, duration) = if version == 1 && mvhd.len() >= 28 {
                    let ts = u32::from_be_bytes(mvhd[16..20].try_into().ok()?) as u64;
                    let dur = u64::from_be_bytes(mvhd[20..28].try_into().ok()?);
                    (ts, dur)
                } else if version == 0 && mvhd.len() >= 20 {
                    let ts = u32::from_be_bytes(mvhd[12..16].try_into().ok()?) as u64;
                    let dur = u32::from_be_bytes(mvhd[16..20].try_into().ok()?) as u64;
                    (ts, dur)
                } else {
                    (0, 0)
                };
                if let Some(secs) = duration
                    .saturating_add(timescale / 2)
                    .checked_div(timescale)
                {
                    return Some(secs.max(1) as u32);
                }
            }
        }
    }

    if let Some(mdat) = find_iso_bmff_box(bytes, b"mdat") {
        if let Ok(text) = std::str::from_utf8(mdat) {
            if let Some(pos) = text.find("dur=") {
                let rest = &text[pos + "dur=".len()..];
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(secs) = digits.parse::<u32>() {
                    if secs > 0 {
                        return Some(secs);
                    }
                }
            }
        }
    }
    None
}

/// Tạo container ISO BMFF / MP4 (`ftyp` + `moov/mvhd` + `mdat`) hợp lệ chứa metadata đặc tả cảnh video.
pub(crate) fn encode_minimal_mp4(
    prompt: &str,
    width: u32,
    height: u32,
    duration_secs: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + prompt.len());
    // Box 1: `ftyp` (24 bytes) - ISO Base Media File Format (`isom`, `mp42`)
    let ftyp_len: u32 = 24;
    out.extend_from_slice(&ftyp_len.to_be_bytes());
    out.extend_from_slice(b"ftyp");
    out.extend_from_slice(b"isom");
    out.extend_from_slice(&512u32.to_be_bytes());
    out.extend_from_slice(b"isommp42");

    // Box 2: `moov` (116 bytes) chứa `mvhd` (108 bytes, version 0, timescale = 1000)
    let mut mvhd_payload = [0u8; 100];
    let timescale: u32 = 1000;
    let duration_ms: u32 = duration_secs.max(1).saturating_mul(timescale);
    mvhd_payload[12..16].copy_from_slice(&timescale.to_be_bytes());
    mvhd_payload[16..20].copy_from_slice(&duration_ms.to_be_bytes());
    mvhd_payload[20..24].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // rate = 1.0
    mvhd_payload[24..26].copy_from_slice(&0x0100u16.to_be_bytes()); // volume = 1.0
    mvhd_payload[96..100].copy_from_slice(&1u32.to_be_bytes()); // next_track_id = 1

    let mvhd_len: u32 = 8 + (mvhd_payload.len() as u32);
    let moov_len: u32 = 8 + mvhd_len;
    out.extend_from_slice(&moov_len.to_be_bytes());
    out.extend_from_slice(b"moov");
    out.extend_from_slice(&mvhd_len.to_be_bytes());
    out.extend_from_slice(b"mvhd");
    out.extend_from_slice(&mvhd_payload);

    // Box 3: `mdat` chứa thông tin khung hình và prompt
    let payload = format!("MP4_STREAM|w={width}|h={height}|dur={duration_secs}s|prompt={prompt}");
    let mdat_len = (8 + payload.len()) as u32;
    out.extend_from_slice(&mdat_len.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(payload.as_bytes());
    out
}

#[cfg(test)]
#[path = "media_artifacts_tests.rs"]
mod tests;
