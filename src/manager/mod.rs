//! `TaskManager` & `TaskManagerInit` — Deep Module điều phối các Task, Followup,
//! Schedule/Reminder và Auto-Flow trên nhiều instance `Orchestrator`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use llm_wiki::WikiEngine;
use orchestrator_agent_rs::Orchestrator;
use tokio::task::JoinHandle;

use crate::error::TaskResult;
use crate::owner_lock::OwnerLock;
use crate::recipe::{
    validate_recipe_registry, validate_recipe_schema, RecipeProviderRuntime, TaskRecipe,
};
use crate::runtime::{SharedTaskCore, WikiAutoFlowBinding};

mod dispatch;
mod lifecycle;
mod prepare;
mod query_api;
mod schedule_api;

/// Pha 1 của quá trình khởi tạo `TaskManager` (ADR-0002).
/// Cho phép lấy các `HostTool` (`reminder_*`, `task_*`) để đăng ký vào `OrchestratorBuilder`
/// trước khi gắn các `Arc<Orchestrator>` đã build và gọi [`Self::start`].
pub struct TaskManagerInit {
    pub(super) core: Arc<SharedTaskCore>,
    pub(super) owner_lock: Option<OwnerLock>,
    pub(super) flows: BTreeMap<String, Arc<Orchestrator>>,
    pub(super) wiki_engine: Option<Arc<WikiEngine>>,
    pub(super) autoflow: Option<WikiAutoFlowBinding>,
    pub(super) recipes: BTreeMap<String, Arc<dyn TaskRecipe>>,
}

/// Deep Module quản lý vòng đời Tác vụ Nền, Hàng đợi Followup, Lịch hẹn/Reminder và Wiki Auto-Flow.
pub struct TaskManager {
    pub(super) core: Arc<SharedTaskCore>,
    pub(super) bg_tasks: Mutex<Vec<JoinHandle<()>>>,
    pub(super) owner_lock: Mutex<Option<OwnerLock>>,
    pub(super) recipes: BTreeMap<String, Arc<dyn TaskRecipe>>,
}

impl TaskManagerInit {
    /// Register a code-defined recipe before building the Orchestrator.
    pub fn register_recipe(mut self, recipe: Arc<dyn TaskRecipe>) -> TaskResult<Self> {
        let metadata = recipe.metadata();
        if metadata.id.trim().is_empty() || metadata.version == 0 || metadata.max_steps == 0 {
            return Err(crate::error::TaskError::config(
                "recipe id, version, and max_steps must be valid",
            ));
        }
        validate_recipe_schema(&metadata.params_schema).map_err(|e| {
            crate::error::TaskError::config(format!("{}: {}", e.validation_path, e.message))
        })?;
        if self.recipes.insert(metadata.id.clone(), recipe).is_some() {
            return Err(crate::error::TaskError::config(format!(
                "duplicate recipe id '{}'",
                metadata.id
            )));
        }
        Ok(self)
    }

    /// Adapter to install with `OrchestratorBuilder::with_provider_runtime`.
    pub fn recipe_provider_runtime(&self) -> TaskResult<Arc<RecipeProviderRuntime>> {
        RecipeProviderRuntime::new(self.recipes.values().cloned().collect())
            .map(Arc::new)
            .map_err(|e| {
                crate::error::TaskError::config(format!("{}: {}", e.validation_path, e.message))
            })
    }

    /// Validate explicit recipe outline dependencies against the server's complete catalogs.
    pub fn validate_recipe_registry(
        &self,
        registered_tools: &std::collections::HashSet<String>,
        registered_agents: &std::collections::HashSet<String>,
    ) -> TaskResult<()> {
        let recipes = self.recipes.values().cloned().collect::<Vec<_>>();
        validate_recipe_registry(&recipes, registered_tools, registered_agents).map_err(|e| {
            crate::error::TaskError::config(format!("{}: {}", e.validation_path, e.message))
        })
    }
}
