//! Cấu hình tĩnh cho `orchestrator_flow_rs` (ADR-0002).

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::error::{TaskError, TaskResult};
use crate::types::TaskScope;

/// Tên flow mặc định khi hệ thống dùng chung một instance `Orchestrator` duy nhất.
pub const DEFAULT_FLOW_ID: &str = "default";

/// Bố cục thư mục chuẩn từ một thư mục gốc duy nhất (`workspace_root`) trên Homelab Server.
///
/// Tự động phân bổ đường dẫn nhất quán cho:
/// - `vault/` (`WikiEngine` Obsidian Vault & `.wiki/index.sqlite`)
/// - `stores/tasks/` (`tasks.db` & `tasks.owner.lock`)
/// - `stores/orchestrator/` (`orchestrator.db` duy nhất cho cả `daily_root` & `wiki_root`)
/// - `prompts/`, `skills/`, `reports/`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLayout {
    root_dir: PathBuf,
}

impl WorkspaceLayout {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Đường dẫn Vault cho `WikiEngine::open(layout.wiki_vault_dir())` (`<root>/vault`).
    pub fn wiki_vault_dir(&self) -> PathBuf {
        self.root_dir.join("vault")
    }

    /// Đường dẫn store cho `TaskConfig` (`<root>/stores/tasks`).
    pub fn tasks_store_dir(&self) -> PathBuf {
        self.root_dir.join("stores").join("tasks")
    }

    /// Đường dẫn store cho instance `Orchestrator` hợp nhất (`<root>/stores/orchestrator`).
    pub fn orchestrator_store_dir(&self) -> PathBuf {
        self.flow_store_dir("orchestrator")
    }

    /// Đường dẫn store cho `daily_orch` nếu tách riêng (`<root>/stores/daily_orch`).
    pub fn daily_orch_store_dir(&self) -> PathBuf {
        self.flow_store_dir("daily_orch")
    }

    /// Đường dẫn store cho `wiki_orch` nếu tách riêng (`<root>/stores/wiki_orch`).
    pub fn wiki_orch_store_dir(&self) -> PathBuf {
        self.flow_store_dir("wiki_orch")
    }

    /// Đường dẫn store cho một `Orchestrator` bất kỳ theo tên (`<root>/stores/<name>`).
    pub fn flow_store_dir(&self, name: &str) -> PathBuf {
        self.root_dir.join("stores").join(name)
    }

    /// Thư mục chứa các file system prompt `.md` (`<root>/prompts`).
    pub fn prompts_dir(&self) -> PathBuf {
        self.root_dir.join("prompts")
    }

    /// Thư mục chứa `skills/<id>/SKILL.md` (`<root>/skills`).
    pub fn skills_dir(&self) -> PathBuf {
        self.root_dir.join("skills")
    }

    /// Thư mục xuất báo cáo Markdown rời của Flow 2 (`<root>/reports`).
    pub fn reports_dir(&self) -> PathBuf {
        self.root_dir.join("reports")
    }

    /// Tạo sẵn các thư mục nền tảng trên đĩa nếu chưa tồn tại.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [
            self.wiki_vault_dir(),
            self.tasks_store_dir(),
            self.orchestrator_store_dir(),
            self.prompts_dir(),
            self.skills_dir(),
            self.reports_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Định danh một Agent mục tiêu trong một Flow cụ thể (VD: `flow_id="daily"`, `agent_id="daily_root"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetAgent {
    pub flow_id: String,
    pub agent_id: String,
}

impl TargetAgent {
    pub fn new(flow_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            flow_id: flow_id.into(),
            agent_id: agent_id.into(),
        }
    }

    /// Định dạng `"flow_id/agent_id"` hoặc `"agent_id"` để dùng trong tool argument.
    pub fn canonical_key(&self) -> String {
        format!("{}/{}", self.flow_id, self.agent_id)
    }
}

/// Tham số vận hành cho các background worker của `TaskManager`.
#[derive(Debug, Clone)]
pub struct TaskRuntimeConfig {
    /// Chu kỳ tối đa của vòng lặp Scheduler khi không có lịch hẹn gần hơn.
    pub scheduler_tick: Duration,
    /// Chu kỳ dự phòng quét Auto-Flow (ngoài tín hiệu `autoflow_notify` tức thì).
    pub autoflow_poll_interval: Duration,
    /// Sức chứa của kênh broadcast `TaskEvent` cho UI View Task.
    pub event_capacity: usize,
    /// Số lượng bản ghi tối đa trên mỗi trang khi truy vấn danh sách Task hoặc Schedule.
    pub page_size: u32,
    /// Số ký tự tối đa của `output_preview` lưu trong bảng `tasks`.
    pub output_preview_chars: usize,
    /// Thời gian chờ các worker dừng hẳn khi gọi `TaskManager::shutdown()`.
    pub shutdown_grace: Duration,
}

impl Default for TaskRuntimeConfig {
    fn default() -> Self {
        Self {
            scheduler_tick: Duration::from_secs(15),
            autoflow_poll_interval: Duration::from_secs(60),
            event_capacity: 1024,
            page_size: 50,
            output_preview_chars: 2000,
            shutdown_grace: Duration::from_secs(15),
        }
    }
}

impl TaskRuntimeConfig {
    pub(crate) fn validate(&self) -> TaskResult<()> {
        if self.scheduler_tick.is_zero() {
            return Err(TaskError::config("scheduler_tick must be > 0"));
        }
        if self.autoflow_poll_interval.is_zero() {
            return Err(TaskError::config("autoflow_poll_interval must be > 0"));
        }
        if self.event_capacity == 0 {
            return Err(TaskError::config("event_capacity must be > 0"));
        }
        if self.page_size == 0 || self.page_size > 500 {
            return Err(TaskError::config("page_size must be in 1..=500"));
        }
        if self.output_preview_chars == 0 {
            return Err(TaskError::config("output_preview_chars must be > 0"));
        }
        if self.shutdown_grace.is_zero() {
            return Err(TaskError::config("shutdown_grace must be > 0"));
        }
        Ok(())
    }
}

/// Cấu hình khởi tạo `TaskManager`.
#[derive(Debug, Clone)]
pub struct TaskConfig {
    /// Thư mục chứa `tasks.db` và `tasks.owner.lock`.
    pub store_dir: PathBuf,
    /// Đường dẫn thư mục gốc của Wiki Vault (`data_root` của `WikiEngine`), nếu có.
    pub wiki_vault_dir: Option<PathBuf>,
    /// Scope mặc định (`owner_id = "personal"`, `workspace_id = "homelab"`).
    pub default_scope: TaskScope,
    /// Múi giờ IANA mặc định (mặc định: `"Asia/Ho_Chi_Minh"`).
    pub default_timezone: String,
    /// Target Agent mặc định khi caller/Voice không chỉ định rõ `target_agent`.
    pub default_target: Option<TargetAgent>,
    /// Cấu hình runtime cho các worker nền.
    pub runtime: TaskRuntimeConfig,
}

impl TaskConfig {
    pub fn new(store_dir: impl Into<PathBuf>) -> Self {
        Self {
            store_dir: store_dir.into(),
            wiki_vault_dir: None,
            default_scope: TaskScope::default(),
            default_timezone: "Asia/Ho_Chi_Minh".to_string(),
            default_target: None,
            runtime: TaskRuntimeConfig::default(),
        }
    }

    /// Khởi tạo `TaskConfig` chuẩn từ một `WorkspaceLayout` (tự động trỏ `store_dir` vào
    /// `<root>/stores/tasks` và `wiki_vault_dir` vào `<root>/vault`).
    pub fn from_layout(layout: &WorkspaceLayout) -> Self {
        Self {
            store_dir: layout.tasks_store_dir(),
            wiki_vault_dir: Some(layout.wiki_vault_dir()),
            default_scope: TaskScope::default(),
            default_timezone: "Asia/Ho_Chi_Minh".to_string(),
            default_target: None,
            runtime: TaskRuntimeConfig::default(),
        }
    }

    /// Khởi tạo `TaskConfig` trực tiếp từ một đường dẫn thư mục gốc `workspace_root`.
    pub fn from_workspace_root(root_dir: impl Into<PathBuf>) -> Self {
        let layout = WorkspaceLayout::new(root_dir);
        Self::from_layout(&layout)
    }

    pub fn with_wiki_vault_dir(mut self, vault_dir: impl Into<PathBuf>) -> Self {
        self.wiki_vault_dir = Some(vault_dir.into());
        self
    }

    pub fn with_default_scope(mut self, scope: TaskScope) -> Self {
        self.default_scope = scope;
        self
    }

    pub fn with_default_timezone(mut self, tz: impl Into<String>) -> Self {
        self.default_timezone = tz.into();
        self
    }

    pub fn with_default_target(
        mut self,
        flow_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        self.default_target = Some(TargetAgent::new(flow_id, agent_id));
        self
    }

    /// Đặt Primary Agent mặc định trên instance `Orchestrator` hợp nhất (`flow_id = "default"`).
    pub fn with_default_agent(mut self, agent_id: impl Into<String>) -> Self {
        self.default_target = Some(TargetAgent::new(DEFAULT_FLOW_ID, agent_id));
        self
    }

    pub fn with_runtime(mut self, runtime: TaskRuntimeConfig) -> Self {
        self.runtime = runtime;
        self
    }

    pub(crate) fn validate_local(&self) -> TaskResult<()> {
        if self.store_dir.as_os_str().is_empty() {
            return Err(TaskError::config("store_dir must not be empty"));
        }
        if let Some(ref vault_dir) = self.wiki_vault_dir {
            if vault_dir.as_os_str().is_empty() {
                return Err(TaskError::config(
                    "wiki_vault_dir must not be empty when set",
                ));
            }
        }
        self.default_scope.validate()?;
        if Tz::from_str(self.default_timezone.trim()).is_err() {
            return Err(TaskError::config(format!(
                "invalid default_timezone '{}'",
                self.default_timezone
            )));
        }
        if let Some(ref target) = self.default_target {
            if target.flow_id.trim().is_empty() || target.agent_id.trim().is_empty() {
                return Err(TaskError::config(
                    "default_target flow_id and agent_id must not be empty",
                ));
            }
        }
        self.runtime.validate()
    }
}
