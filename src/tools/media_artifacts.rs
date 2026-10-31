//! Bộ công cụ `HostTool` đa phương tiện & đa định dạng cho Subagent (`image_create`, `video_create`, `video_concat`, `tts_synthesize`, `artifact_create`)
//! xuất bản trực tiếp vào `ArtifactStore` (`ctx.artifacts().put(...)`) của `Orchestrator` và hiển thị
//! dưới dạng [`TaskArtifactView`] trong `TaskManager`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator_agent_rs::{
    ArtifactInput, ArtifactRole, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError,
    ToolOutcome,
};
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
pub(crate) use crate::tools::media_tts::synthesize_pcm_wav;
pub use crate::tools::media_tts::{
    DefaultTtsSynthesizerBackend, SynthesizedAudioPayload, TtsProviderConstraints,
    TtsSynthesisRequest, TtsSynthesizeTool, TtsSynthesizerBackend,
};
pub use crate::tools::media_video::{
    normalize_video_stem, ConcatenatedVideoPayload, DefaultVideoConcatBackend,
    DefaultVideoGeneratorBackend, GeneratedVideoClip, GeneratedVideoPayload, MediaBackendError,
    MediaCallContext, VideoConcatBackend, VideoConcatInputClip, VideoConcatRequest,
    VideoConcatTool, VideoCreateTool, VideoGenerationRequest, VideoGeneratorBackend,
    VideoProviderConstraints, VIDEO_CONCAT_TOOL,
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

#[cfg(test)]
#[path = "media_artifacts_tests.rs"]
mod tests;
