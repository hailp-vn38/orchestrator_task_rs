//! `orchestrator_task_rs` — Thư viện quản lý **Task** chạy nền, hàng đợi **Followup**,
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
//! use orchestrator_task_rs::{
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
mod runtime;
mod store;
mod tools;
mod types;

pub use config::{TargetAgent, TaskConfig, TaskRuntimeConfig, WorkspaceLayout, DEFAULT_FLOW_ID};
pub use error::{ErrorCode, TaskError, TaskResult};
pub use manager::{TaskManager, TaskManagerInit};
pub use runtime::WikiAutoFlowBinding;
pub use tools::{
    add_to_sources_host_tool, build_sample_technical_pdf, convert_document_with_anydoc,
    fetch_playlist, fetch_transcript, pdf_host_tools, report_host_tools, wiki_host_tools,
    wiki_readonly_host_tools, youtube_host_tools, AddToSourcesTool, AnydocInspectPdfTool,
    AnydocSaveToSourcesTool, ConvertedPdfDoc, MarkdownCreateTool, MarkdownReadTool,
    PlaylistVideoEntry, SharedPdfState, TaskToolSpec, TaskToolSuite, WikiEngineBridgeTool,
    WorkspaceToolKit, YoutubeCollectorEnv, YtFetchTranscriptTool, YtListPlaylistTool,
};
pub use types::{
    AgentCatalogEntry, CreateScheduleRequest, CreateTaskRequest, FollowupStatus, FollowupView,
    Priority, ScheduleActionKind, ScheduleFilter, ScheduleKind, ScheduleSessionMode,
    ScheduleStatus, ScheduleView, TaskAgentMessageView, TaskAgentRunView, TaskAgentSessionView,
    TaskDetailView, TaskEvent, TaskExecutionSummary, TaskFeedItem, TaskFilter,
    TaskMessageToolCallView, TaskPage, TaskScope, TaskSource, TaskStatus, TaskSubscription,
    TaskToolInvocationView, TaskView, WikiDeleteOutcome, DEFAULT_OWNER_ID, DEFAULT_WORKSPACE_ID,
};
