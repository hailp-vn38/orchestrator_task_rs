//! Công cụ hậu kỳ ghép nối video MP4 (`video_concat`), [`VideoConcatBackend`] và các hàm xử lý
//! container ISO BMFF / MP4.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orchestrator_agent_rs::{
    ArtifactInput, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};

use crate::tools::media_artifacts::{
    mirror_artifact_to_dir, parse_role, DEFAULT_VIDEO_HEIGHT, DEFAULT_VIDEO_WIDTH,
    VIDEO_CONCAT_TIMEOUT,
};
use crate::tools::media_video::normalize_video_stem;
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

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
/// - Hỗ trợ ghép in-process cho container ISO BMFF `MP4_STREAM|` của [`crate::tools::media_video::DefaultVideoGeneratorBackend`].
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
            description: "Concatenate 2 or more sequential MP4 video clip artifacts (`video_artifact_ids`) in order into a single continuous primary MP4 video artifact. Use `video_concat` when merging user-attached `.mp4` Input Artifacts, combining multiple existing video artifacts, or when `video_create` explicitly reports `requires_concat: true`.".to_string(),
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
