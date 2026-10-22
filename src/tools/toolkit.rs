//! `WorkspaceToolKit` — Bộ quản lý và khởi tạo trọn gói toàn bộ `HostTool` cho kiến trúc Dual-Flow
//! (Wiki Engine, `add_to_sources`, Markdown Reports, YouTube Playlist Collector, PDF `anydoc` Collector,
//! và Task/Reminder Tools từ `TaskManagerInit`).

use std::path::PathBuf;
use std::sync::Arc;

use llm_wiki::{AdaptiveReadBudget, WikiEngine};
use orchestrator_agent_rs::HostTool;
use tokio::sync::Notify;

use crate::config::WorkspaceLayout;
use crate::manager::TaskManagerInit;
use crate::tools::markdown_report::report_host_tools;
use crate::tools::pdf::{pdf_host_tools, SharedPdfState};
use crate::tools::wiki_bridge::{
    add_to_sources_host_tool, wiki_host_tools, wiki_readonly_host_tools,
};
use crate::tools::youtube::{youtube_host_tools, YoutubeCollectorEnv};

const DEFAULT_CONTEXT_LIMIT_TOKENS: usize = 200_000;

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
}

impl WorkspaceToolKit {
    /// Khởi tạo `WorkspaceToolKit` từ `WorkspaceLayout`, `Arc<WikiEngine>` và `Arc<Notify>`.
    ///
    /// Tự động thiết lập:
    /// - `reports_dir` = `layout.reports_dir()`
    /// - `pdf_state` làm việc tại `<root>/work/pdf`
    /// - `yt_env` làm việc tại `<root>/work/yt`
    /// - `read_budget` mặc định cho context window `200_000` tokens
    pub fn from_layout(
        layout: &WorkspaceLayout,
        engine: Arc<WikiEngine>,
        autoflow_notify: Arc<Notify>,
    ) -> Self {
        let work_dir = layout.root_dir().join("work");
        Self::new(
            engine,
            autoflow_notify,
            layout.reports_dir(),
            work_dir.join("pdf"),
            work_dir.join("yt"),
        )
    }

    /// Khởi tạo `WorkspaceToolKit` với các đường dẫn thư mục tùy chỉnh.
    pub fn new(
        engine: Arc<WikiEngine>,
        autoflow_notify: Arc<Notify>,
        reports_dir: impl Into<PathBuf>,
        pdf_work_dir: impl Into<PathBuf>,
        yt_work_dir: impl Into<PathBuf>,
    ) -> Self {
        let pdf_state = Arc::new(SharedPdfState::new(
            Arc::clone(&engine),
            Arc::clone(&autoflow_notify),
            pdf_work_dir,
        ));
        let yt_env = Arc::new(YoutubeCollectorEnv::new(
            Arc::clone(&engine),
            Arc::clone(&autoflow_notify),
            yt_work_dir,
        ));
        Self {
            engine,
            autoflow_notify,
            reports_dir: reports_dir.into(),
            read_budget: AdaptiveReadBudget::for_context_limit(DEFAULT_CONTEXT_LIMIT_TOKENS),
            pdf_state,
            yt_env,
        }
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

    /// Xuất `HostTool` `add_to_sources`.
    pub fn add_to_sources_host_tool(&self) -> Arc<dyn HostTool> {
        add_to_sources_host_tool(Arc::clone(&self.engine), Arc::clone(&self.autoflow_notify))
    }

    /// Xuất cặp `HostTool` `[markdown_create, markdown_read]`.
    pub fn report_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        report_host_tools(self.reports_dir.clone())
    }

    /// Xuất cặp `HostTool` `[yt_list_playlist, yt_fetch_transcript]`.
    pub fn youtube_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        youtube_host_tools(Arc::clone(&self.yt_env))
    }

    /// Xuất cặp `HostTool` `[anydoc_inspect_pdf, anydoc_save_to_sources]`.
    pub fn pdf_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        pdf_host_tools(Arc::clone(&self.pdf_state))
    }

    /// Xuất toàn bộ `HostTool` thu thập nguồn bên ngoài (`yt_list_playlist`, `yt_fetch_transcript`,
    /// `anydoc_inspect_pdf`, `anydoc_save_to_sources`) cho subagent `source_collector`.
    pub fn collector_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.pdf_host_tools();
        tools.extend(self.youtube_host_tools());
        tools
    }

    /// Xuất bộ `HostTool` chuẩn cho **Flow 2 (`daily_orch`)** khi tách riêng 2 instance `Orchestrator`:
    /// gồm `wiki_readonly_host_tools()`, `add_to_sources`, `markdown_create`, `markdown_read`,
    /// và 4 tool `reminder_*` từ `TaskManagerInit`.
    pub fn daily_flow_host_tools(&self, init: &TaskManagerInit) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.wiki_readonly_host_tools();
        tools.push(self.add_to_sources_host_tool());
        tools.extend(self.report_host_tools());
        tools.extend(init.reminder_host_tools());
        tools
    }

    /// Xuất bộ `HostTool` chuẩn cho **Flow 1 (`wiki_orch`)** khi tách riêng 2 instance `Orchestrator`:
    /// gồm toàn bộ 14 tool `wiki_*` (đọc + ghi) và 4 tool thu thập nguồn (`anydoc_*` + `yt_*`).
    pub fn wiki_flow_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.collector_host_tools();
        tools.extend(self.wiki_host_tools());
        tools
    }

    /// Xuất toàn bộ `HostTool` chuẩn cho **1 instance `Orchestrator` hợp nhất** (chứa cả `daily_root` và `wiki_root`):
    /// gồm `anydoc_*`, `yt_*`, `add_to_sources`, `markdown_*`, `reminder_*` và 14 tool `wiki_*`.
    pub fn all_host_tools(&self, init: &TaskManagerInit) -> Vec<Arc<dyn HostTool>> {
        let mut tools = self.collector_host_tools();
        tools.push(self.add_to_sources_host_tool());
        tools.extend(self.report_host_tools());
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
        tools.extend(init.all_host_tools());
        tools.extend(self.wiki_host_tools());
        tools
    }
}
