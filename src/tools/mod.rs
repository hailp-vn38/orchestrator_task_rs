//! Dual-Mode Task & Reminder Tools:
//! 1. Chế độ Raw JSON (`TaskToolSuite::tool_specs` + `TaskToolSuite::execute_raw`) cho Voice Agent.
//! 2. Chế độ `HostTool` (`impl orchestrator_agent_rs::HostTool`) để cắm trực tiếp vào `daily_orch`.

use std::sync::Arc;

use orchestrator_agent_rs::HostTool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::TaskResult;
use crate::runtime::SharedTaskCore;
use crate::types::{AgentCatalogEntry, TaskScope};

pub(crate) mod catalog;
pub(crate) mod host_adapter;
pub(crate) mod voice_handler;

/// Đặc tả một Tool (tương thích trực tiếp với OpenAI / Anthropic / Gemini / Voice Agent function calling).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Bộ công cụ hợp nhất phục vụ cả Voice Agent (Raw JSON) và `Orchestrator` (`HostTool`).
#[derive(Clone)]
pub struct TaskToolSuite {
    core: Arc<SharedTaskCore>,
}

impl TaskToolSuite {
    pub(crate) fn new(core: Arc<SharedTaskCore>) -> Self {
        Self { core }
    }

    /// Trả về danh sách các Primary/Root Agents khả dụng kèm mô tả từ mọi Flow đã đăng ký.
    pub fn available_agents(&self) -> Vec<AgentCatalogEntry> {
        self.core
            .flows
            .get()
            .map(|f| f.catalog().to_vec())
            .unwrap_or_default()
    }

    /// Trả về toàn bộ Tool Specs (8 tools: `task_create`, `task_followup`, `task_list`, `task_cancel`,
    /// `reminder_create`, `reminder_list`, `reminder_complete`, `reminder_delete`), trong đó
    /// `task_create` và `reminder_create` đã tự động nhúng danh sách `target_agent` và mô tả
    /// từ các `Orchestrator` để Voice Agent tự lựa chọn.
    pub fn tool_specs(&self) -> Vec<TaskToolSpec> {
        let agents = self.available_agents();
        catalog::build_all_tool_specs(&agents)
    }

    /// Chỉ lấy 4 tool `task_*` (`task_create`, `task_followup`, `task_list`, `task_cancel`) cho Voice Agent.
    pub fn voice_task_tool_specs(&self) -> Vec<TaskToolSpec> {
        let agents = self.available_agents();
        catalog::build_voice_task_specs(&agents)
    }

    /// Thực thi bất kỳ tool nào bằng Raw JSON với `default_scope` (`"personal"`, `"homelab"`).
    pub async fn execute_raw(&self, tool_name: &str, args: Value) -> TaskResult<Value> {
        let scope = self.core.config.default_scope.clone();
        self.execute_raw_scoped(&scope, tool_name, args).await
    }

    /// Thực thi bất kỳ tool nào bằng Raw JSON với `TaskScope` tùy chỉnh.
    pub async fn execute_raw_scoped(
        &self,
        scope: &TaskScope,
        tool_name: &str,
        args: Value,
    ) -> TaskResult<Value> {
        voice_handler::execute_tool_raw(&self.core, scope, tool_name, args).await
    }

    /// Xuất 4 `HostTool` dành riêng cho `reminder_planner` (`reminder_create`, `reminder_list`,
    /// `reminder_complete`, `reminder_delete`) để đăng ký vào `daily_orch` ở Pha 1 (`prepare`).
    pub fn reminder_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        host_adapter::build_reminder_host_tools(Arc::clone(&self.core))
    }

    /// Xuất toàn bộ 8 `HostTool` (`task_*` + `reminder_*`).
    pub fn all_host_tools(&self) -> Vec<Arc<dyn HostTool>> {
        host_adapter::build_all_host_tools(Arc::clone(&self.core))
    }
}
