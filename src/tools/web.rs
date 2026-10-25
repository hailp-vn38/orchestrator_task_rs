//! Bộ công cụ `HostTool` tải và bóc tách nội dung trang Web / HTML (`fetch_html`,
//! `html_extract_metadata`, `html_extract_article`, `html_extract_feed`, `html_extract_links`,
//! `html_select_elements`) cùng bộ HTML parser thuần không cần thêm dependency ngoài.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orchestrator_agent_rs::{
    ArtifactInput, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Giới hạn ký tự nội dung bài viết trả về cho model để không tràn context.
const MAX_ARTICLE_CHARS: usize = 24_000;

/// Danh sách toàn bộ 6 tên tool HTML để khai báo nhanh trong `AgentDefinition::with_allowed_tools(HTML_TOOL_NAMES)`.
pub const HTML_TOOL_NAMES: [&str; 6] = [
    "fetch_html",
    "html_extract_metadata",
    "html_extract_article",
    "html_extract_feed",
    "html_extract_links",
    "html_select_elements",
];

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", format!("Thiếu hoặc rỗng: {key}")))
}

fn validate_http_url(url: &str) -> Result<(), ToolError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(host_tool_err(
            "INVALID_URL",
            "Chỉ hỗ trợ URL http:// hoặc https://",
        ));
    }
    if url.chars().any(char::is_whitespace) {
        return Err(host_tool_err(
            "INVALID_URL",
            "URL không được chứa khoảng trắng",
        ));
    }
    Ok(())
}

/// Bộ nhớ đệm và thư mục lưu HTML nguyên bản cho các tool HTML dùng chung.
pub struct HtmlWorkspace {
    raw_dir: PathBuf,
    cache: Mutex<HashMap<String, String>>,
}

impl HtmlWorkspace {
    pub fn new(raw_dir: impl Into<PathBuf>) -> Self {
        Self {
            raw_dir: raw_dir.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Đường dẫn thư mục lưu các bản snapshot HTML tải về.
    pub fn raw_dir(&self) -> &Path {
        &self.raw_dir
    }

    /// Nạp sẵn nội dung HTML vào bộ nhớ đệm cho một `url` (hữu ích khi test hoặc khi host đã tải HTML trước).
    pub fn insert_cached_html(&self, url: impl Into<String>, html: impl Into<String>) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(url.into(), html.into());
    }

    /// Xuất toàn bộ 6 `HostTool` xử lý Web/HTML gắn với `Arc<HtmlWorkspace>`.
    pub fn host_tools(self: &Arc<Self>) -> Vec<Arc<dyn HostTool>> {
        web_host_tools(Arc::clone(self))
    }

    /// Lấy HTML từ cache hoặc tải mới bằng `curl` (blocking).
    fn fetch_blocking(&self, url: &str) -> Result<(String, bool), ToolError> {
        validate_http_url(url)?;
        if let Some(cached) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(url)
            .cloned()
        {
            return Ok((cached, true));
        }
        std::fs::create_dir_all(&self.raw_dir).map_err(|e| host_tool_err("IO", e.to_string()))?;
        let output = std::process::Command::new("curl")
            .args([
                "-sSL",
                "--compressed",
                "--max-time",
                "30",
                "-A",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
                "--",
            ])
            .arg(url)
            .output()
            .map_err(|e| host_tool_err("CURL_UNAVAILABLE", format!("Không chạy được curl: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(host_tool_err(
                "FETCH_FAILED",
                format!("curl thất bại ({}): {}", output.status, stderr.trim()),
            ));
        }
        let html = String::from_utf8_lossy(&output.stdout).into_owned();
        if html.trim().is_empty() {
            return Err(host_tool_err("EMPTY_HTML", "Trang trả về nội dung rỗng"));
        }
        let file_path = self.raw_dir.join(format!("{}.html", uuid::Uuid::new_v4()));
        let _ = std::fs::write(&file_path, &html);
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(url.to_string(), html.clone());
        Ok((html, false))
    }

    pub async fn get_or_fetch(
        self: &Arc<Self>,
        ctx: &ToolContext,
        url: &str,
    ) -> Result<(String, bool), ToolError> {
        let ws = self.clone();
        let u = url.to_string();
        let (html, from_cache) = tokio::task::spawn_blocking(move || ws.fetch_blocking(&u))
            .await
            .map_err(|e| host_tool_err("INTERNAL", e.to_string()))??;
        if !from_cache {
            ctx.progress(&format!("Đã tải HTML ({} bytes)", html.len()));
        }
        Ok((html, from_cache))
    }
}

/// Phân loại 6 công cụ xử lý HTML trong `HtmlTool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtmlToolKind {
    FetchHtml,
    ExtractMetadata,
    ExtractArticle,
    ExtractFeed,
    ExtractLinks,
    SelectElements,
}

impl HtmlToolKind {
    pub const ALL: [Self; 6] = [
        Self::FetchHtml,
        Self::ExtractMetadata,
        Self::ExtractArticle,
        Self::ExtractFeed,
        Self::ExtractLinks,
        Self::SelectElements,
    ];
}

/// `HostTool` thực thi một trong 6 thao tác tải/bóc tách HTML trên `HtmlWorkspace`.
pub struct HtmlTool {
    ws: Arc<HtmlWorkspace>,
    kind: HtmlToolKind,
}

impl HtmlTool {
    pub fn new(ws: Arc<HtmlWorkspace>, kind: HtmlToolKind) -> Arc<Self> {
        Arc::new(Self { ws, kind })
    }
}

/// Xuất trọn bộ 6 `HostTool` xử lý Web/HTML (`fetch_html`, `html_extract_metadata`,
/// `html_extract_article`, `html_extract_feed`, `html_extract_links`, `html_select_elements`).
pub fn web_host_tools(ws: Arc<HtmlWorkspace>) -> Vec<Arc<dyn HostTool>> {
    HtmlToolKind::ALL
        .into_iter()
        .map(|kind| HtmlTool::new(Arc::clone(&ws), kind) as Arc<dyn HostTool>)
        .collect()
}

#[async_trait::async_trait]
impl HostTool for HtmlTool {
    fn descriptor(&self) -> ToolDescriptor {
        let url_prop = json!({"type": "string", "description": "http(s) URL of the web page"});
        match self.kind {
            HtmlToolKind::FetchHtml => ToolDescriptor {
                name: "fetch_html".into(),
                description: "Fetch raw HTML from an http(s) URL, cache it for subsequent HTML tools, store it as an artifact, and return a structural overview of the page (including article_tag_count to distinguish single articles from category/listing pages).".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"url": url_prop},
                    "required": ["url"],
                    "additionalProperties": false
                }),
                effect: ToolEffect::ReadOnly,
            },
            HtmlToolKind::ExtractMetadata => ToolDescriptor {
                name: "html_extract_metadata".into(),
                description: "Extract metadata from the HTML page at URL: <title>, meta description, OpenGraph tags, canonical URL, published date, author, and JSON-LD NewsArticle fields.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"url": url_prop},
                    "required": ["url"],
                    "additionalProperties": false
                }),
                effect: ToolEffect::ReadOnly,
            },
            HtmlToolKind::ExtractArticle => ToolDescriptor {
                name: "html_extract_article".into(),
                description: "Extract the main readable article content (title, sapo/lead, headings, paragraphs, bullet points, and content images) from a single article HTML page.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"url": url_prop},
                    "required": ["url"],
                    "additionalProperties": false
                }),
                effect: ToolEffect::ReadOnly,
            },
            HtmlToolKind::ExtractFeed => ToolDescriptor {
                name: "html_extract_feed".into(),
                description: "Extract a list of news/article cards ({title, url, snippet}) from a category or listing page (e.g. https://vnexpress.net/thoi-su). Use the returned article URLs with fetch_html + html_extract_article to read each article's full detail.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "url": url_prop,
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of news items to return (1..=30, default 10)"
                        }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
                effect: ToolEffect::ReadOnly,
            },
            HtmlToolKind::ExtractLinks => ToolDescriptor {
                name: "html_extract_links".into(),
                description: "Extract hyperlinks (<a href>) with anchor text from the article body or the entire HTML page. Useful for finding related articles on the same topic.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "url": url_prop,
                        "scope": {
                            "type": "string",
                            "enum": ["article", "all"],
                            "description": "Where to extract links from: 'article' (main content region) or 'all' (whole page)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of links to return (1..=30)"
                        }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
                effect: ToolEffect::ReadOnly,
            },
            HtmlToolKind::SelectElements => ToolDescriptor {
                name: "html_select_elements".into(),
                description: "Select HTML elements by tag name (e.g. 'h1', 'h2', 'p', 'article', 'div') and optional attribute substring filter, returning their cleaned text content.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "url": url_prop,
                        "tag": {
                            "type": "string",
                            "description": "HTML tag name to extract, e.g. h1, h2, p, article, div"
                        },
                        "attr_contains": {
                            "type": "string",
                            "description": "Optional substring that the opening tag attributes must contain (e.g. id or class name)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of matching elements to return (1..=30)"
                        }
                    },
                    "required": ["url", "tag"],
                    "additionalProperties": false
                }),
                effect: ToolEffect::ReadOnly,
            },
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let url = str_arg(&args, "url")?.to_string();
        let (html, from_cache) = self.ws.get_or_fetch(&ctx, &url).await?;

        match self.kind {
            HtmlToolKind::FetchHtml => {
                let artifact = ctx
                    .artifacts()
                    .put(ArtifactInput::intermediate_bytes(
                        "page.html",
                        "text/html; charset=utf-8",
                        html.as_bytes().to_vec(),
                    ))
                    .await
                    .ok();
                let meta = extract_metadata(&html);
                let article_count = find_elements(&html, "article").len();
                let overview = json!({
                    "url": url,
                    "cached": from_cache,
                    "bytes": html.len(),
                    "title": meta["title"],
                    "description": meta["description"],
                    "article_tag_count": article_count,
                    "has_main_tag": !find_elements(&html, "main").is_empty(),
                    "h1_count": find_elements(&html, "h1").len(),
                    "h2_count": find_elements(&html, "h2").len(),
                    "h3_count": find_elements(&html, "h3").len(),
                    "p_count": find_elements(&html, "p").len(),
                    "artifact_id": artifact.as_ref().map(|a| &a.id),
                });
                let meta_out = match artifact {
                    Some(a) => json!({"url": url, "bytes": html.len(), "artifact_ids": [a.id]}),
                    None => json!({"url": url, "bytes": html.len()}),
                };
                Ok(host_tool_outcome(
                    &ctx,
                    serde_json::to_string_pretty(&overview).unwrap_or_default(),
                    meta_out,
                ))
            }
            HtmlToolKind::ExtractMetadata => {
                ctx.progress("Đang trích xuất metadata từ HTML...");
                let meta = extract_metadata(&html);
                Ok(host_tool_outcome(
                    &ctx,
                    serde_json::to_string_pretty(&meta).unwrap_or_default(),
                    json!({"url": url}),
                ))
            }
            HtmlToolKind::ExtractArticle => {
                ctx.progress("Đang bóc tách nội dung bài viết từ HTML...");
                let article = extract_article(&html);
                let mut body = String::new();
                if !article.title.is_empty() {
                    body.push_str(&format!("# {}\n\n", article.title));
                }
                if !article.sapo.is_empty() {
                    body.push_str(&format!("**Tóm lược (Sapo):** {}\n\n", article.sapo));
                }
                if !article.sections.is_empty() {
                    body.push_str("## Nội dung trong trang\n\n");
                    for s in &article.sections {
                        body.push_str(s);
                        body.push_str("\n\n");
                    }
                }
                if !article.images.is_empty() {
                    body.push_str("## Ảnh / Infographic trong bài\n\n");
                    for img in &article.images {
                        let alt = img["alt"].as_str().unwrap_or("").trim();
                        let src = img["url"].as_str().unwrap_or("").trim();
                        let label = if alt.is_empty() { "image" } else { alt };
                        body.push_str(&format!("- ![{label}]({src})\n"));
                    }
                }
                let total_chars = body.chars().count();
                let truncated: String = body.chars().take(MAX_ARTICLE_CHARS).collect();
                Ok(host_tool_outcome(
                    &ctx,
                    truncated,
                    json!({
                        "url": url,
                        "title": article.title,
                        "paragraph_count": article.sections.len(),
                        "image_count": article.images.len(),
                        "chars": total_chars
                    }),
                ))
            }
            HtmlToolKind::ExtractFeed => {
                ctx.progress("Đang trích xuất danh sách bài viết từ trang chuyên mục...");
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(10)
                    .clamp(1, 30) as usize;
                let items = extract_feed_items(&html, &url, limit);
                Ok(host_tool_outcome(
                    &ctx,
                    serde_json::to_string_pretty(&json!({
                        "url": url,
                        "count": items.len(),
                        "items": items
                    }))
                    .unwrap_or_default(),
                    json!({"url": url, "count": items.len()}),
                ))
            }
            HtmlToolKind::ExtractLinks => {
                let scope = args
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("article");
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(15)
                    .clamp(1, 30) as usize;
                let article_only = scope != "all";
                let links = extract_links(&html, &url, article_only, limit);
                Ok(host_tool_outcome(
                    &ctx,
                    serde_json::to_string_pretty(&json!({
                        "url": url,
                        "scope": if article_only { "article" } else { "all" },
                        "count": links.len(),
                        "links": links
                    }))
                    .unwrap_or_default(),
                    json!({"url": url, "count": links.len()}),
                ))
            }
            HtmlToolKind::SelectElements => {
                let tag = str_arg(&args, "tag")?.to_ascii_lowercase();
                let filter = args
                    .get("attr_contains")
                    .and_then(Value::as_str)
                    .map(|s| s.to_ascii_lowercase());
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(15)
                    .clamp(1, 30) as usize;
                let cleaned = clean_noise_tags(&html);
                let mut items = Vec::new();
                for (attrs, inner) in find_elements(&cleaned, &tag) {
                    if let Some(ref needle) = filter {
                        if !attrs.to_ascii_lowercase().contains(needle) {
                            continue;
                        }
                    }
                    let text = strip_tags(&inner);
                    if !text.is_empty() {
                        items.push(json!({
                            "attrs": collapse_ws(&attrs),
                            "text": text
                        }));
                        if items.len() >= limit {
                            break;
                        }
                    }
                }
                Ok(host_tool_outcome(
                    &ctx,
                    serde_json::to_string_pretty(&json!({
                        "url": url,
                        "tag": tag,
                        "count": items.len(),
                        "elements": items
                    }))
                    .unwrap_or_default(),
                    json!({"url": url, "tag": tag, "count": items.len()}),
                ))
            }
        }
    }
}

// ===========================================================================
// Bộ xử lý và bóc tách HTML thuần (Pure Rust HTML Parser & Extractor)
// ===========================================================================

/// Giải mã các HTML entities phổ biến và mã số (`&#...;`, `&#x...;`).
pub fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi_rel) = after.bytes().take(12).position(|b| b == b';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..semi_rel];
        let decoded = match entity {
            "nbsp" | "#160" => Some(" ".to_string()),
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" | "#39" | "#039" => Some("'".to_string()),
            "ndash" => Some("–".to_string()),
            "mdash" => Some("—".to_string()),
            "laquo" => Some("«".to_string()),
            "raquo" => Some("»".to_string()),
            "hellip" => Some("…".to_string()),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            }
            _ if entity.starts_with('#') => entity[1..]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string()),
            _ => None,
        };
        if let Some(ch) = decoded {
            out.push_str(&ch);
            rest = &after[semi_rel + 1..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Gộp mọi chuỗi khoảng trắng liên tiếp thành một dấu cách đơn.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bỏ toàn bộ thẻ HTML trong đoạn văn bản và giải mã entity.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    collapse_ws(&decode_html_entities(&out))
}

fn is_tag_boundary(b: Option<u8>) -> bool {
    matches!(b, None | Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/'))
}

fn find_tag_end(html: &str, start: usize) -> Option<usize> {
    html[start..].find('>').map(|i| start + i + 1)
}

/// Xóa comment HTML `<!-- ... -->`.
pub fn remove_html_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        if let Some(end) = rest[start + 4..].find("-->") {
            rest = &rest[start + 4 + end + 3..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Xóa mọi khối `<tag ...>...</tag>` (có tính độ sâu lồng nhau).
pub fn remove_tag_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some(rel) = lower[cursor..].find(&open_pat) else {
            out.push_str(&html[cursor..]);
            break;
        };
        let start = cursor + rel;
        if !is_tag_boundary(bytes.get(start + open_pat.len()).copied()) {
            out.push_str(&html[cursor..start + open_pat.len()]);
            cursor = start + open_pat.len();
            continue;
        }
        out.push_str(&html[cursor..start]);
        let Some(mut scan) = find_tag_end(&lower, start) else {
            break;
        };
        if lower[start..scan].trim_end().ends_with("/>") {
            cursor = scan;
            continue;
        }
        let mut depth = 1usize;
        while depth > 0 && scan < html.len() {
            let next_open = lower[scan..].find(&open_pat).map(|i| scan + i);
            let next_close = lower[scan..].find(&close_pat).map(|i| scan + i);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    if is_tag_boundary(bytes.get(o + open_pat.len()).copied()) {
                        if let Some(end_o) = find_tag_end(&lower, o) {
                            if !lower[o..end_o].trim_end().ends_with("/>") {
                                depth += 1;
                            }
                            scan = end_o;
                            continue;
                        }
                    }
                    scan = o + open_pat.len();
                }
                (_, Some(c)) => {
                    if is_tag_boundary(bytes.get(c + close_pat.len()).copied()) {
                        depth -= 1;
                        scan = find_tag_end(&lower, c).unwrap_or(html.len());
                    } else {
                        scan = c + close_pat.len();
                    }
                }
                _ => {
                    scan = html.len();
                    break;
                }
            }
        }
        cursor = scan;
    }
    out
}

/// Tìm các phần tử `<tag ...>inner</tag>` (trả về `(open_tag_attrs, inner_html)`).
pub fn find_elements(html: &str, tag: &str) -> Vec<(String, String)> {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}");
    let mut results = Vec::new();
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some(rel) = lower[cursor..].find(&open_pat) else {
            break;
        };
        let start = cursor + rel;
        if !is_tag_boundary(bytes.get(start + open_pat.len()).copied()) {
            cursor = start + open_pat.len();
            continue;
        }
        let Some(open_end) = find_tag_end(&lower, start) else {
            break;
        };
        let open_tag = html[start + open_pat.len()..open_end - 1].to_string();
        if open_tag.trim_end().ends_with('/') {
            results.push((open_tag, String::new()));
            cursor = open_end;
            continue;
        }
        let mut scan = open_end;
        let mut depth = 1usize;
        let mut close_start = html.len();
        let mut after_close = html.len();
        while depth > 0 && scan < html.len() {
            let next_open = lower[scan..].find(&open_pat).map(|i| scan + i);
            let next_close = lower[scan..].find(&close_pat).map(|i| scan + i);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    if is_tag_boundary(bytes.get(o + open_pat.len()).copied()) {
                        if let Some(end_o) = find_tag_end(&lower, o) {
                            if !lower[o..end_o].trim_end().ends_with("/>") {
                                depth += 1;
                            }
                            scan = end_o;
                            continue;
                        }
                    }
                    scan = o + open_pat.len();
                }
                (_, Some(c)) => {
                    if is_tag_boundary(bytes.get(c + close_pat.len()).copied()) {
                        depth -= 1;
                        close_start = c;
                        after_close = find_tag_end(&lower, c).unwrap_or(html.len());
                        scan = after_close;
                    } else {
                        scan = c + close_pat.len();
                    }
                }
                _ => break,
            }
        }
        let inner = html[open_end..close_start].to_string();
        results.push((open_tag, inner));
        cursor = after_close;
    }
    results
}

/// Lấy giá trị thuộc tính `attr="..."` hoặc `attr='...'` từ chuỗi thuộc tính của thẻ.
pub fn get_attr(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut search_from = 0usize;
    while search_from < lower.len() {
        let Some(rel) = lower[search_from..].find(name) else {
            break;
        };
        let pos = search_from + rel;
        let before_ok = pos == 0
            || matches!(
                lower.as_bytes()[pos - 1],
                b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\''
            );
        let after_name = &attrs[pos + name.len()..];
        let trimmed = after_name.trim_start();
        if !before_ok || !trimmed.starts_with('=') {
            search_from = pos + name.len();
            continue;
        }
        let val_part = trimmed[1..].trim_start();
        let val = if let Some(rest) = val_part.strip_prefix('"') {
            rest.split('"').next().unwrap_or("")
        } else if let Some(rest) = val_part.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("")
        } else {
            val_part.split_whitespace().next().unwrap_or("")
        };
        return Some(decode_html_entities(val.trim()));
    }
    None
}

/// Tìm tất cả các thẻ đơn/mở `<tag ...>` và trả về chuỗi thuộc tính bên trong.
pub fn find_open_tags(html: &str, tag: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let pat = format!("<{tag}");
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(&pat) {
        let start = cursor + rel;
        if !is_tag_boundary(bytes.get(start + pat.len()).copied()) {
            cursor = start + pat.len();
            continue;
        }
        let Some(end) = find_tag_end(&lower, start) else {
            break;
        };
        out.push(html[start + pat.len()..end - 1].to_string());
        cursor = end;
    }
    out
}

/// Trích xuất metadata từ HTML (`<title>`, `<meta>`, `<link rel="canonical">`, JSON-LD).
pub fn extract_metadata(html: &str) -> Value {
    let title = find_elements(html, "title")
        .into_iter()
        .next()
        .map(|(_, inner)| strip_tags(&inner))
        .unwrap_or_default();

    let mut meta_map: HashMap<String, String> = HashMap::new();
    for attrs in find_open_tags(html, "meta") {
        let key = get_attr(&attrs, "property")
            .or_else(|| get_attr(&attrs, "name"))
            .or_else(|| get_attr(&attrs, "itemprop"));
        let content = get_attr(&attrs, "content");
        if let (Some(k), Some(v)) = (key, content) {
            if !v.is_empty() {
                meta_map.entry(k.to_ascii_lowercase()).or_insert(v);
            }
        }
    }

    let canonical = find_open_tags(html, "link")
        .into_iter()
        .find(|a| get_attr(a, "rel").is_some_and(|r| r.eq_ignore_ascii_case("canonical")))
        .and_then(|a| get_attr(&a, "href"));

    let mut ld_headlines = Vec::new();
    let mut ld_descriptions = Vec::new();
    let mut ld_authors = Vec::new();
    let mut ld_dates = Vec::new();
    for (attrs, inner) in find_elements(html, "script") {
        let is_ld =
            get_attr(&attrs, "type").is_some_and(|t| t.to_ascii_lowercase().contains("ld+json"));
        if !is_ld {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(inner.trim()) {
            if let Some(h) = val.get("headline").and_then(Value::as_str) {
                ld_headlines.push(decode_html_entities(h.trim()));
            }
            if let Some(d) = val.get("description").and_then(Value::as_str) {
                ld_descriptions.push(decode_html_entities(d.trim()));
            }
            if let Some(dt) = val.get("datePublished").and_then(Value::as_str) {
                ld_dates.push(dt.trim().to_string());
            }
            if let Some(author) = val.get("author") {
                if let Some(name) = author.get("name").and_then(Value::as_str) {
                    ld_authors.push(decode_html_entities(name.trim()));
                } else if let Some(s) = author.as_str() {
                    ld_authors.push(decode_html_entities(s.trim()));
                }
            }
        }
    }

    let origin_data_src = find_elements(html, "span")
        .into_iter()
        .find(|(attrs, _)| {
            get_attr(attrs, "id")
                .is_some_and(|id| id == "url_origin_full" || id == "origin_name_full")
        })
        .and_then(|(attrs, _)| get_attr(&attrs, "data-src"));

    json!({
        "title": if !title.is_empty() { Some(title) } else { meta_map.get("og:title").cloned() },
        "og_title": meta_map.get("og:title"),
        "description": meta_map.get("description").or_else(|| meta_map.get("og:description")),
        "site_name": meta_map.get("og:site_name"),
        "keywords": meta_map.get("keywords").or_else(|| meta_map.get("news_keywords")),
        "canonical_url": canonical.or_else(|| meta_map.get("og:url").cloned()),
        "published_at": meta_map.get("pubdate")
            .or_else(|| meta_map.get("datepublished"))
            .cloned()
            .or_else(|| ld_dates.into_iter().next()),
        "author": meta_map.get("author").cloned().or_else(|| ld_authors.into_iter().next()),
        "ld_headline": ld_headlines.into_iter().next(),
        "ld_description": ld_descriptions.into_iter().next(),
        "origin_data_src": origin_data_src,
    })
}

/// Chọn vùng HTML chứa nội dung chính của bài viết (`<article>`, `<main>`, hoặc `<body>`).
pub fn locate_article_region(html: &str) -> String {
    let articles = find_elements(html, "article");
    if let Some((_, inner)) = articles.iter().find(|(attrs, _)| {
        get_attr(attrs, "id").is_some_and(|id| id.contains("article"))
            || get_attr(attrs, "class").is_some_and(|c| c.contains("content-main"))
    }) {
        return inner.clone();
    }
    if let Some((_, inner)) = articles.into_iter().find(|(_, inner)| inner.len() > 200) {
        return inner;
    }
    if let Some((_, inner)) = find_elements(html, "main").into_iter().next() {
        return inner;
    }
    if let Some((_, inner)) = find_elements(html, "body").into_iter().next() {
        return inner;
    }
    html.to_string()
}

/// Làm sạch các thẻ nhiễu (script, style, svg, nav, footer, form, iframe, button).
pub fn clean_noise_tags(html: &str) -> String {
    let mut cleaned = remove_html_comments(html);
    for noisy in [
        "script", "style", "noscript", "svg", "nav", "footer", "aside", "form", "iframe", "button",
    ] {
        cleaned = remove_tag_blocks(&cleaned, noisy);
    }
    cleaned
}

/// Kết quả bóc tách bài viết có cấu trúc từ HTML.
#[derive(Debug, Clone)]
pub struct ExtractedArticle {
    pub title: String,
    pub sapo: String,
    pub sections: Vec<String>,
    pub images: Vec<Value>,
}

/// Trích xuất bài viết có cấu trúc từ HTML.
pub fn extract_article(html: &str) -> ExtractedArticle {
    let meta = extract_metadata(html);
    let region = clean_noise_tags(&locate_article_region(html));

    let h1_title = find_elements(&region, "h1")
        .into_iter()
        .map(|(_, inner)| strip_tags(&inner))
        .find(|t| !t.is_empty())
        .or_else(|| {
            find_elements(html, "h1")
                .into_iter()
                .map(|(_, inner)| strip_tags(&inner))
                .find(|t| !t.is_empty())
        })
        .or_else(|| {
            meta.get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let sapo = find_elements(&region, "h2")
        .into_iter()
        .map(|(_, inner)| strip_tags(&inner))
        .find(|t| !t.is_empty())
        .or_else(|| {
            meta.get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let mut sections: Vec<String> = Vec::new();
    let mut push_unique = |line: String| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "Lưu bài viết thành công"
            || trimmed.starts_with("Bạn có thể xem lại bài viết đã lưu")
            || trimmed == "Bấm xem >>"
        {
            return;
        }
        if !sections.iter().any(|existing| existing == trimmed) {
            sections.push(trimmed.to_string());
        }
    };

    for (attrs, inner) in find_elements(&region, "h2") {
        let text = strip_tags(&inner);
        let is_sapo = get_attr(&attrs, "id").is_some_and(|id| id.contains("sapo")) || text == sapo;
        if !text.is_empty() && !is_sapo {
            push_unique(format!("## {text}"));
        }
    }
    for (_, inner) in find_elements(&region, "h3") {
        let text = strip_tags(&inner);
        if !text.is_empty() {
            push_unique(format!("### {text}"));
        }
    }
    for (_, inner) in find_elements(&region, "p") {
        let text = strip_tags(&inner);
        if !text.is_empty() && text != h1_title && text != sapo {
            push_unique(text);
        }
    }
    for (_, inner) in find_elements(&region, "li") {
        let text = strip_tags(&inner);
        if !text.is_empty() {
            push_unique(format!("- {text}"));
        }
    }

    let mut images = Vec::new();
    for attrs in find_open_tags(&region, "img") {
        let url = get_attr(&attrs, "data-original")
            .or_else(|| get_attr(&attrs, "data-src"))
            .or_else(|| get_attr(&attrs, "src"))
            .unwrap_or_default();
        if url.is_empty()
            || url.starts_with("data:image/")
            || url.contains("in-image-close")
            || url.contains("logo")
            || url.contains("loading.gif")
        {
            continue;
        }
        let alt = get_attr(&attrs, "alt").unwrap_or_default();
        images.push(json!({"alt": alt, "url": url}));
    }

    ExtractedArticle {
        title: h1_title,
        sapo,
        sections,
        images,
    }
}

/// Chuẩn hóa URL liên kết (hỗ trợ URL tương đối `/path` hoặc `//host/path`).
pub fn resolve_url(base_url: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty()
        || href.starts_with('#')
        || href.starts_with("javascript:")
        || href.starts_with("mailto:")
        || href.starts_with("tel:")
    {
        return None;
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let (scheme, rest) = base_url.split_once("://")?;
    if href.starts_with("//") {
        return Some(format!("{scheme}:{href}"));
    }
    let host = rest.split('/').next()?;
    if href.starts_with('/') {
        return Some(format!("{scheme}://{host}{href}"));
    }
    Some(format!("{scheme}://{host}/{href}"))
}

/// Trích xuất các liên kết `<a href="...">text</a>` từ vùng bài viết hoặc toàn trang.
pub fn extract_links(html: &str, base_url: &str, article_only: bool, limit: usize) -> Vec<Value> {
    let target = if article_only {
        clean_noise_tags(&locate_article_region(html))
    } else {
        clean_noise_tags(html)
    };
    let mut links = Vec::new();
    let mut seen = Vec::new();
    for (attrs, inner) in find_elements(&target, "a") {
        let Some(raw_href) = get_attr(&attrs, "href") else {
            continue;
        };
        let Some(url) = resolve_url(base_url, &raw_href) else {
            continue;
        };
        let text = strip_tags(&inner);
        let title_attr = get_attr(&attrs, "title").unwrap_or_default();
        let label = if !text.is_empty() && text != "Bấm xem >>" {
            text
        } else if !title_attr.is_empty() {
            title_attr
        } else {
            continue;
        };
        if seen.iter().any(|u| u == &url) {
            continue;
        }
        seen.push(url.clone());
        links.push(json!({"text": label, "url": url}));
        if links.len() >= limit {
            break;
        }
    }
    links
}

/// Trích xuất danh sách các mục tin (`title`, `url`, `snippet`) trên trang chuyên mục / trang chủ.
pub fn extract_feed_items(html: &str, base_url: &str, limit: usize) -> Vec<Value> {
    let cleaned = clean_noise_tags(html);
    let mut items = Vec::new();
    let mut seen_urls = Vec::new();

    for (_, article_inner) in find_elements(&cleaned, "article") {
        let mut found_title = String::new();
        let mut found_url = String::new();

        for h_tag in ["h1", "h2", "h3", "h4"] {
            for (_, h_inner) in find_elements(&article_inner, h_tag) {
                for (a_attrs, a_inner) in find_elements(&h_inner, "a") {
                    if let Some(href) =
                        get_attr(&a_attrs, "href").and_then(|h| resolve_url(base_url, &h))
                    {
                        let t = strip_tags(&a_inner);
                        let title = if !t.is_empty() {
                            t
                        } else {
                            get_attr(&a_attrs, "title").unwrap_or_default()
                        };
                        if !title.is_empty() {
                            found_title = title;
                            found_url = href;
                            break;
                        }
                    }
                }
                if !found_url.is_empty() {
                    break;
                }
            }
            if !found_url.is_empty() {
                break;
            }
        }

        if found_url.is_empty() || seen_urls.iter().any(|u| u == &found_url) {
            continue;
        }

        let snippet = find_elements(&article_inner, "p")
            .into_iter()
            .map(|(_, p_inner)| strip_tags(&p_inner))
            .find(|text| !text.is_empty() && text != &found_title)
            .unwrap_or_default();

        seen_urls.push(found_url.clone());
        items.push(json!({
            "title": found_title,
            "url": found_url,
            "snippet": snippet,
        }));
        if items.len() >= limit {
            break;
        }
    }
    items
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
