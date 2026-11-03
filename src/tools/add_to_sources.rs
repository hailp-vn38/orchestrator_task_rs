//! Cổng tiếp nhận nguồn hợp nhất `add_to_sources` (`AddToSourcesTool`) cho mô hình Dual-Flow,
//! hỗ trợ lưu transcript YouTube, tài liệu PDF (`anydoc`) và bản tổng hợp Markdown kèm các cổng
//! kiểm duyệt Provenance, Secret/Prompt-Injection và Minimum Substance.

use std::sync::Arc;

use llm_wiki::{sanitize_topic_folder, slugify, WikiEngine, WriteTopicSourceRequest};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::tools::source_identity::{
    content_sha256, existing_source_tool_outcome, resolve_content_target, ContentTarget,
    ExistingReason, SaveSourceOutcome, CONTENT_HASH_KEY,
};
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

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
                    "force": {
                        "type": "boolean",
                        "description": "Rewrite a source that already exists and re-ingest it. Only when the user explicitly asks to redo it; otherwise an existing source is reported as SOURCE_ALREADY_EXISTS and nothing is written."
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

        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        let defer_wiki_ingest = args
            .get("defer_wiki_ingest")
            .and_then(Value::as_bool)
            .unwrap_or(false);

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
                .save_video_to_sources_with_autoflow(
                    video_id,
                    raw_topic,
                    custom_title,
                    custom_stem,
                    tags,
                    force,
                    !defer_wiki_ingest,
                )
                .await?;
            let written = match written {
                SaveSourceOutcome::Written(written) => written,
                SaveSourceOutcome::Existing { found, reason } => {
                    return Ok(existing_source_tool_outcome(&ctx, &found, reason));
                }
            };
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
                    "autoflow_triggered": !defer_wiki_ingest,
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
                    force,
                )
                .await?;
            let written = match written {
                SaveSourceOutcome::Written(written) => written,
                SaveSourceOutcome::Existing { found, reason } => {
                    return Ok(existing_source_tool_outcome(&ctx, &found, reason));
                }
            };
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

        let content_hash = content_sha256(body);
        let (topic_folder, file_stem) = match resolve_content_target(
            &self.engine,
            topic_folder,
            file_stem,
            &content_hash,
            force,
        )? {
            ContentTarget::Existing(found) => {
                return Ok(existing_source_tool_outcome(
                    &ctx,
                    &found,
                    ExistingReason::SameIdentity,
                ));
            }
            ContentTarget::Write { topic_folder, stem } => (topic_folder, stem),
        };

        let mut extra_frontmatter = vec![
            (
                CONTENT_HASH_KEY.to_string(),
                Value::String(content_hash.clone()),
            ),
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
        if !defer_wiki_ingest {
            self.autoflow_notify.notify_one();
        }

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
                "autoflow_triggered": !defer_wiki_ingest,
            }),
        ))
    }
}
