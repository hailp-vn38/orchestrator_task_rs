//! `orchestrator_flow_rs` — Thư viện quản lý **Task** chạy nền, hàng đợi **Followup**,
//! lịch hẹn **Schedule / Reminder** (One-shot Timer & Cron), bộ công cụ **Dual-Mode Tools**
//! (Voice Agent Raw JSON & `HostTool`) và cầu nối **Wiki Auto-Flow** cho hệ thống Đa Agent (`Orchestrator`).
//!
//! Xem thêm tài liệu tham chiếu đầy đủ tại `docs/API.md`.
//!
//! ## Kiến trúc & Khởi tạo 2 Pha
//!
//! 1. **Pha 1 ([`TaskManager::prepare`])**: Kiểm tra [`TaskConfig`], tạo thư mục lưu trữ,
//!    lấy khóa độc quyền `tasks.owner.lock` và mở/migrate cơ sở dữ liệu SQLite `tasks.db`.
//!    Xuất bộ `HostTool` qua [`TaskManagerInit::reminder_host_tools`] hoặc [`TaskManagerInit::all_host_tools`]
//!    để đăng ký vào `OrchestratorBuilder`.
//! 2. **Pha 2 ([`TaskManagerInit::start`])**: Gắn các `Arc<Orchestrator>` bằng
//!    [`TaskManagerInit::with_orchestrator`] hoặc [`TaskManagerInit::register_flow`] (kèm
//!    [`WikiAutoFlowBinding`] nếu dùng Auto-Flow), đồng bộ (reconcile) các Task đang dở dang
//!    từ lần chạy trước và khởi chạy các background worker (`Scheduler` & `Auto-Flow`).
//!
//! ## Ví dụ ghép nối
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use orchestrator_agent_rs::Orchestrator;
//! use orchestrator_flow_rs::{
//!     CreateTaskRequest, TaskConfig, TaskManager, TaskResult, WorkspaceLayout,
//! };
//!
//! async fn boot(orchestrator: Arc<Orchestrator>) -> TaskResult<Arc<TaskManager>> {
//!     let layout = WorkspaceLayout::new("./homelab_workspace");
//!     let config = TaskConfig::from_layout(&layout)
//!         .with_default_timezone("Asia/Ho_Chi_Minh")
//!         .with_default_agent("daily_root");
//!
//!     let init = TaskManager::prepare(config).await?;
//!     // Có thể lấy `init.reminder_host_tools()` trước khi build `orchestrator`.
//!     let task_mgr = init.with_orchestrator(orchestrator).start().await?;
//!
//!     let _task = task_mgr
//!         .create_task(CreateTaskRequest::new(
//!             "Tóm tắt tài liệu",
//!             "Đọc Wiki và tóm tắt các điểm chính.",
//!         ))
//!         .await?;
//!
//!     Ok(task_mgr)
//! }
//! ```

mod config;
mod error;
mod manager;
mod owner_lock;
pub mod prompts;
mod recipe;
mod runtime;
mod store;
mod tools;
mod types;

pub use config::{TargetAgent, TaskConfig, TaskRuntimeConfig, WorkspaceLayout, DEFAULT_FLOW_ID};
pub use error::{ErrorCode, TaskError, TaskResult};
pub use manager::{TaskManager, TaskManagerInit};
pub use orchestrator_agent_rs::{ArtifactInput, ArtifactRef, ArtifactRole, MAX_SUBMIT_ARTIFACTS};
pub use prompts::{
    DAILY_ROOT_PROMPT, DAILY_SYNTHESIS_STAGING_SKILL, DAILY_TASK_REMINDER_SKILL,
    MEDIA_CREATOR_PROMPT, MEDIA_PROCESSOR_PROMPT, REMINDER_PLANNER_PROMPT, REPORT_STAGER_PROMPT,
    SOURCE_COLLECTOR_PROMPT, WIKI_CURATOR_PROMPT, WIKI_INGESTOR_PROMPT, WIKI_RESEARCHER_PROMPT,
    WIKI_ROOT_PROMPT,
};
pub use recipe::{
    validate_recipe_params, validate_recipe_registry, validate_recipe_schema, RecipeDecision,
    RecipeMetadata, RecipeProgress, RecipeProviderRuntime, RecipeStepKind, RecipeStepOutline,
    RecipeValidationError, TaskRecipe, TaskRecipeView,
};
pub use runtime::WikiAutoFlowBinding;
pub use tools::{
    add_to_sources_host_tool, build_sample_technical_pdf, build_tool_specs_for_agents,
    clean_noise_tags, collapse_ws, convert_document_with_anydoc, decode_html_entities,
    extract_article, extract_feed_items, extract_links, extract_metadata,
    extract_wav_from_mp4_bytes, find_elements, find_open_tags, format_segments_as_srt,
    format_srt_timestamp, get_attr, inspect_mp4_bytes, inspect_mp4_duration_secs,
    inspect_wav_bytes, locate_article_region, media_host_tools, media_host_tools_with_video,
    normalize_video_stem, pdf_host_tools, remove_html_comments, remove_tag_blocks,
    report_host_tools, resolve_url, strip_tags, web_host_tools, wiki_host_tools,
    wiki_readonly_host_tools, youtube_host_tools, AddToSourcesTool, AnydocInspectPdfTool,
    AnydocSaveToSourcesTool, ArtifactCreateTool, AsrTranscribeTool, AsrTranscriberBackend,
    AsrTranscriptionRequest, CollectionStatusTool, ConcatenatedVideoPayload, ConvertedPdfDoc,
    DefaultAsrTranscriberBackend, DefaultImageGeneratorBackend, DefaultTtsSynthesizerBackend,
    DefaultVideoConcatBackend, DefaultVideoGeneratorBackend, ExistingReason, ExtractedArticle,
    GeneratedImagePayload, GeneratedVideoClip, GeneratedVideoPayload, HtmlTool, HtmlToolKind,
    HtmlWorkspace, ImageCreateTool, ImageGenerationRequest, ImageGeneratorBackend,
    MarkdownCreateTool, MarkdownReadTool, MediaBackendError, MediaCallContext, PlaylistVideoEntry,
    ReferenceImageInput, SaveSourceOutcome, SharedPdfState, SubmittingReconciliation,
    SynthesizedAudioPayload, TaskToolSpec, TaskToolSuite, TranscribedAudioPayload,
    TranscribedAudioSegment, TtsProviderConstraints, TtsSynthesisRequest, TtsSynthesizeTool,
    TtsSynthesizerBackend, VideoConcatBackend, VideoConcatInputClip, VideoConcatRequest,
    VideoConcatTool, VideoCreateTool, VideoGenerationRequest, VideoGeneratorBackend,
    VideoOperationJournal, VideoProviderConstraints, WavHeaderInfo, WavSampleFormat,
    WikiEngineBridgeTool, WorkspaceToolKit, YoutubeCollectorEnv, YtFetchTranscriptTool,
    YtListPlaylistTool, ASR_TRANSCRIBE_TOOL, CODE_ASR_UNSUPPORTED_AUDIO, COLLECTOR_TOOL_NAMES,
    DEFAULT_PNG_HEIGHT, DEFAULT_PNG_WIDTH, DEFAULT_SVG_HEIGHT, DEFAULT_SVG_WIDTH,
    DEFAULT_VIDEO_HEIGHT, DEFAULT_VIDEO_WIDTH, HTML_TOOL_NAMES, MAX_IMAGE_REFERENCES,
    MAX_REFERENCE_IMAGE_BYTES, VIDEO_CONCAT_TOOL,
};
pub use types::{
    AgentCatalogEntry, CreateScheduleRequest, CreateTaskRequest, FollowupStatus, FollowupView,
    Priority, ResolveStuckTool, ScheduleActionKind, ScheduleFilter, ScheduleKind,
    ScheduleSessionMode, ScheduleStatus, ScheduleView, StuckToolDecision, TaskAgentMessageView,
    TaskAgentRunView, TaskAgentSessionView, TaskArtifactCatalogEntry, TaskArtifactCatalogPage,
    TaskArtifactCatalogSummary, TaskArtifactDownload, TaskArtifactFilter, TaskArtifactKind,
    TaskArtifactRole, TaskArtifactView, TaskDetailView, TaskEvent, TaskExecutionSummary,
    TaskFeedItem, TaskFilter, TaskMessageToolCallView, TaskPage, TaskRecipeRequest, TaskScope,
    TaskSource, TaskStatus, TaskSubscription, TaskToolInvocationView, TaskView, WikiDeleteOutcome,
    DEFAULT_OWNER_ID, DEFAULT_WORKSPACE_ID,
};
