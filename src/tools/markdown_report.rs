//! Bộ công cụ `HostTool` xuất và đọc báo cáo Markdown độc lập (`markdown_create`, `markdown_read`)
//! trong thư mục `reports/` (hoặc `WorkspaceLayout::reports_dir()`).

use std::path::PathBuf;
use std::sync::Arc;

use llm_wiki::slugify;
use orchestrator_agent_rs::{
    ArtifactInput, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};

use crate::config::WorkspaceLayout;
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Công cụ `markdown_create`: tạo file báo cáo Markdown độc lập trong `reports_dir`.
pub struct MarkdownCreateTool {
    pub reports_dir: PathBuf,
}

impl MarkdownCreateTool {
    pub fn new(reports_dir: impl Into<PathBuf>) -> Self {
        Self {
            reports_dir: reports_dir.into(),
        }
    }

    pub fn from_layout(layout: &WorkspaceLayout) -> Self {
        Self::new(layout.reports_dir())
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

#[async_trait::async_trait]
impl HostTool for MarkdownCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "markdown_create".to_string(),
            description:
                "Create a standalone Markdown report file in the workspace reports/ directory."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filename", "title", "markdown_body"],
                "properties": {
                    "filename": { "type": "string", "description": "Filename (e.g. 'daily-report.md')." },
                    "title": { "type": "string", "description": "Report title." },
                    "markdown_body": { "type": "string", "description": "Full Markdown report content." }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::Mutating,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let filename = args
            .get("filename")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Report");
        let body = args
            .get("markdown_body")
            .and_then(Value::as_str)
            .unwrap_or("");

        let stem = slugify(filename.trim_end_matches(".md"), 64);
        let safe_name = format!("{}.md", if stem.is_empty() { "report" } else { &stem });
        std::fs::create_dir_all(&self.reports_dir)
            .map_err(|e| host_tool_err("IO_ERROR", e.to_string()))?;
        let full_path = self.reports_dir.join(&safe_name);
        let normalized_body = llm_wiki::normalize_media_markdown_links(body.trim());
        let trimmed_body = normalized_body.trim();
        let content = if trimmed_body.starts_with("# ") {
            format!("{trimmed_body}\n")
        } else {
            format!("# {title}\n\n{trimmed_body}\n")
        };
        std::fs::write(&full_path, &content)
            .map_err(|e| host_tool_err("IO_ERROR", e.to_string()))?;

        let artifact = ctx
            .artifacts()
            .put(ArtifactInput::primary_bytes(
                &safe_name,
                "text/markdown; charset=utf-8",
                content.as_bytes().to_vec(),
            ))
            .await?;

        Ok(host_tool_outcome(
            &ctx,
            format!(
                "Saved report to {} (artifact_id={})",
                full_path.display(),
                artifact.id
            ),
            json!({
                "filename": safe_name,
                "path": full_path.display().to_string(),
                "bytes": content.len(),
                "artifact_id": artifact.id,
                "artifact_ids": [artifact.id],
                "artifact_kind": "report",
                "mime_type": artifact.mime_type,
                "sha256": artifact.sha256
            }),
        ))
    }
}

/// Công cụ `markdown_read`: đọc lại file báo cáo Markdown từ `reports_dir`.
pub struct MarkdownReadTool {
    pub reports_dir: PathBuf,
}

impl MarkdownReadTool {
    pub fn new(reports_dir: impl Into<PathBuf>) -> Self {
        Self {
            reports_dir: reports_dir.into(),
        }
    }

    pub fn from_layout(layout: &WorkspaceLayout) -> Self {
        Self::new(layout.reports_dir())
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

#[async_trait::async_trait]
impl HostTool for MarkdownReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "markdown_read".to_string(),
            description: "Read back a Markdown report file from the workspace reports/ directory."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filename"],
                "properties": {
                    "filename": { "type": "string", "description": "Filename in reports/." }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let filename = args
            .get("filename")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let stem = slugify(filename.trim_end_matches(".md"), 64);
        let safe_name = format!("{}.md", if stem.is_empty() { "report" } else { &stem });
        let full_path = self.reports_dir.join(&safe_name);
        let content = std::fs::read_to_string(&full_path)
            .map_err(|e| host_tool_err("READ_ERROR", e.to_string()))?;

        Ok(host_tool_outcome(
            &ctx,
            content.clone(),
            json!({
                "filename": safe_name,
                "bytes": content.len()
            }),
        ))
    }
}

/// Xuất cặp `HostTool` `[markdown_create, markdown_read]` gắn với `reports_dir`.
pub fn report_host_tools(reports_dir: impl Into<PathBuf>) -> Vec<Arc<dyn HostTool>> {
    let dir = reports_dir.into();
    vec![
        Arc::new(MarkdownCreateTool::new(dir.clone())),
        Arc::new(MarkdownReadTool::new(dir)),
    ]
}
