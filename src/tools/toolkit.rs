//! `WorkspaceToolKit` — Bộ quản lý và khởi tạo trọn gói toàn bộ `HostTool` cho kiến trúc Dual-Flow
//! (Wiki Engine, `add_to_sources`, Markdown Reports, YouTube Playlist Collector, PDF `anydoc` Collector,
//! Web / HTML Extractor Tools, và Task/Reminder Tools từ `TaskManagerInit`).

use std::path::PathBuf;
use std::sync::Arc;

use llm_wiki::{AdaptiveReadBudget, WikiEngine};
use orchestrator_agent_rs::HostTool;
use tokio::sync::Notify;

use crate::config::WorkspaceLayout;
use crate::manager::TaskManagerInit;
use crate::tools::asr_transcribe::{
    AsrTranscribeTool, AsrTranscriberBackend, DefaultAsrTranscriberBackend,
};
use crate::tools::markdown_report::report_host_tools;
use crate::tools::media_artifacts::{
    media_host_tools_with_video, DefaultImageGeneratorBackend, DefaultTtsSynthesizerBackend,
    DefaultVideoGeneratorBackend, ImageGeneratorBackend, TtsSynthesizerBackend,
    VideoGeneratorBackend,
};
use crate::tools::pdf::{pdf_host_tools, SharedPdfState};
use crate::tools::web::{web_host_tools, HtmlWorkspace};
use crate::tools::wiki_bridge::{wiki_host_tools, wiki_readonly_host_tools, AddToSourcesTool};
use crate::tools::youtube::{youtube_host_tools, YoutubeCollectorEnv};

const DEFAULT_CONTEXT_LIMIT_TOKENS: usize = 200_000;

/// Danh sách tên 9 `HostTool` chỉ đọc (`ReadOnly`) để kiểm tra/trích xuất nguồn ngoại vi thuộc Flow 1 (`source_collector`):
/// `anydoc_inspect_pdf` (PDF), `yt_list_playlist` / `yt_fetch_transcript` (YouTube), và `fetch_html`/`html_*` (Web/HTML).
/// Việc lưu vào `sources/` được thực hiện thống nhất qua công cụ duy nhất `add_to_sources`.
pub const COLLECTOR_TOOL_NAMES: &[&str] = &[
    "anydoc_inspect_pdf",
    "yt_list_playlist",
    "yt_fetch_transcript",
    "fetch_html",
    "html_extract_metadata",
    "html_extract_article",
    "html_extract_feed",
    "html_extract_links",
    "html_select_elements",
];

/// Bộ công cụ hợp nhất ở mức Workspace giúp Host chỉ cần khởi tạo 1 lần và lấy ra các nhóm `HostTool`
/// để đăng ký vào `OrchestratorBuilder` mà không cần tự viết lại từng adapter.
#[derive(Clone)]
pub struct WorkspaceToolKit {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
    pub reports_dir: PathBuf,
    pub read_budget: AdaptiveReadBudget,
    pub pdf_state: Arc<SharedPdfState>,
    pub yt_env: Arc<YoutubeCollectorEnv>,
    pub html_workspace: Arc<HtmlWorkspace>,
    pub image_backend: Arc<dyn ImageGeneratorBackend>,
    pub video_backend: Arc<dyn VideoGeneratorBackend>,
    pub tts_backend: Arc<dyn TtsSynthesizerBackend>,
    pub asr_backend: Arc<dyn AsrTranscriberBackend>,
}

impl WorkspaceToolKit {
    /// Khởi tạo `WorkspaceToolKit` từ `WorkspaceLayout`, `Arc<WikiEngine>` và `Arc<Notify>`.
    ///
    /// Tự động thiết lập:
    /// - `reports_dir` = `layout.reports_dir()`
    /// - `pdf_state` làm việc tại `<root>/work/pdf`
    /// - `yt_env` làm việc tại `<root>/work/yt`
    /// - `html_workspace` làm việc tại `<root>/work/html`
    /// - `read_budget` mặc định cho context window `200_000` tokens
    pub fn from_layout(
        layout: &WorkspaceLayout,
        engine: Arc<WikiEngine>,
        autoflow_notify: Arc<Notify>,
    ) -> Self {
        let work_dir = layout.root_dir().join("work");
        let mut kit = Self::new(
            engine,
            autoflow_notify,
            layout.reports_dir(),
            work_dir.join("pdf"),
            work_dir.join("yt"),
        );
        kit.html_workspace = Arc::new(HtmlWorkspace::new(work_dir.join("html")));
        kit
    }

    /// Khởi tạo `WorkspaceToolKit` với các đường dẫn thư mục tùy chỉnh.
    pub fn new(
        engine: Arc<WikiEngine>,
        autoflow_notify: Arc<Notify>,
        reports_dir: impl Into<PathBuf>,
        pdf_work_dir: impl Into<PathBuf>,
        yt_work_dir: impl Into<PathBuf>,
    ) -> Self {
        let pdf_dir = pdf_work_dir.into();
        let html_dir = pdf_dir
            .parent()
            .map(|p| p.join("html"))
            .unwrap_or_else(|| pdf_dir.join("html"));
        let pdf_state = Arc::new(SharedPdfState::new(
            Arc::clone(&engine),
            Arc::clone(&autoflow_notify),
            pdf_dir,
        ));
        let yt_env = Arc::new(YoutubeCollectorEnv::new(
            Arc::clone(&engine),
            Arc::clone(&autoflow_notify),
            yt_work_dir,
        ));
        let html_workspace = Arc::new(HtmlWorkspace::new(html_dir));
        Self {
            engine,
            autoflow_notify,
            reports_dir: reports_dir.into(),
            read_budget: AdaptiveReadBudget::for_context_limit(DEFAULT_CONTEXT_LIMIT_TOKENS),
            pdf_state,
            yt_env,
            html_workspace,
            image_backend: Arc::new(DefaultImageGeneratorBackend),
            video_backend: Arc::new(DefaultVideoGeneratorBackend),
            tts_backend: Arc::new(DefaultTtsSynthesizerBackend),
            asr_backend: Arc::new(DefaultAsrTranscriberBackend),
        }
    }

    /// Tùy chỉnh `ImageGeneratorBackend` cho công cụ `image_create`.
    pub fn with_image_backend(mut self, backend: Arc<dyn ImageGeneratorBackend>) -> Self {
        self.image_backend = backend;
        self
    }

    /// Tùy chỉnh `VideoGeneratorBackend` cho công cụ `video_create`.
    pub fn with_video_backend(mut self, backend: Arc<dyn VideoGeneratorBackend>) -> Self {
        self.video_backend = backend;
        self
    }

    /// Tùy chỉnh `TtsSynthesizerBackend` cho công cụ `tts_synthesize`.
    pub fn with_tts_backend(mut self, backend: Arc<dyn TtsSynthesizerBackend>) -> Self {
        self.tts_backend = backend;
        self
    }

    /// Tùy chỉnh `AsrTranscriberBackend` cho công cụ `asr_transcribe`.
    pub fn with_asr_backend(mut self, backend: Arc<dyn AsrTranscriberBackend>) -> Self {
        self.asr_backend = backend;
        self
    }

    /// Tùy chỉnh thư mục lưu cache HTML nguyên bản cho `HtmlWorkspace`.
    pub fn with_html_work_dir(mut self, html_work_dir: impl Into<PathBuf>) -> Self {
        self.html_workspace = Arc::new(HtmlWorkspace::new(html_work_dir));
        self
    }

    /// Ghi đè `AdaptiveReadBudget` cho các tool đọc của `WikiEngine`.
    pub fn with_read_budget(mut self, read_budget: AdaptiveReadBudget) -> Self {
        self.read_budget = read_budget;
        self
    }

    /// Tính và đặt `AdaptiveReadBudget` dựa trên `context_limit_tokens`.
    pub fn with_context_limit_tokens(mut self, context_limit_tokens: usize) -> Self {
        self.read_budget = AdaptiveReadBudget::for_context_limit(context_limit_tokens);
        self
    }

    /// Tùy chỉnh ngôn ngữ phụ đề ưu tiên (`langs`, VD: `"en,vi"`) và giới hạn số video (`limit`) cho bộ thu thập YouTube.
    pub fn with_youtube_options(mut self, langs: impl Into<String>, limit: usize) -> Self {
        let work_dir = self.yt_env.work_dir.clone();
        self.yt_env = Arc::new(
            YoutubeCollectorEnv::new(
                Arc::clone(&self.engine),
                Arc::clone(&self.autoflow_notify),
                work_dir,
            )
            .with_langs(langs)
            .with_limit(limit),
        );
        self
    }

    /// Tùy chỉnh giới hạn số ký tự tối đa trích xuất từ mỗi file PDF (`max_chars`).
    pub fn with_pdf_max_chars(mut self, max_chars: Option<usize>) -> Self {
        let work_dir = self.pdf_state.work_dir.clone();
        self.pdf_state = Arc::new(
            SharedPdfState::new(
                Arc::clone(&self.engine),
                Arc::clone(&self.autoflow_notify),
                work_dir,
            )
            .with_max_chars(max_chars),
        );
        self
    }

    /// Xuất toàn bộ 14 `HostTool` của `WikiEngine` (cả `ReadOnly` và `Mutating`).
    pub fn wiki_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        wiki_host_tools(Arc::clone(&self.engine), self.read_budget)
    }

    /// Chỉ xuất các `HostTool` ở chế độ `ReadOnly` của `WikiEngine` (dành cho Flow `daily` / `wiki_researcher`).
    pub fn wiki_readonly_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        wiki_readonly_host_tools(Arc::clone(&self.engine), self.read_budget)
    }

    /// Xuất `HostTool` `add_to_sources` hợp nhất (gắn kèm `yt_env` và `pdf_state` để hỗ trợ lưu cả YouTube, PDF và Markdown tổng hợp).
    pub fn add_to_sources_host_tool(&self) -> Arc<dyn HostTool> {
        Arc::new(
            AddToSourcesTool::new(Arc::clone(&self.engine), Arc::clone(&self.autoflow_notify))
                .with_youtube_env(Arc::clone(&self.yt_env))
                .with_pdf_state(Arc::clone(&self.pdf_state)),
        )
    }

    /// Xuất cặp `HostTool` `[markdown_create, markdown_read]`.
    pub fn report_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        report_host_tools(self.reports_dir.clone())
    }

    /// Xuất bộ `HostTool` đa phương tiện & đa định dạng (`image_create`, `video_create`, `video_concat`, `tts_synthesize`, `asr_transcribe`, `artifact_create`).
    pub fn media_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        let mut tools = media_host_tools_with_video(
            Arc::clone(&self.image_backend),
            Arc::clone(&self.video_backend),
            Arc::clone(&self.tts_backend),
            Some(self.reports_dir.clone()),
        );
        tools.push(AsrTranscribeTool::with_backend(Arc::clone(&self.asr_backend)).into_host_tool());
        tools
    }

    /// Xuất cặp `HostTool` `[yt_list_playlist, yt_fetch_transcript]`.
    pub fn youtube_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        youtube_host_tools(Arc::clone(&self.yt_env))
    }

    /// Xuất cặp `HostTool` `[anydoc_inspect_pdf, anydoc_save_to_sources]`.
    pub fn pdf_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        pdf_host_tools(Arc::clone(&self.pdf_state))
    }

    /// Xuất trọn bộ 6 `HostTool` tải và trích xuất Web/HTML (`fetch_html`, `html_extract_metadata`,
    /// `html_extract_article`, `html_extract_feed`, `html_extract_links`, `html_select_elements`).
    pub fn web_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        web_host_tools(Arc::clone(&self.html_workspace))
    }

    /// Xuất toàn bộ 10 `HostTool` thu thập nguồn bên ngoài (`anydoc_inspect_pdf`, `anydoc_save_to_sources`,
    /// `yt_list_playlist`, `yt_fetch_transcript`, và 6 tool Web/HTML `fetch_html`, `html_*`).
    pub fn collector_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.pdf_host_tools();
        tools.extend(self.youtube_host_tools());
        tools.extend(self.web_host_tools());
        tools
    }

    /// Xuất bộ `HostTool` chuẩn cho **Flow 1 (`daily_orch` / `daily_root` — Thu thập thông tin & Đa chức năng)** khi tách riêng 2 instance `Orchestrator`:
    /// gồm `wiki_readonly_host_tools()`, `collector_host_tools()` (`anydoc_*` + `yt_*` + `fetch_html`/`html_*`),
    /// `add_to_sources`, `markdown_create`, `markdown_read`, `image_create`, `tts_synthesize`, `artifact_create`,
    /// và 4 tool `reminder_*` từ `TaskManagerInit`.
    pub fn daily_flow_host_tools(&self, init: &TaskManagerInit) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.wiki_readonly_host_tools();
        tools.extend(self.collector_host_tools());
        tools.push(self.add_to_sources_host_tool());
        tools.extend(self.report_host_tools());
        tools.extend(self.media_host_tools());
        tools.extend(init.reminder_host_tools());
        tools
    }

    /// Xuất bộ `HostTool` chuẩn cho **Flow 2 (`wiki_orch` / `wiki_root` — Đường ống Wiki Tự động khi `sources/` có file mới)** khi tách riêng 2 instance `Orchestrator`:
    /// chỉ gồm 14 tool `wiki_*` (đọc + ghi) của `WikiEngine`, hoàn toàn không chứa tool thu thập ngoại vi (`yt_*`, `anydoc_*`, `fetch_html`).
    ///
    /// Đây là phương thức đối xứng ngữ nghĩa với [`Self::daily_flow_host_tools`] cho mô hình triển khai 2 `Orchestrator` độc lập.
    #[inline]
    pub fn wiki_flow_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        self.wiki_host_tools()
    }

    /// Xuất toàn bộ `HostTool` chuẩn cho **1 instance `Orchestrator` hợp nhất** (chứa cả `daily_root` và `wiki_root`):
    /// gồm `anydoc_*`, `yt_*`, `fetch_html`/`html_*`, `add_to_sources`, `markdown_*`, `image_create`,
    /// `tts_synthesize`, `artifact_create`, `reminder_*` và 14 tool `wiki_*`.
    pub fn all_host_tools(&self, init: &TaskManagerInit) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.collector_host_tools();
        tools.push(self.add_to_sources_host_tool());
        tools.extend(self.report_host_tools());
        tools.extend(self.media_host_tools());
        tools.extend(init.reminder_host_tools());
        tools.extend(self.wiki_host_tools());
        tools
    }

    /// Xuất toàn bộ `HostTool` bao gồm cả 6 tool `task_*` (`task_create`, `task_followup`, `task_list`,
    /// `task_detail`, `task_cancel`, `task_delete`) cùng toàn bộ công cụ của [`Self::all_host_tools`].
    pub fn all_host_tools_with_tasks(&self, init: &TaskManagerInit) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.collector_host_tools();
        tools.push(self.add_to_sources_host_tool());
        tools.extend(self.report_host_tools());
        tools.extend(self.media_host_tools());
        tools.extend(init.all_host_tools());
        tools.extend(self.wiki_host_tools());
        tools
    }
}
