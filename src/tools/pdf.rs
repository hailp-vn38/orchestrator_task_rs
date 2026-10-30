//! Bộ chuyển đổi tài liệu PDF (và DOCX/PPTX/EPUB) sang GitHub-Flavored Markdown bằng `anydoc`
//! kèm 2 `HostTool` (`anydoc_inspect_pdf` và `anydoc_save_to_sources`) dành cho subagent `source_collector`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anydoc::{ConvertError, Format};
use llm_wiki::{
    clip_transcript, sanitize_topic_folder, slugify, WikiEngine, WriteTopicSourceRequest,
    WriteTopicSourceResponse,
};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

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
    ) -> std::result::Result<WriteTopicSourceResponse, ToolError> {
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
        Ok(written)
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
            )
            .await?;

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

fn format_label(format: Format) -> &'static str {
    match format {
        Format::Pdf => "pdf",
        Format::Doc => "doc",
        Format::Docx => "docx",
        Format::Odt => "odt",
        Format::Ppt => "ppt",
        Format::Pptx => "pptx",
        Format::Rtf => "rtf",
        Format::Epub => "epub",
        Format::Excel => "excel",
        Format::Ods => "ods",
        Format::Odp => "odp",
        Format::Csv => "csv",
    }
}

/// Chuyển đổi một tài liệu PDF/Office tại `path` sang GitHub-Flavored Markdown bằng `anydoc`.
pub fn convert_document_with_anydoc(
    index: usize,
    path: &Path,
    max_chars: Option<usize>,
) -> io::Result<ConvertedPdfDoc> {
    let bytes = std::fs::read(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Không thể đọc file `{}`: {e}", path.display()),
        )
    })?;
    if bytes.is_empty() {
        return Err(io::Error::other(format!("File rỗng: {}", path.display())));
    }

    let format = Format::from_bytes(&bytes)
        .or_else(|| Format::from_path(path))
        .ok_or_else(|| {
            io::Error::other(format!(
                "anydoc không nhận diện được định dạng của file `{}`",
                path.display()
            ))
        })?;

    let (raw_markdown, skipped_ocr_pages, total_pdf_pages) =
        extract_markdown_with_anydoc(&bytes, format, path)?;

    let raw_file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document.pdf")
        .to_string();
    let raw_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let clean_stem = slugify(raw_stem, 48);
    let file_stem = if clean_stem.is_empty() {
        format!("doc-{index:02}")
    } else {
        clean_stem
    };

    let (normalized_md, inferred_title, outline_headings) =
        normalize_anydoc_markdown(&raw_markdown, raw_stem, max_chars);
    let char_count = normalized_md.chars().count();
    let line_count = normalized_md.lines().count().max(1);

    Ok(ConvertedPdfDoc {
        index,
        source_path: path.to_path_buf(),
        file_name: raw_file_name,
        file_stem,
        format_name: format_label(format),
        inferred_title,
        outline_headings,
        markdown: normalized_md,
        char_count,
        line_count,
        skipped_ocr_pages,
        total_pdf_pages,
        saved_source_scoped_path: None,
    })
}

fn extract_markdown_with_anydoc(
    bytes: &[u8],
    format: Format,
    path: &Path,
) -> io::Result<(String, Vec<u32>, Option<u32>)> {
    match anydoc::to_markdown_bytes(bytes, format) {
        Ok(md) => Ok((md, Vec::new(), None)),
        Err(ConvertError::NeedsOcr { pages, page_count }) if format == Format::Pdf => {
            if pages.len() >= page_count as usize {
                return Err(io::Error::other(format!(
                    "Toàn bộ {page_count} trang của PDF `{}` là ảnh scan (cần OCR)",
                    path.display()
                )));
            }

            if let Ok(mut doc) = lopdf::Document::load_mem(bytes) {
                doc.delete_pages(&pages);
                let mut stripped_buf = Vec::new();
                if doc.save_to(&mut stripped_buf).is_ok() {
                    if let Ok(md) = anydoc::to_markdown_bytes(&stripped_buf, Format::Pdf) {
                        let note = format!(
                            "> *[Lưu ý từ `anydoc`: Đã trích xuất văn bản từ {}/{} trang PDF; bỏ qua {} trang thuần ảnh: {:?}]*\n\n",
                            page_count.saturating_sub(pages.len() as u32),
                            page_count,
                            pages.len(),
                            pages
                        );
                        return Ok((format!("{note}{md}"), pages, Some(page_count)));
                    }
                }
            }

            let inspected = pdf_inspector::process_pdf_mem(bytes).map_err(|e| {
                io::Error::other(format!(
                    "Không thể trích xuất trang văn bản từ `{}`: {e}",
                    path.display()
                ))
            })?;
            if let Some(md) = inspected.markdown.filter(|s| !s.trim().is_empty()) {
                let note = format!(
                    "> *[Lưu ý từ `anydoc`: Đã trích xuất văn bản từ {}/{} trang PDF; bỏ qua {} trang thuần ảnh: {:?}]*\n\n",
                    page_count.saturating_sub(pages.len() as u32),
                    page_count,
                    pages.len(),
                    pages
                );
                return Ok((format!("{note}{md}"), pages, Some(page_count)));
            }

            Err(io::Error::other(format!(
                "Lỗi khi chuyển đổi `{}` sang Markdown bằng anydoc: pages {:?} of {} need OCR",
                path.display(),
                pages,
                page_count
            )))
        }
        Err(e) => Err(io::Error::other(format!(
            "Lỗi khi chuyển đổi `{}` sang Markdown bằng anydoc: {e}",
            path.display()
        ))),
    }
}

fn normalize_anydoc_markdown(
    raw_md: &str,
    fallback_stem: &str,
    max_chars: Option<usize>,
) -> (String, String, Vec<String>) {
    let mut inferred_title: Option<String> = None;
    let mut headings: Vec<String> = Vec::new();
    let mut lines_out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut blank_streak = 0usize;

    for raw_line in raw_md.lines() {
        let trimmed_end = raw_line.trim_end();
        let trimmed = trimmed_end.trim();

        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            blank_streak = 0;
            lines_out.push(trimmed_end.to_string());
            continue;
        }
        if in_fence {
            lines_out.push(trimmed_end.to_string());
            continue;
        }
        if trimmed.is_empty() {
            blank_streak += 1;
            if blank_streak <= 1 && !lines_out.is_empty() {
                lines_out.push(String::new());
            }
            continue;
        }
        blank_streak = 0;

        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            let text = trimmed[level..].trim();
            if !text.is_empty() {
                if inferred_title.is_none() && level <= 2 {
                    inferred_title = Some(text.to_string());
                }
                if level <= 3 && headings.len() < 25 {
                    headings.push(format!("{} {}", "#".repeat(level), text));
                }
            }
            lines_out.push(trimmed.to_string());
            continue;
        }

        if trimmed.starts_with('|') || trimmed.chars().count() <= 140 {
            lines_out.push(trimmed_end.to_string());
        } else {
            for wrapped in wrap_long_prose_line(trimmed, 110) {
                lines_out.push(wrapped);
            }
        }
    }

    let mut joined = lines_out.join("\n");
    if let Some(limit) = max_chars.filter(|&n| n > 0) {
        if joined.chars().count() > limit {
            let mut cut = String::new();
            for line in joined.lines() {
                if cut.chars().count() + line.chars().count() + 1 > limit {
                    break;
                }
                cut.push_str(line);
                cut.push('\n');
            }
            cut.push_str(&format!(
                "\n> *[Nội dung PDF được cắt tại ngưỡng {limit} ký tự]*\n"
            ));
            joined = cut;
        }
    }

    let title = inferred_title.unwrap_or_else(|| humanize_stem(fallback_stem));
    (joined, title, headings)
}

fn wrap_long_prose_line(line: &str, max_width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for word in line.split_whitespace() {
        let w_len = word.chars().count();
        if current_chars > 0 && current_chars + 1 + w_len > max_width {
            out.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if current_chars > 0 {
            current.push(' ');
            current_chars += 1;
        }
        current.push_str(word);
        current_chars += w_len;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn humanize_stem(stem: &str) -> String {
    let words: Vec<String> = stem
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        "PDF Technical Document".to_string()
    } else {
        words.join(" ")
    }
}

/// Tạo một file PDF 1.4 mẫu chứa tài liệu kỹ thuật để chạy thử nghiệm luồng `PDF -> anydoc -> Wiki`.
pub fn build_sample_technical_pdf() -> Vec<u8> {
    let lines: &[(u16, &str)] = &[
        (18, "Hybrid Vector and Lexical Retrieval in Local LLM Wikis"),
        (14, "1. Architecture Overview"),
        (
            11,
            "Modern retrieval systems often suffer from context fragmentation when chunking documents blindly.",
        ),
        (
            11,
            "Combining Andrej Karpathy LLM Wiki architecture with QMD hybrid search solves this problem.",
        ),
        (
            11,
            "Raw documents in sources are converted from PDF to Markdown using Anydoc and indexed locally.",
        ),
        (14, "2. Reciprocal Rank Fusion and SQLite FTS5"),
        (
            11,
            "Lexical search uses SQLite FTS5 with BM25 scoring to capture exact identifiers and symbols.",
        ),
        (
            11,
            "Semantic search uses cosine similarity over dense embeddings stored in chunk vectors.",
        ),
        (
            11,
            "Reciprocal Rank Fusion merges both ranked lists with constant k equal to 60 and a 2x primary weight.",
        ),
        (
            11,
            "Top ranked candidates receive an explicit rank bonus of 0.05 for rank 1 and 0.02 for ranks 2 to 3.",
        ),
        (14, "3. Obsidian Graph View and Bidirectional WikiLinks"),
        (
            11,
            "Each compiled source summary in wiki sources links to canonical Entity and Concept pages.",
        ),
        (
            11,
            "Wiki Lint audits broken links, orphan wiki pages, and uningested sources automatically.",
        ),
    ];

    let mut stream = String::from("BT\n");
    let mut y = 730i32;
    for &(font_size, text) in lines {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        stream.push_str(&format!(
            "/F1 {font_size} Tf\n1 0 0 1 54 {y} Tm\n({escaped}) Tj\n"
        ));
        y -= if font_size >= 14 { 28 } else { 18 };
    }
    stream.push_str("ET\n");

    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
        format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in &offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}
