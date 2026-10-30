//! Công cụ tạo video MP4 (`video_create`) và trait [`VideoGeneratorBackend`] cho `Orchestrator`.

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

use crate::tools::media_artifacts::{
    mirror_artifact_to_dir, parse_role, DEFAULT_VIDEO_HEIGHT, DEFAULT_VIDEO_WIDTH,
    VIDEO_CREATE_TIMEOUT,
};
use crate::tools::media_image::{resolve_first_frame_from_args, ReferenceImageInput};
use crate::tools::media_video_concat::encode_minimal_mp4;
pub use crate::tools::media_video_concat::{
    inspect_mp4_duration_secs, ConcatenatedVideoPayload, DefaultVideoConcatBackend,
    VideoConcatBackend, VideoConcatInputClip, VideoConcatRequest, VideoConcatTool,
    VIDEO_CONCAT_TOOL,
};
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

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

/// Công cụ `video_create`: tạo video minh họa (`video/mp4`) từ prompt
/// và lưu trữ vào `ArtifactStore` của Task. Khi backend trả về nhiều phân cảnh (`clips.len() > 1`),
/// công cụ tự động lưu các clip thành phần ở vai trò `intermediate` và hợp nhất chúng thành một
/// artifact MP4 `primary` hoàn chỉnh qua `concat_backend`.
pub struct VideoCreateTool {
    pub backend: Arc<dyn VideoGeneratorBackend>,
    pub output_dir: Option<PathBuf>,
    pub(crate) concat_backend: Arc<dyn VideoConcatBackend>,
}

impl VideoCreateTool {
    pub fn new(backend: Arc<dyn VideoGeneratorBackend>) -> Self {
        Self {
            backend,
            output_dir: None,
            concat_backend: Arc::new(DefaultVideoConcatBackend),
        }
    }

    pub fn with_output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_concat_backend(
        mut self,
        concat_backend: Arc<dyn VideoConcatBackend>,
    ) -> Self {
        self.concat_backend = concat_backend;
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
                        "description": "Artifact visibility role for the final video output (default: 'primary'; when multiple clips are returned by the provider, individual clips are stored as 'intermediate' and automatically merged into a single 'primary' MP4 artifact)."
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

        let mut artifact_ids = Vec::with_capacity(clips.len() + usize::from(is_multi_clip));
        let mut clip_entries = Vec::with_capacity(clips.len());
        let mut concat_inputs = Vec::with_capacity(if is_multi_clip { clips.len() } else { 0 });
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

            let concat_bytes = if is_multi_clip {
                Some(clip.bytes.clone())
            } else {
                None
            };

            let artifact = ctx
                .artifacts()
                .put(ArtifactInput::from_bytes(
                    &clip_filename,
                    &clip.mime_type,
                    clip_role,
                    clip.bytes,
                ))
                .await?;

            if let Some(bytes) = concat_bytes {
                concat_inputs.push(VideoConcatInputClip {
                    artifact_id: artifact.id.clone(),
                    filename: artifact.name.clone(),
                    bytes,
                    duration_secs: clip_duration,
                });
            }

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

        let mut primary = first_artifact
            .ok_or_else(|| host_tool_err("VIDEO_GENERATION_FAILED", "No video clips returned"))?;
        let mut primary_mirrored_path = first_mirrored_path;
        let mut requires_concat = false;
        let mut intermediate_artifact_ids = Vec::new();

        if is_multi_clip {
            let concat_req = VideoConcatRequest {
                filename: format!("{safe_stem}.mp4"),
                title: req.title.clone(),
                clips: concat_inputs,
            };
            match self.concat_backend.concat_videos(&concat_req).await {
                Ok(merged) => {
                    primary_mirrored_path = mirror_artifact_to_dir(
                        self.output_dir.as_ref(),
                        &merged.filename,
                        &merged.bytes,
                    );
                    total_duration_secs = merged.duration_secs.max(total_duration_secs);
                    let merged_art = ctx
                        .artifacts()
                        .put(ArtifactInput::from_bytes(
                            &merged.filename,
                            &merged.mime_type,
                            requested_role,
                            merged.bytes,
                        ))
                        .await?;
                    intermediate_artifact_ids =
                        std::mem::replace(&mut artifact_ids, vec![merged_art.id.clone()]);
                    primary = merged_art;
                }
                Err(_) => {
                    requires_concat = true;
                    intermediate_artifact_ids = artifact_ids.clone();
                }
            }
        }

        let msg_suffix = generated
            .provider_message
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|m| format!("\nProvider message:\n{m}"))
            .unwrap_or_default();
        let summary = if requires_concat {
            format!(
                "Created {} intermediate video clip artifacts ({}s total, requested {}s): {:?}. Automatic concatenation failed; delegate to `media_processor` (`video_concat`) with `video_artifact_ids` in order to merge them into a single primary MP4 video artifact.{msg_suffix}",
                clip_entries.len(),
                total_duration_secs,
                duration_secs,
                intermediate_artifact_ids
            )
        } else if is_multi_clip {
            format!(
                "Created and merged {} video clips into primary video artifact '{}' (id={}, mime={}, duration_secs={}, bytes={}, intermediate_clips={:?}){msg_suffix}",
                clip_entries.len(),
                primary.name,
                primary.id,
                primary.mime_type,
                total_duration_secs,
                primary.size_bytes,
                intermediate_artifact_ids
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
                "intermediate_artifact_ids": intermediate_artifact_ids,
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
                "requires_concat": requires_concat,
                "provider_message": generated.provider_message,
                "provider": generated.provider,
                "path": primary_mirrored_path
            }),
        ))
    }
}

#[cfg(test)]
#[path = "media_video_tests.rs"]
mod tests;
