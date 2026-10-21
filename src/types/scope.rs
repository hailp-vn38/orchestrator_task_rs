//! Scope cô lập dữ liệu `(owner_id, workspace_id)` (mặc định: `"personal"`, `"homelab"`).

use orchestrator_agent_rs::{CallerContext, ToolScope};
use serde::{Deserialize, Serialize};

use crate::error::{TaskError, TaskResult};

pub const DEFAULT_OWNER_ID: &str = "personal";
pub const DEFAULT_WORKSPACE_ID: &str = "homelab";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskScope {
    pub owner_id: String,
    pub workspace_id: String,
}

impl Default for TaskScope {
    fn default() -> Self {
        Self {
            owner_id: DEFAULT_OWNER_ID.to_string(),
            workspace_id: DEFAULT_WORKSPACE_ID.to_string(),
        }
    }
}

impl TaskScope {
    pub fn new(owner_id: impl Into<String>, workspace_id: impl Into<String>) -> Self {
        Self {
            owner_id: owner_id.into(),
            workspace_id: workspace_id.into(),
        }
    }

    pub fn to_caller_context(&self) -> CallerContext {
        CallerContext::new(self.owner_id.clone(), self.workspace_id.clone())
    }

    pub fn from_tool_scope(scope: &ToolScope) -> Self {
        Self {
            owner_id: scope.owner_id.clone(),
            workspace_id: scope.workspace_id.clone(),
        }
    }

    pub(crate) fn validate(&self) -> TaskResult<()> {
        if self.owner_id.trim().is_empty() || self.workspace_id.trim().is_empty() {
            return Err(TaskError::invalid_input(
                "owner_id and workspace_id must not be empty",
            ));
        }
        Ok(())
    }
}
