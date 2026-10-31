//! Cầu nối `HostTool` cho `llm_wiki::WikiEngine` (`WikiEngineBridgeTool`)
//! và tái xuất cổng tiếp nhận nguồn `add_to_sources` (`AddToSourcesTool`).

use std::sync::Arc;

use llm_wiki::{is_read_only_wiki_tool, AdaptiveReadBudget, LlmToolSpec, WikiEngine};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::Value;

pub use crate::tools::add_to_sources::{add_to_sources_host_tool, AddToSourcesTool};

pub(crate) fn host_tool_err(code: &str, msg: impl Into<String>) -> ToolError {
    ToolError {
        code: code.into(),
        message: msg.into(),
    }
}

pub(crate) fn host_tool_outcome(
    ctx: &ToolContext,
    content: String,
    metadata: Value,
) -> ToolOutcome {
    ToolOutcome {
        call_id: ctx.scope().tool_call_id.clone(),
        content,
        metadata,
    }
}

/// Adapter biến một `LlmToolSpec` của `WikiEngine` thành `orchestrator_agent_rs::HostTool`.
pub struct WikiEngineBridgeTool {
    pub engine: Arc<WikiEngine>,
    pub spec: LlmToolSpec,
    pub effect: ToolEffect,
    pub read_budget: AdaptiveReadBudget,
}

impl WikiEngineBridgeTool {
    /// Tạo một `WikiEngineBridgeTool` và tự động suy diễn `ToolEffect` (`ReadOnly` hoặc `Mutating`)
    /// thông qua `llm_wiki::is_read_only_wiki_tool`.
    pub fn new(
        engine: Arc<WikiEngine>,
        spec: LlmToolSpec,
        read_budget: AdaptiveReadBudget,
    ) -> Self {
        let effect = if is_read_only_wiki_tool(&spec.name) {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::Mutating
        };
        Self {
            engine,
            spec,
            effect,
            read_budget,
        }
    }
}

#[async_trait::async_trait]
impl HostTool for WikiEngineBridgeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.spec.name.clone(),
            description: self.spec.description.clone(),
            input_schema: self.spec.parameters.clone(),
            effect: self.effect,
        }
    }

    async fn execute(&self, ctx: ToolContext, mut args: Value) -> Result<ToolOutcome, ToolError> {
        if self.spec.name == "wiki_tree" {
            if let Some(obj) = args.as_object_mut() {
                if let Some(sub) = obj.get("sub_path").and_then(Value::as_str) {
                    let trimmed = sub.trim().trim_matches('/');
                    if trimmed.is_empty() || trimmed == "." {
                        obj.remove("sub_path");
                    }
                }
            }
        }
        let mut out = self
            .engine
            .execute_llm_tool_with_budget(&self.spec.name, args, self.read_budget)
            .map_err(|e| host_tool_err("WIKI_TOOL_ERROR", e.to_string()))?;

        if self.effect == ToolEffect::Mutating {
            if let Some(obj) = out.metadata.as_object_mut() {
                obj.insert("mutating".to_string(), Value::Bool(true));
            }
        }

        if let Some(ref msg) = out.progress_message {
            ctx.progress(msg);
        }
        Ok(host_tool_outcome(&ctx, out.content, out.metadata))
    }
}

/// Xuất toàn bộ 14 `HostTool` native của `WikiEngine` (cả `ReadOnly` và `Mutating`).
pub fn wiki_host_tools(
    engine: Arc<WikiEngine>,
    read_budget: AdaptiveReadBudget,
) -> Vec<Arc<dyn HostTool>> {
    engine
        .llm_tool_definitions()
        .into_iter()
        .map(|spec| {
            Arc::new(WikiEngineBridgeTool::new(
                Arc::clone(&engine),
                spec,
                read_budget,
            )) as Arc<dyn HostTool>
        })
        .collect()
}

/// Chỉ xuất các `HostTool` ở chế độ `ReadOnly` của `WikiEngine`
/// (`wiki_tree`, `wiki_query`, `wiki_get`, `wiki_multi_get`, `wiki_metadata`, `wiki_status`,
/// `wiki_graph_neighbors`, `wiki_lint`, `wiki_schema`) — dùng khi tách riêng `daily_orch` (Flow 1).
pub fn wiki_readonly_host_tools(
    engine: Arc<WikiEngine>,
    read_budget: AdaptiveReadBudget,
) -> Vec<Arc<dyn HostTool>> {
    engine
        .llm_tool_definitions()
        .into_iter()
        .filter(|spec| is_read_only_wiki_tool(&spec.name))
        .map(|spec| {
            Arc::new(WikiEngineBridgeTool {
                engine: Arc::clone(&engine),
                spec,
                effect: ToolEffect::ReadOnly,
                read_budget,
            }) as Arc<dyn HostTool>
        })
        .collect()
}
