//! Adapter biến các tool của `TaskToolSuite` thành `orchestrator_agent_rs::HostTool`
//! để đăng ký vào `daily_orch` (hoặc bất kỳ `Orchestrator` nào).

use std::sync::Arc;

use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::Value;

use crate::runtime::SharedTaskCore;
use crate::tools::catalog::{
    build_all_tool_specs, build_reminder_specs, TOOL_REMINDER_LIST, TOOL_TASK_LIST,
};
use crate::tools::voice_handler::execute_tool_raw;
use crate::tools::TaskToolSpec;
use crate::types::TaskScope;

pub(crate) struct TaskHostTool {
    core: Arc<SharedTaskCore>,
    fallback_spec: TaskToolSpec,
    effect: ToolEffect,
}

#[async_trait::async_trait]
impl HostTool for TaskHostTool {
    fn descriptor(&self) -> ToolDescriptor {
        // Nếu FlowRegistry đã được gắn (sau start()), lấy schema có danh sách Agent động;
        // nếu đang ở Pha 1 (prepare()), dùng fallback_spec hợp lệ để vượt qua validation của OrchestratorBuilder.
        let spec = if let Some(flows) = self.core.flows.get() {
            build_all_tool_specs(flows.catalog())
                .into_iter()
                .find(|s| s.name == self.fallback_spec.name)
                .unwrap_or_else(|| self.fallback_spec.clone())
        } else {
            self.fallback_spec.clone()
        };

        ToolDescriptor {
            name: spec.name,
            description: spec.description,
            input_schema: spec.parameters,
            effect: self.effect,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let scope = TaskScope::from_tool_scope(ctx.scope());
        let val = execute_tool_raw(&self.core, &scope, &self.fallback_spec.name, args)
            .await
            .map_err(|e| ToolError {
                code: e.code().as_str().to_string(),
                message: e.message().to_string(),
            })?;

        let content = serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string());
        Ok(ToolOutcome {
            call_id: ctx.scope().tool_call_id.clone(),
            content,
            metadata: val,
        })
    }
}

pub(crate) fn build_reminder_host_tools(core: Arc<SharedTaskCore>) -> Vec<Arc<dyn HostTool>> {
    let specs = build_reminder_specs(&[]);
    specs
        .into_iter()
        .map(|spec| {
            let effect = if spec.name == TOOL_REMINDER_LIST {
                ToolEffect::ReadOnly
            } else {
                ToolEffect::Mutating
            };
            Arc::new(TaskHostTool {
                core: Arc::clone(&core),
                fallback_spec: spec,
                effect,
            }) as Arc<dyn HostTool>
        })
        .collect()
}

pub(crate) fn build_all_host_tools(core: Arc<SharedTaskCore>) -> Vec<Arc<dyn HostTool>> {
    let specs = build_all_tool_specs(&[]);
    specs
        .into_iter()
        .map(|spec| {
            let effect = if spec.name == TOOL_REMINDER_LIST || spec.name == TOOL_TASK_LIST {
                ToolEffect::ReadOnly
            } else {
                ToolEffect::Mutating
            };
            Arc::new(TaskHostTool {
                core: Arc::clone(&core),
                fallback_spec: spec,
                effect,
            }) as Arc<dyn HostTool>
        })
        .collect()
}
