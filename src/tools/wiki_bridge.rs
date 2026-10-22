//! Cầu nối `HostTool` cho `llm_wiki::WikiEngine` (`WikiEngineBridgeTool`)
//! và cổng tiếp nhận nguồn `add_to_sources` (`AddToSourcesTool`).

use std::sync::Arc;

use llm_wiki::{
    is_read_only_wiki_tool, sanitize_topic_folder, slugify, AdaptiveReadBudget, LlmToolSpec,
    WikiEngine, WriteTopicSourceRequest,
};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

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

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let out = self
            .engine
            .execute_llm_tool_with_budget(&self.spec.name, args, self.read_budget)
            .map_err(|e| host_tool_err("WIKI_TOOL_ERROR", e.to_string()))?;

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
/// `wiki_graph_neighbors`, `wiki_lint`, `wiki_schema`) — dùng khi tách riêng `daily_orch` (Flow 2).
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

/// Công cụ `add_to_sources`: lưu bản tổng hợp Markdown vào `sources/<domain>/<topic>/<slug>.md`
/// và kích hoạt `autoflow_notify` để đánh thức `WikiAutoFlowBinding` tức thì.
pub struct AddToSourcesTool {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
}

impl AddToSourcesTool {
    pub fn new(engine: Arc<WikiEngine>, autoflow_notify: Arc<Notify>) -> Self {
        Self {
            engine,
            autoflow_notify,
        }
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

/// Hàm tiện ích khởi tạo nhanh `Arc<dyn HostTool>` cho `add_to_sources`.
pub fn add_to_sources_host_tool(
    engine: Arc<WikiEngine>,
    autoflow_notify: Arc<Notify>,
) -> Arc<dyn HostTool> {
    Arc::new(AddToSourcesTool::new(engine, autoflow_notify))
}

#[async_trait::async_trait]
impl HostTool for AddToSourcesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "add_to_sources".to_string(),
            description: "Stage a synthesized Markdown document into `sources/<domain>/<topic>/<slug>.md` and immediately trigger the Flow 1 background Wiki compiler (`autoflow_notify`).".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["topic_folder", "title", "markdown_body"],
                "properties": {
                    "topic_folder": {
                        "type": "string",
                        "description": "Two-level kebab-case folder '<domain>/<topic>' inside sources/ (e.g. 'software-architecture/agent-tasks')."
                    },
                    "file_stem": {
                        "type": "string",
                        "description": "Optional kebab-case filename stem without .md."
                    },
                    "title": {
                        "type": "string",
                        "description": "Document title."
                    },
                    "markdown_body": {
                        "type": "string",
                        "description": "Self-contained Markdown synthesis body."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Hierarchical tags."
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::Mutating,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let raw_topic = args
            .get("topic_folder")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing topic_folder"))?;
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing title"))?;
        let body = args
            .get("markdown_body")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing markdown_body"))?;

        let topic_folder = sanitize_topic_folder(raw_topic);
        let file_stem = args
            .get("file_stem")
            .and_then(Value::as_str)
            .map(|s| slugify(s, 64))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| slugify(title, 64));
        let tags: Vec<String> = args
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        ctx.progress(&format!(
            "Staging source into sources/{topic_folder}/{file_stem}.md..."
        ));

        let written = self
            .engine
            .write_topic_source(WriteTopicSourceRequest {
                topic_folder,
                file_stem: Some(file_stem),
                title: title.to_string(),
                tags,
                extra_frontmatter: Vec::new(),
                header_bullets: Vec::new(),
                body_heading: None,
                raw_body: body.to_string(),
                preview_chars: Some(500),
            })
            .map_err(|e| host_tool_err("WRITE_SOURCE_ERROR", e.to_string()))?;

        // Kích hoạt tức thì Managed Auto-Flow Worker trong `TaskManager`
        self.autoflow_notify.notify_one();

        Ok(host_tool_outcome(
            &ctx,
            written.agent_output,
            json!({
                "source_rel_path": written.info.scoped_path,
                "vault_rel_path": written.info.rel_path,
                "docid": written.info.docid,
                "autoflow_triggered": true,
            }),
        ))
    }
}
