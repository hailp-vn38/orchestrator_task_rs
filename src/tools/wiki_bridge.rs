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

/// Công cụ `add_to_sources`: lưu bản tổng hợp Markdown vào `sources/<domain>/<topic>/<slug>.md`
/// và kích hoạt `autoflow_notify` để đánh thức `WikiAutoFlowBinding` tức thì.
///
/// ### Các cổng kiểm duyệt (học từ Javis OS `_promote_sync`):
/// 1. **Provenance Gate (`provenance`)**: Chặn cứng `provenance = "assistant"` (`PROVENANCE_BLOCKED`)
///    để ngăn hiện tượng AI tự suy đoán rồi tự hợp thức hóa thành nguồn tri thức chuẩn.
/// 2. **Secret & Prompt-Injection Gate**: Quét tĩnh `title` và `markdown_body` để chặn khóa bí mật,
///    API token hoặc chỉ thị prompt injection (`CONTENT_POLICY_BLOCKED`).
/// 3. **Minimum Substance Gate**: Từ chối nội dung quá ngắn (`< 20` ký tự thực chất) để tránh kích hoạt
///    Auto-Flow lãng phí token (`INSUFFICIENT_SUBSTANCE`).
/// 4. **Conflict & Open Questions Metadata**: Ghi nhận `target_hint`, `conflict_with` và `open_questions`
///    vào YAML frontmatter + thân bài để `wiki_ingestor` xử lý mâu thuẫn không phá hủy.
pub struct AddToSourcesTool {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
    pub yt_env: Option<Arc<crate::tools::youtube::YoutubeCollectorEnv>>,
    pub pdf_state: Option<Arc<crate::tools::pdf::SharedPdfState>>,
}

const MIN_SOURCE_SUBSTANCE_CHARS: usize = 20;

const SECRET_MARKERS: &[&str] = &[
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN OPENSSH PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "sk-ant-api",
    "sk-proj-",
    "ghp_",
    "github_pat_",
    "AIzaSy",
];

const INJECTION_MARKERS: &[&str] = &[
    "ignore all previous instructions",
    "ignore previous instructions",
    "disregard system prompt",
    "disregard all prior instructions",
    "<|im_start|>system",
    "[inst] <<sys>>",
];

fn validate_source_content_safety(title: &str, body: &str) -> Result<(), ToolError> {
    let non_ws_chars = body.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws_chars < MIN_SOURCE_SUBSTANCE_CHARS {
        return Err(host_tool_err(
            "INSUFFICIENT_SUBSTANCE",
            format!(
                "markdown_body is too short ({non_ws_chars} non-whitespace chars; minimum is {MIN_SOURCE_SUBSTANCE_CHARS})"
            ),
        ));
    }

    let combined = format!("{title}\n{body}");
    for marker in SECRET_MARKERS {
        if combined.contains(marker) {
            return Err(host_tool_err(
                "CONTENT_POLICY_BLOCKED",
                format!("Source content rejected: detected secret/credential pattern '{marker}'"),
            ));
        }
    }

    let lowered = combined.to_ascii_lowercase();
    for marker in INJECTION_MARKERS {
        if lowered.contains(marker) {
            return Err(host_tool_err(
                "CONTENT_POLICY_BLOCKED",
                format!("Source content rejected: detected prompt-injection directive '{marker}'"),
            ));
        }
    }

    Ok(())
}

fn normalize_and_validate_provenance(raw: Option<&str>) -> Result<&'static str, ToolError> {
    let val = raw.map(str::trim).filter(|s| !s.is_empty());
    match val {
        None => Ok("verified_synthesis"),
        Some(p) => {
            let lower = p.to_ascii_lowercase();
            match lower.as_str() {
                "user" => Ok("user"),
                "source" => Ok("source"),
                "verified_synthesis" | "verified" | "mixed" => Ok("verified_synthesis"),
                "assistant" | "assistant_only" | "unverified" => Err(host_tool_err(
                    "PROVENANCE_BLOCKED",
                    "Cannot stage assistant-only unverified speculation into `sources/`. Confirm claims with the user, ground them in an external source, or pass unresolved items via `open_questions` with `provenance=\"verified_synthesis\"`.",
                )),
                _ => Err(host_tool_err(
                    "INVALID_ARGUMENT",
                    format!(
                        "Invalid provenance '{p}': expected 'user', 'source', 'verified_synthesis', or 'assistant'"
                    ),
                )),
            }
        }
    }
}

impl AddToSourcesTool {
    pub fn new(engine: Arc<WikiEngine>, autoflow_notify: Arc<Notify>) -> Self {
        Self {
            engine,
            autoflow_notify,
            yt_env: None,
            pdf_state: None,
        }
    }

    pub fn with_youtube_env(
        mut self,
        yt_env: Arc<crate::tools::youtube::YoutubeCollectorEnv>,
    ) -> Self {
        self.yt_env = Some(yt_env);
        self
    }

    pub fn with_pdf_state(mut self, pdf_state: Arc<crate::tools::pdf::SharedPdfState>) -> Self {
        self.pdf_state = Some(pdf_state);
        self
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
            description: "Unified tool to save a raw or synthesized Markdown source into `sources/<domain>/<topic>/<slug>.md` and trigger Flow 2 Wiki compilation (`autoflow_notify`). ONLY call this tool when the user EXPLICITLY requests saving/adding content into `sources/` or the Wiki. Supports saving: (1) a YouTube video transcript via `video_id`, (2) a converted PDF/Office document via `pdf_doc_index` or `pdf_path`, or (3) a Web article / synthesized discussion via `title` + `markdown_body`.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["topic_folder"],
                "properties": {
                    "topic_folder": {
                        "type": "string",
                        "description": "Two-level kebab-case folder '<domain>/<topic>' inside sources/ (e.g. 'software-architecture/agent-tasks' or 'robotics/xgo-duck')."
                    },
                    "video_id": {
                        "type": "string",
                        "description": "Optional YouTube `video_id` from `yt_list_playlist` / `yt_fetch_transcript`. When provided, saves the full video transcript directly into `sources/<topic_folder>/` without needing `markdown_body`."
                    },
                    "pdf_doc_index": {
                        "type": "integer",
                        "description": "Optional 1-based `doc_index` from `anydoc_inspect_pdf` (or `doc_index`). When provided, saves the full `anydoc`-converted PDF Markdown directly into `sources/<topic_folder>/` without needing `markdown_body`."
                    },
                    "doc_index": {
                        "type": "integer",
                        "description": "Alias for `pdf_doc_index` (1-based document index from `anydoc_inspect_pdf`)."
                    },
                    "pdf_path": {
                        "type": "string",
                        "description": "Optional path to a PDF/Office file on disk to convert with `anydoc` and save directly into `sources/<topic_folder>/`."
                    },
                    "file_stem": {
                        "type": "string",
                        "description": "Optional kebab-case filename stem without .md."
                    },
                    "title": {
                        "type": "string",
                        "description": "Document title (required when saving `markdown_body`; optional override when saving `video_id` or `pdf_doc_index`/`pdf_path`)."
                    },
                    "markdown_body": {
                        "type": "string",
                        "description": "Self-contained Markdown body (>= 20 non-whitespace chars, free of secrets/prompt injection). Required when saving Web articles or synthesized discussions (omit when using `video_id` or `pdf_doc_index`/`pdf_path`)."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Hierarchical tags in kebab-case (e.g. ['topic/robotics/xgo-duck', 'robotics'])."
                    },
                    "provenance": {
                        "type": "string",
                        "enum": ["user", "source", "verified_synthesis", "assistant"],
                        "description": "Origin of the synthesized knowledge: 'user' (user-confirmed decision/fact), 'source' (grounded in external source), 'verified_synthesis' (default), or 'assistant' (blocked if unverified assistant-only speculation)."
                    },
                    "target_hint": {
                        "type": "string",
                        "description": "Optional hint for Flow 2 (`wiki_ingestor`) indicating existing Wiki pages (`entities/<slug>`, `concepts/<slug>`, `wiki/sources/...`) to update or cross-link."
                    },
                    "conflict_with": {
                        "type": "string",
                        "description": "Optional existing Wiki page path/slug that this synthesis contradicts so Flow 2 preserves both perspectives non-destructively."
                    },
                    "open_questions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of unresolved questions or unverified hypotheses to record in a dedicated Caveat / Open Questions section instead of promoting as established facts."
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
        let custom_title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let custom_stem = args
            .get("file_stem")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Nhánh 1: Lưu transcript video YouTube (`video_id`)
        if let Some(video_id) = args
            .get("video_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let yt_env = self.yt_env.as_ref().ok_or_else(|| {
                host_tool_err(
                    "YOUTUBE_ENV_UNAVAILABLE",
                    "YoutubeCollectorEnv chưa được gắn vào AddToSourcesTool",
                )
            })?;
            ctx.progress(&format!(
                "Đang lưu transcript video YouTube `{video_id}` vào `sources/{raw_topic}/`..."
            ));
            let written = yt_env
                .save_video_to_sources(video_id, raw_topic, custom_title, custom_stem, tags)
                .await?;
            return Ok(host_tool_outcome(
                &ctx,
                written.agent_output,
                json!({
                    "source_rel_path": written.info.scoped_path,
                    "vault_rel_path": written.info.rel_path,
                    "topic_folder": written.topic_folder,
                    "docid": written.info.docid,
                    "chars": written.body_char_count,
                    "lines": written.info.line_count,
                    "origin": "youtube",
                    "video_id": video_id,
                    "autoflow_triggered": true,
                }),
            ));
        }

        // Nhánh 2: Lưu tài liệu PDF/Office (`pdf_doc_index` / `doc_index` / `pdf_path`)
        let pdf_idx = args
            .get("pdf_doc_index")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0)
            .or_else(|| {
                args.get("doc_index")
                    .and_then(Value::as_u64)
                    .filter(|&n| n > 0)
            })
            .map(|n| n as usize);
        let pdf_path = args
            .get("pdf_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        let has_markdown_body = args
            .get("markdown_body")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if (pdf_idx.is_some() || pdf_path.is_some()) && !has_markdown_body {
            let pdf_state = self.pdf_state.as_ref().ok_or_else(|| {
                host_tool_err(
                    "PDF_STATE_UNAVAILABLE",
                    "SharedPdfState chưa được gắn vào AddToSourcesTool",
                )
            })?;
            ctx.progress(&format!(
                "Đang lưu tài liệu PDF vào `sources/{raw_topic}/`..."
            ));
            let written = pdf_state
                .save_doc_to_sources(
                    pdf_idx,
                    pdf_path,
                    custom_stem,
                    raw_topic,
                    custom_title,
                    tags,
                )
                .await?;
            return Ok(host_tool_outcome(
                &ctx,
                written.agent_output,
                json!({
                    "source_rel_path": written.info.scoped_path,
                    "vault_rel_path": written.info.rel_path,
                    "topic_folder": written.topic_folder,
                    "docid": written.info.docid,
                    "chars": written.body_char_count,
                    "lines": written.info.line_count,
                    "origin": "pdf",
                    "autoflow_triggered": true,
                }),
            ));
        }

        // Nhánh 3: Lưu Markdown trực tiếp (`title` + `markdown_body`)
        let title =
            custom_title.ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing title"))?;
        let body = args
            .get("markdown_body")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                host_tool_err(
                    "INVALID_ARGUMENT",
                    "Missing markdown_body (or provide video_id / pdf_doc_index / pdf_path)",
                )
            })?;

        let provenance =
            normalize_and_validate_provenance(args.get("provenance").and_then(Value::as_str))?;
        validate_source_content_safety(title, body)?;

        let topic_folder = sanitize_topic_folder(raw_topic);
        let file_stem = custom_stem
            .map(|s| slugify(s, 64))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| slugify(title, 64));

        let target_hint = args
            .get("target_hint")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let conflict_with = args
            .get("conflict_with")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let open_questions: Vec<String> = args
            .get("open_questions")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let mut extra_frontmatter = vec![
            (
                "provenance".to_string(),
                Value::String(provenance.to_string()),
            ),
            (
                "origin".to_string(),
                Value::String("daily-flow-staged".to_string()),
            ),
        ];
        let mut header_bullets = vec![("Provenance".to_string(), format!("`{provenance}`"))];

        if let Some(ref hint) = target_hint {
            extra_frontmatter.push(("target_hint".to_string(), Value::String(hint.clone())));
            header_bullets.push(("Target Wiki Hint".to_string(), hint.clone()));
        }
        if let Some(ref conflict) = conflict_with {
            extra_frontmatter.push(("conflict_with".to_string(), Value::String(conflict.clone())));
            header_bullets.push((
                "Conflict / Non-Destructive Merge Target".to_string(),
                conflict.clone(),
            ));
        }

        let mut final_body = body.trim_end().to_string();
        if conflict_with.is_some() || !open_questions.is_empty() {
            final_body.push_str("\n\n## Mâu thuẫn & Câu hỏi mở (Conflict & Open Questions)\n\n");
            final_body.push_str("> [!WARNING] Contradiction / Caveat\n");
            if let Some(ref conflict) = conflict_with {
                final_body.push_str(&format!(
                    "> - **Mâu thuẫn với trang hiện có (`{conflict}`)**: Khi biên dịch sang `wiki/`, giữ nguyên cả quan điểm cũ lẫn phát hiện mới từ nguồn này thay vì ghi đè.\n"
                ));
            }
            for q in &open_questions {
                final_body.push_str(&format!("> - **Câu hỏi mở (Chưa xác minh)**: {q}\n"));
            }
        }

        ctx.progress(&format!(
            "Staging source into sources/{topic_folder}/{file_stem}.md (provenance={provenance})..."
        ));

        let written = self
            .engine
            .write_topic_source(WriteTopicSourceRequest {
                topic_folder,
                file_stem: Some(file_stem),
                title: title.to_string(),
                tags,
                extra_frontmatter,
                header_bullets,
                body_heading: None,
                raw_body: final_body,
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
                "provenance": provenance,
                "target_hint": target_hint,
                "conflict_with": conflict_with,
                "open_questions_count": open_questions.len(),
                "autoflow_triggered": true,
            }),
        ))
    }
}
