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

/// Video ngắn hơn yêu cầu tối đa chừng này (nửa một clip 10s) vẫn được coi là hoàn tất.
const VIDEO_DURATION_TOLERANCE_SECS: u32 = 5;

/// Giao diện ghi nhật ký tiến trình bền vững vào bảng `tool_operations` của `Orchestrator`.
#[async_trait::async_trait]
pub trait VideoOperationJournal: Send + Sync {
    async fn advance(
        &self,
        update: orchestrator_agent_rs::OperationUpdate,
    ) -> Result<(), orchestrator_agent_rs::ToolError>;
}

#[async_trait::async_trait]
impl VideoOperationJournal for ToolContext {
    async fn advance(
        &self,
        update: orchestrator_agent_rs::OperationUpdate,
    ) -> Result<(), orchestrator_agent_rs::ToolError> {
        self.operation().advance(update).await
    }
}

/// Kết quả đối soát một bản ghi `tool_operations` đang ở trạng thái `submitting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmittingReconciliation {
    /// Đọc được `/chat/history` của `remote_session_id`: trả tổng số `message.user` hiện có.
    RemoteUserTurns(u32),
    /// Không đọc được history hoặc provider không hỗ trợ đối soát.
    Unreadable,
}

/// Ngữ cảnh điều phối truyền xuống các backend media (`image_create`, `video_create`),
/// bao gồm tín hiệu hủy hợp tác / shutdown và trạng thái `tool_operations` bền vững.
#[derive(Clone, Default)]
pub struct MediaCallContext {
    pub cancel: orchestrator_agent_rs::CancelSignal,
    pub operation: Option<orchestrator_agent_rs::ToolOperation>,
    pub journal: Option<Arc<dyn VideoOperationJournal>>,
}

impl std::fmt::Debug for MediaCallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaCallContext")
            .field("cancel", &self.cancel)
            .field("operation", &self.operation)
            .field("has_journal", &self.journal.is_some())
            .finish()
    }
}

/// Lỗi có cấu trúc từ [`VideoGeneratorBackend`] để giữ nguyên mã lỗi gốc (`VIDEO_GENERATION_TIMEOUT`,
/// `PROVIDER_RATE_LIMITED`, v.v.) lên tới `ToolError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaBackendError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl MediaBackendError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

impl From<String> for MediaBackendError {
    fn from(message: String) -> Self {
        Self::new("VIDEO_GENERATION_FAILED", message, false)
    }
}

impl From<&str> for MediaBackendError {
    fn from(message: &str) -> Self {
        Self::new("VIDEO_GENERATION_FAILED", message.to_string(), false)
    }
}

impl std::fmt::Display for MediaBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
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
    /// Khoá liên tục theo job (root session id của lần gọi tool). Backend dùng khoá này để giữ
    /// một session phía provider cho mọi lần `video_create` trong cùng một job.
    #[serde(default)]
    pub continuation_key: Option<String>,
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
        let duration_secs = inspect_mp4_duration_secs(&bytes).unwrap_or(self.duration_secs);
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
        ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError>;

    /// Đối soát trạng thái `submitting` sau khi tiến trình khởi động lại.
    async fn reconcile_submitting(
        &self,
        _req: &VideoGenerationRequest,
        _op: &orchestrator_agent_rs::ToolOperation,
        _ctx: &MediaCallContext,
    ) -> SubmittingReconciliation {
        SubmittingReconciliation::Unreadable
    }

    /// Tiếp tục theo dõi/chạy lô tiếp theo của một operation đã sang `running`.
    async fn resume_video(
        &self,
        _req: &VideoGenerationRequest,
        _op: &orchestrator_agent_rs::ToolOperation,
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        Err(MediaBackendError::new(
            "TOOL_OUTCOME_UNKNOWN",
            "video provider does not support resuming an in-flight operation",
            false,
        ))
    }

    /// Tải lại các tệp video khi operation đã sang `downloading` trước lúc crash.
    async fn download_video(
        &self,
        req: &VideoGenerationRequest,
        op: &orchestrator_agent_rs::ToolOperation,
        ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
        self.resume_video(req, op, ctx).await
    }
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
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedVideoPayload, MediaBackendError> {
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
            effect: ToolEffect::Resumable,
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
        let max_total = constraints.max_total_duration_secs.max(1);
        let duration_secs = args
            .get("duration_secs")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(4)
            .max(1);
        if duration_secs > max_total {
            // Never shorten silently: the agent must know the video it asked for cannot be made.
            return Err(host_tool_err(
                "INVALID_ARGUMENT",
                format!(
                    "duration_secs={duration_secs} exceeds this provider's max_total_duration_secs={max_total}. Split the video at scene boundaries into several video_create calls of at most {max_total}s each, then merge the resulting clips in order with video_concat."
                ),
            ));
        }
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
            continuation_key: Some(ctx.scope().root_session_id.clone())
                .filter(|key| !key.trim().is_empty()),
        };
        let (width, height) = req.effective_dimensions();
        req.width = width;
        req.height = height;

        // Cancellation is cooperative: stop waiting for the provider as soon as the job is
        // cancelled instead of holding the run until the provider (or the tool timeout) gives up.
        let request_json_str = json!({
            "prompt": req.prompt,
            "duration_secs": req.duration_secs,
            "filename": req.filename,
            "width": req.width,
            "height": req.height,
        })
        .to_string();

        let (has_op_store, existing_op) = match ctx.operation().current().await {
            Ok(op) => (true, op),
            Err(e) if e.code == "ARTIFACT_UNAVAILABLE" => (false, None),
            Err(e) => return Err(e),
        };

        enum DispatchAction {
            Submit(Option<orchestrator_agent_rs::ToolOperation>),
            Resume(orchestrator_agent_rs::ToolOperation),
            Download(orchestrator_agent_rs::ToolOperation),
        }

        let action = if !has_op_store {
            DispatchAction::Submit(None)
        } else {
            match existing_op {
                None => {
                    let deadline_at = ctx.operation().now_secs().saturating_add(900);
                    let mut op = ctx
                        .operation()
                        .begin(deadline_at, &constraints.provider_label)
                        .await?;
                    ctx.operation()
                        .advance(orchestrator_agent_rs::OperationUpdate {
                            request_json: Some(request_json_str.clone()),
                            ..Default::default()
                        })
                        .await?;
                    op.request_json = Some(request_json_str);
                    DispatchAction::Submit(Some(op))
                }
                Some(op) => match op.status {
                    orchestrator_agent_rs::ToolOperationStatus::Committed => {
                        let saved: Value = op
                            .result_json
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or_else(|| json!({}));
                        let content = saved
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or("Created video artifact")
                            .to_string();
                        let metadata = saved.get("metadata").cloned().unwrap_or(Value::Null);
                        return Ok(host_tool_outcome(&ctx, content, metadata));
                    }
                    orchestrator_agent_rs::ToolOperationStatus::Failed
                    | orchestrator_agent_rs::ToolOperationStatus::Expired
                    | orchestrator_agent_rs::ToolOperationStatus::Cancelled => {
                        let saved: Value = op
                            .result_json
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or_else(|| json!({}));
                        let default_code = match op.status {
                            orchestrator_agent_rs::ToolOperationStatus::Cancelled => "CANCELLED",
                            orchestrator_agent_rs::ToolOperationStatus::Expired => {
                                "VIDEO_GENERATION_TIMEOUT"
                            }
                            _ => "VIDEO_GENERATION_FAILED",
                        };
                        let code = saved
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or(default_code);
                        let message = saved
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("video operation did not complete");
                        return Err(host_tool_err(code, message));
                    }
                    orchestrator_agent_rs::ToolOperationStatus::Submitting => {
                        let probe_ctx = MediaCallContext {
                            cancel: ctx.cancel_signal().clone(),
                            operation: Some(op.clone()),
                            journal: Some(Arc::new(ctx.clone())),
                        };
                        let Some(before) = op.user_turns_before else {
                            return Err(host_tool_err(
                                "TOOL_OUTCOME_UNKNOWN",
                                "in-flight video submission has no recorded user_turns_before",
                            ));
                        };
                        match self
                            .backend
                            .reconcile_submitting(&req, &op, &probe_ctx)
                            .await
                        {
                            SubmittingReconciliation::RemoteUserTurns(actual)
                                if actual > before =>
                            {
                                let mut running_op = op;
                                ctx.operation()
                                    .advance(orchestrator_agent_rs::OperationUpdate {
                                        status: Some(
                                            orchestrator_agent_rs::ToolOperationStatus::Running,
                                        ),
                                        ..Default::default()
                                    })
                                    .await?;
                                running_op.status =
                                    orchestrator_agent_rs::ToolOperationStatus::Running;
                                DispatchAction::Resume(running_op)
                            }
                            SubmittingReconciliation::RemoteUserTurns(actual)
                                if actual == before =>
                            {
                                DispatchAction::Submit(Some(op))
                            }
                            _ => {
                                return Err(host_tool_err(
                                    "TOOL_OUTCOME_UNKNOWN",
                                    "could not reconcile in-flight video submission after restart",
                                ));
                            }
                        }
                    }
                    orchestrator_agent_rs::ToolOperationStatus::Running => {
                        DispatchAction::Resume(op)
                    }
                    orchestrator_agent_rs::ToolOperationStatus::Downloading => {
                        DispatchAction::Download(op)
                    }
                },
            }
        };

        let op_for_ctx = match &action {
            DispatchAction::Submit(op) => op.clone(),
            DispatchAction::Resume(op) | DispatchAction::Download(op) => Some(op.clone()),
        };
        let call_ctx = MediaCallContext {
            cancel: ctx.cancel_signal().clone(),
            operation: op_for_ctx,
            journal: has_op_store
                .then(|| Arc::new(ctx.clone()) as Arc<dyn VideoOperationJournal>),
        };
        let backend_fut = async {
            match &action {
                DispatchAction::Submit(_) => self.backend.generate_video(&req, &call_ctx).await,
                DispatchAction::Resume(op) => self.backend.resume_video(&req, op, &call_ctx).await,
                DispatchAction::Download(op) => {
                    self.backend.download_video(&req, op, &call_ctx).await
                }
            }
        };
        let mut generated = tokio::select! {
            result = backend_fut => {
                match result {
                    Ok(payload) => payload,
                    Err(e) => {
                        if has_op_store && e.code != "TOOL_OUTCOME_UNKNOWN" {
                            let status = if e.code == "VIDEO_GENERATION_TIMEOUT" {
                                orchestrator_agent_rs::ToolOperationStatus::Expired
                            } else if e.code == "CANCELLED" {
                                orchestrator_agent_rs::ToolOperationStatus::Cancelled
                            } else {
                                orchestrator_agent_rs::ToolOperationStatus::Failed
                            };
                            let _ = ctx
                                .operation()
                                .advance(orchestrator_agent_rs::OperationUpdate {
                                    status: Some(status),
                                    result_json: Some(
                                        json!({ "code": e.code, "message": e.message }).to_string(),
                                    ),
                                    ..Default::default()
                                })
                                .await;
                        }
                        return Err(host_tool_err(&e.code, e.message));
                    }
                }
            }
            () = ctx.cancel_token().cancelled() => {
                if has_op_store
                    && ctx.cancel_reason() != Some(orchestrator_agent_rs::CancelReason::Shutdown)
                {
                    let _ = ctx
                        .operation()
                        .advance(orchestrator_agent_rs::OperationUpdate {
                            status: Some(orchestrator_agent_rs::ToolOperationStatus::Cancelled),
                            result_json: Some(
                                json!({
                                    "code": "CANCELLED",
                                    "message": "video generation was cancelled before the provider finished"
                                })
                                .to_string(),
                            ),
                            ..Default::default()
                        })
                        .await;
                }
                return Err(host_tool_err(
                    "CANCELLED",
                    "video generation was cancelled before the provider finished",
                ));
            }
        };

        let clips = generated.take_effective_clips();
        let is_multi_clip = clips.len() > 1;
        let safe_stem = normalize_video_stem(filename, "video");

        let mut artifact_ids = Vec::with_capacity(clips.len() + usize::from(is_multi_clip));
        let mut clip_entries = Vec::with_capacity(clips.len());
        let mut concat_inputs = Vec::with_capacity(if is_multi_clip { clips.len() } else { 0 });
        let mut total_duration_secs: u32 = 0;
        let mut duration_measurable = true;
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
            let clip_duration = match inspect_mp4_duration_secs(&clip.bytes) {
                Some(d) => d,
                None => {
                    duration_measurable = false;
                    clip.duration_secs
                }
            };
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
                    if let Some(merged_dur) = inspect_mp4_duration_secs(&merged.bytes) {
                        duration_measurable = true;
                        total_duration_secs = merged_dur.max(total_duration_secs);
                    } else {
                        total_duration_secs = merged.duration_secs.max(total_duration_secs);
                    }
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

        // The model only reads `summary`; metadata is for hosts. State completeness in the text so
        // a short video is never mistaken for a finished one.
        let complete = !requires_concat
            && duration_measurable
            && total_duration_secs.saturating_add(VIDEO_DURATION_TOLERANCE_SECS) >= duration_secs;
        let complete_status = if !requires_concat && !duration_measurable {
            "unknown"
        } else if complete {
            "true"
        } else {
            "false"
        };
        let next_action = if requires_concat {
            "concat_required"
        } else if complete {
            "none"
        } else {
            "call_again_same_job"
        };
        let summary = format!(
            "{summary}\nrequested={duration_secs}s actual={total_duration_secs}s complete={complete_status} next_action={next_action}"
        );

        let metadata = json!({
            "complete": if !requires_concat && !duration_measurable { json!("unknown") } else { json!(complete) },
            "next_action": next_action,
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
        });

        if has_op_store {
            let mut all_persisted_ids = intermediate_artifact_ids.clone();
            if let Some(ids) = metadata.get("artifact_ids").and_then(Value::as_array) {
                for id in ids.iter().filter_map(Value::as_str) {
                    if !all_persisted_ids.iter().any(|x| x == id) {
                        all_persisted_ids.push(id.to_string());
                    }
                }
            }
            ctx.operation()
                .advance(orchestrator_agent_rs::OperationUpdate {
                    status: Some(orchestrator_agent_rs::ToolOperationStatus::Committed),
                    artifact_ids_json: Some(json!(all_persisted_ids).to_string()),
                    result_json: Some(
                        json!({
                            "content": summary,
                            "metadata": metadata,
                        })
                        .to_string(),
                    ),
                    ..Default::default()
                })
                .await?;
        }

        Ok(host_tool_outcome(&ctx, summary, metadata))
    }
}

#[cfg(test)]
#[path = "media_video_tests.rs"]
mod tests;
