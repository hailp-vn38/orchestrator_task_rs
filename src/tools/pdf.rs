//! Bộ chuyển đổi tài liệu PDF (và DOCX/PPTX/EPUB) sang GitHub-Flavored Markdown bằng `anydoc`
//! kèm 2 `HostTool` (`anydoc_inspect_pdf` và `anydoc_save_to_sources`) dành cho subagent `source_collector`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use llm_wiki::{
    clip_transcript, sanitize_topic_folder, slugify, WikiEngine, WriteTopicSourceRequest,
};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::tools::source_identity::{
    content_sha256, existing_source_tool_outcome, stem_free_of_collision, ExistingReason,
    SaveSourceOutcome, CONTENT_HASH_KEY,
};
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

pub use crate::tools::pdf_convert::{build_sample_technical_pdf, convert_document_with_anydoc};

const COLLECTOR_PREVIEW_CHARS: usize = 900;
const READONLY_PDF_MAX_CHARS: usize = 32_000;

/// Kết quả chuyển đổi một tài liệu PDF/Office sang GitHub-Flavored Markdown bằng `anydoc`.
#[derive(Clone, Debug)]
pub struct ConvertedPdfDoc {
    pub index: usize,
    pub source_path: PathBuf,
    pub file_name: String,
    pub file_stem: String,
    pub format_name: &'static str,
    pub inferred_title: String,
    pub outline_headings: Vec<String>,
    pub markdown: String,
    pub char_count: usize,
    pub line_count: usize,
    pub skipped_ocr_pages: Vec<u32>,
    pub total_pdf_pages: Option<u32>,
    pub saved_source_scoped_path: Option<String>,
}

/// Trạng thái dùng chung giữa `AnydocInspectPdfTool` và `AnydocSaveToSourcesTool`.
pub struct SharedPdfState {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
    pub work_dir: PathBuf,
    pub max_chars: Option<usize>,
    pub input_paths: Mutex<Vec<PathBuf>>,
    pub docs: Mutex<Vec<ConvertedPdfDoc>>,
}

impl SharedPdfState {
    pub fn new(
        engine: Arc<WikiEngine>,
        autoflow_notify: Arc<Notify>,
        work_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            engine,
            autoflow_notify,
            work_dir: work_dir.into(),
            max_chars: Some(32_000),
            input_paths: Mutex::new(Vec::new()),
            docs: Mutex::new(Vec::new()),
        }
    }

    /// Tùy chỉnh giới hạn số ký tự tối đa trích xuất từ mỗi tài liệu PDF (`None` = không giới hạn).
    pub fn with_max_chars(mut self, max_chars: Option<usize>) -> Self {
        self.max_chars = max_chars;
        self
    }

    /// Đăng ký trước danh sách đường dẫn file PDF cần xử lý cho phiên hiện tại.
    pub fn set_input_paths(&self, paths: Vec<PathBuf>) {
        *self.input_paths.lock().unwrap_or_else(|e| e.into_inner()) = paths;
        self.docs.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Lấy bản sao danh sách các tài liệu đã chuyển đổi trong phiên hiện tại.
    pub fn recorded_docs(&self) -> Vec<ConvertedPdfDoc> {
        self.docs.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Xuất cặp `HostTool` `[anydoc_inspect_pdf, anydoc_save_to_sources]` gắn với `Arc<SharedPdfState>`.
    pub fn host_tools(self: &Arc<Self>) -> Vec<Arc<dyn HostTool>> {
        pdf_host_tools(Arc::clone(self))
    }

    pub fn ensure_converted(
        &self,
        extra_path: Option<PathBuf>,
    ) -> std::result::Result<Vec<ConvertedPdfDoc>, ToolError> {
        if let Some(p) = extra_path {
            let mut paths = self.input_paths.lock().unwrap_or_else(|e| e.into_inner());
            if !paths.contains(&p) {
                paths.push(p);
                self.docs.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
        }

        {
            let guard = self.docs.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.is_empty() {
                return Ok(guard.clone());
            }
        }

        let paths = self
            .input_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if paths.is_empty() {
            return Err(host_tool_err(
                "NO_PDF_INPUT",
                "Chưa có đường dẫn file PDF nào được cung cấp (truyền `pdf_path` hoặc đăng ký qua `set_input_paths`).",
            ));
        }

        let _ = std::fs::create_dir_all(&self.work_dir);
        let mut converted = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let doc = convert_document_with_anydoc(i + 1, path, self.max_chars)
                .map_err(|e| host_tool_err("ANYDOC_CONVERT_ERROR", e.to_string()))?;
            let snapshot_path = self.work_dir.join(format!("{}.anydoc.md", doc.file_stem));
            let _ = std::fs::write(&snapshot_path, &doc.markdown);
            converted.push(doc);
        }

        let mut guard = self.docs.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_empty() {
            *guard = converted;
        }
        Ok(guard.clone())
    }

    /// Lưu tài liệu PDF đã chuyển đổi vào `sources/<topic_folder>/<stem>.md` và kích hoạt `autoflow_notify`.
    pub async fn save_doc_to_sources(
        self: &Arc<Self>,
        requested_idx: Option<usize>,
        extra_path: Option<PathBuf>,
        requested_stem: Option<&str>,
        raw_topic: &str,
        custom_title: Option<&str>,
        user_tags: Vec<String>,
        force: bool,
    ) -> std::result::Result<SaveSourceOutcome, ToolError> {
        let state = Arc::clone(self);
        let docs = tokio::task::spawn_blocking(move || state.ensure_converted(extra_path))
            .await
            .map_err(|e| host_tool_err("INTERNAL", e.to_string()))??;

        let clean_requested_stem = requested_stem.map(str::trim).filter(|s| !s.is_empty());

        let doc = docs
            .iter()
            .find(|d| {
                if let Some(idx) = requested_idx {
                    d.index == idx
                } else if let Some(st) = clean_requested_stem {
                    d.file_stem == slugify(st, 48) || d.file_name == st
                } else {
                    d.index == 1
                }
            })
            .cloned()
            .ok_or_else(|| {
                host_tool_err(
                    "INVALID_ARGUMENT",
                    format!(
                        "Không tìm thấy PDF khớp với doc_index={requested_idx:?} / file_stem={clean_requested_stem:?}"
                    ),
                )
            })?;

        let topic_folder = sanitize_topic_folder(raw_topic);
        let title = custom_title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&doc.inferred_title)
            .to_string();

        let stem = clean_requested_stem
            .map(|s| slugify(s, 48))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| doc.file_stem.clone());

        let content_hash = content_sha256(&doc.markdown);
        if !force {
            let known = self
                .engine
                .find_source_by_frontmatter(CONTENT_HASH_KEY, &content_hash)
                .map_err(|e| host_tool_err("WIKI_SOURCE_LOOKUP_FAILED", e.to_string()))?;
            if let Some(found) = known {
                return Ok(SaveSourceOutcome::Existing {
                    found,
                    reason: ExistingReason::SameIdentity,
                });
            }
        }
        let stem = if force {
            stem
        } else {
            stem_free_of_collision(
                &self.engine,
                &topic_folder,
                &stem,
                (CONTENT_HASH_KEY, &content_hash),
                &content_hash[..12],
            )
        };

        let mut tags = vec!["pdf".to_string(), "anydoc".to_string()];
        for clean in user_tags {
            let clean = clean.trim().to_string();
            if !clean.is_empty() && !tags.contains(&clean) {
                tags.push(clean);
            }
        }

        let mut extra_frontmatter = vec![
            ("source_format".to_string(), json!(doc.format_name)),
            ("converter".to_string(), json!("anydoc")),
            ("original_file".to_string(), json!(doc.file_name)),
            (CONTENT_HASH_KEY.to_string(), json!(content_hash)),
        ];
        let mut header_bullets = vec![
            ("File gốc".to_string(), format!("`{}`", doc.file_name)),
            (
                "Bộ chuyển đổi".to_string(),
                format!("`anydoc` (`{}` -> GFM Markdown)", doc.format_name),
            ),
        ];
        if let Some(total_pages) = doc.total_pdf_pages {
            extra_frontmatter.push(("total_pdf_pages".to_string(), json!(total_pages)));
            header_bullets.push((
                "Số trang PDF".to_string(),
                format!(
                    "{}/{} trang có text (bỏ qua trang thuần ảnh: {:?})",
                    total_pages.saturating_sub(doc.skipped_ocr_pages.len() as u32),
                    total_pages,
                    doc.skipped_ocr_pages
                ),
            ));
        }

        let written = self
            .engine
            .write_topic_source(WriteTopicSourceRequest {
                topic_folder,
                file_stem: Some(stem),
                title,
                raw_body: doc.markdown.clone(),
                body_heading: Some("Extracted Document Markdown (via anydoc)".to_string()),
                tags,
                extra_frontmatter,
                header_bullets,
                preview_chars: Some(COLLECTOR_PREVIEW_CHARS),
            })
            .map_err(|e| host_tool_err("WIKI_SOURCE_WRITE_FAILED", e.to_string()))?;

        if let Some(entry) = self
            .docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|d| d.index == doc.index)
        {
            entry.saved_source_scoped_path = Some(written.info.scoped_path.clone());
        }

        self.autoflow_notify.notify_one();
        Ok(SaveSourceOutcome::Written(Box::new(written)))
    }
}

/// Xuất `HostTool` `[anydoc_inspect_pdf]` (ReadOnly) từ `Arc<SharedPdfState>`.
/// Việc lưu vào `sources/` được gom về công cụ hợp nhất `add_to_sources`.
pub fn pdf_host_tools(state: Arc<SharedPdfState>) -> Vec<Arc<dyn HostTool>> {
    vec![Arc::new(AnydocInspectPdfTool { state })]
}

/// Công cụ `anydoc_inspect_pdf`: chuyển đổi tài liệu PDF/Office sang Markdown bằng `anydoc`
/// ở chế độ `ReadOnly` và trả về tiêu đề, mục lục đề mục cùng toàn bộ nội dung Markdown để tóm tắt/hỏi đáp.
pub struct AnydocInspectPdfTool {
    pub state: Arc<SharedPdfState>,
}

impl AnydocInspectPdfTool {
    pub fn new(state: Arc<SharedPdfState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl HostTool for AnydocInspectPdfTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "anydoc_inspect_pdf".into(),
            description: "Read-only tool: convert input PDF/document file(s) to GitHub-Flavored Markdown using `anydoc` and return `doc_index`, `file_name`, `file_stem`, `inferred_title`, `outline_headings`, character/line counts, and the extracted Markdown content for summarization or Q&A. Does NOT write to `sources/`. (To save a PDF into `sources/` when explicitly requested, call `add_to_sources` with `pdf_doc_index` or `pdf_path`).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pdf_path": {
                        "type": "string",
                        "description": "Optional path to a PDF file on disk to inspect."
                    },
                    "doc_index": {
                        "type": "integer",
                        "description": "Optional 1-based document index to inspect (omit to inspect all loaded PDF files)."
                    }
                }
            }),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let filter_idx = args
            .get("doc_index")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0)
            .map(|n| n as usize);
        let extra_path = args
            .get("pdf_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        ctx.progress("Đang trích xuất Markdown từ PDF bằng anydoc...");
        let state = self.state.clone();
        let docs = tokio::task::spawn_blocking(move || state.ensure_converted(extra_path))
            .await
            .map_err(|e| host_tool_err("INTERNAL", e.to_string()))??;

        let mut out = String::new();
        for doc in &docs {
            if let Some(target_idx) = filter_idx {
                if doc.index != target_idx {
                    continue;
                }
            }
            let headings_str = if doc.outline_headings.is_empty() {
                "_(Không có heading cấp 1–3)_".to_string()
            } else {
                doc.outline_headings.join("\n  ")
            };
            let content = clip_transcript(&doc.markdown, READONLY_PDF_MAX_CHARS);
            out.push_str(&format!(
                "=== DOCUMENT #{idx} ===\n\
                 - source_path     : {spath}\n\
                 - file_name       : {fname}\n\
                 - file_stem       : {stem}\n\
                 - detected_format : {fmt}\n\
                 - inferred_title  : {title}\n\
                 - char_count      : {chars}\n\
                 - line_count      : {lines}\n\
                 - outline_headings:\n  {headings}\n\
                 --- EXTRACTED MARKDOWN ---\n\
                 {content}\n\
                 --- END MARKDOWN ---\n\n",
                idx = doc.index,
                spath = doc.source_path.display(),
                fname = doc.file_name,
                stem = doc.file_stem,
                fmt = doc.format_name,
                title = doc.inferred_title,
                chars = doc.char_count,
                lines = doc.line_count,
                headings = headings_str,
                content = content
            ));
        }

        if out.is_empty() {
            return Err(host_tool_err(
                "NOT_FOUND",
                format!("Không tìm thấy tài liệu với doc_index={filter_idx:?}"),
            ));
        }

        Ok(host_tool_outcome(&ctx, out, json!({ "count": docs.len() })))
    }
}

/// Công cụ legacy `anydoc_save_to_sources` (đã được hợp nhất vào `add_to_sources` qua tham số `pdf_doc_index` / `pdf_path`).
pub struct AnydocSaveToSourcesTool {
    pub state: Arc<SharedPdfState>,
}

impl AnydocSaveToSourcesTool {
    pub fn new(state: Arc<SharedPdfState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl HostTool for AnydocSaveToSourcesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "anydoc_save_to_sources".into(),
            description: "Save the `anydoc`-converted Markdown of a PDF document into `sources/<topic_folder>/<file_stem>.md` organized by `<domain>/<topic>` with hierarchical tags and trigger Managed Wiki Auto-Flow.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["topic_folder"],
                "properties": {
                    "doc_index": {
                        "type": "integer",
                        "description": "1-based `doc_index` from `anydoc_inspect_pdf` (defaults to 1)."
                    },
                    "file_stem": {
                        "type": "string",
                        "description": "Optional explicit file_stem (defaults to the PDF's `file_stem`)."
                    },
                    "topic_folder": {
                        "type": "string",
                        "description": "Topic folder path inside `sources/` in `kebab-case` (`<domain>/<topic>`, e.g. 'ai-engineering/hybrid-search')."
                    },
                    "title": {
                        "type": "string",
                        "description": "Human-readable document title."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Hierarchical topic and domain tags in `kebab-case` (e.g. ['topic/ai-engineering/hybrid-search', 'pdf', 'bm25', 'rrf'])."
                    }
                }
            }),
            effect: ToolEffect::Mutating,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let requested_idx = args
            .get("doc_index")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0)
            .map(|n| n as usize);
        let requested_stem = args.get("file_stem").and_then(Value::as_str);
        let raw_topic = args
            .get("topic_folder")
            .and_then(Value::as_str)
            .unwrap_or("general/pdf-docs");
        let custom_title = args.get("title").and_then(Value::as_str);
        let user_tags: Vec<String> = args
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let written = self
            .state
            .save_doc_to_sources(
                requested_idx,
                None,
                requested_stem,
                raw_topic,
                custom_title,
                user_tags,
                false,
            )
            .await?;
        let written = match written {
            SaveSourceOutcome::Written(written) => written,
            SaveSourceOutcome::Existing { found, reason } => {
                return Ok(existing_source_tool_outcome(&ctx, &found, reason));
            }
        };

        ctx.progress(&format!(
            "Đã lưu PDF Markdown vào `{}` (docid: {})",
            written.info.rel_path, written.info.docid
        ));

        Ok(host_tool_outcome(
            &ctx,
            written.agent_output,
            json!({
                "source_rel_path": written.info.scoped_path,
                "vault_rel_path": written.info.rel_path,
                "topic_folder": written.topic_folder,
                "docid": written.info.docid,
                "chars": written.body_char_count,
                "lines": written.info.line_count,
                "autoflow_triggered": true
            }),
        ))
    }
}
