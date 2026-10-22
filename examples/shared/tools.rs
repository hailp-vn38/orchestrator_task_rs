//! Bộ `HostTool` dùng chung cho các example:
//! 1. `WikiEngineBridgeTool`: bọc các tool native của `WikiEngine` (`ReadOnly` cho `daily_orch`, đầy đủ cho `wiki_orch`).
//! 2. `AddToSourcesTool`: cầu nối từ Flow 2 (`report_stager`) ghi vào `sources/<domain>/<topic>/` và kích hoạt `autoflow_notify`.
//! 3. `YtListPlaylistTool` & `YtFetchTranscriptTool`: bộ thu thập YouTube Playlist cho `source_collector` (Flow 1), lưu vào `sources/<domain>/<topic>/` và kích hoạt `autoflow_notify`.
//! 4. `MarkdownCreateTool` & `MarkdownReadTool`: xuất báo cáo rời vào `layout.reports_dir()`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use llm_wiki::{
    sanitize_topic_folder, slugify, AdaptiveReadBudget, LlmToolSpec, WikiEngine,
    WriteTopicSourceRequest,
};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::shared::ytdlp::{fetch_playlist, fetch_transcript};

/// Giới hạn ký tự cho bản xem trước ngắn khi `source_collector` lưu transcript vào `sources/`
/// (tránh làm đầy context window của `source_collector` khi tải nhiều video liên tiếp).
const COLLECTOR_PREVIEW_CHARS: usize = 600;

pub fn tool_err(code: &str, msg: impl Into<String>) -> ToolError {
    ToolError {
        code: code.into(),
        message: msg.into(),
    }
}

fn outcome(ctx: &ToolContext, content: String, metadata: Value) -> ToolOutcome {
    ToolOutcome {
        call_id: ctx.scope().tool_call_id.clone(),
        content,
        metadata,
    }
}

pub struct WikiEngineBridgeTool {
    pub engine: Arc<WikiEngine>,
    pub spec: LlmToolSpec,
    pub effect: ToolEffect,
    pub read_budget: AdaptiveReadBudget,
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
            .map_err(|e| tool_err("WIKI_TOOL_ERROR", e.to_string()))?;

        if let Some(ref msg) = out.progress_message {
            ctx.progress(msg);
        }
        Ok(outcome(&ctx, out.content, out.metadata))
    }
}

pub struct AddToSourcesTool {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
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
            .ok_or_else(|| tool_err("INVALID_ARGUMENT", "Missing topic_folder"))?;
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| tool_err("INVALID_ARGUMENT", "Missing title"))?;
        let body = args
            .get("markdown_body")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| tool_err("INVALID_ARGUMENT", "Missing markdown_body"))?;

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
            .map_err(|e| tool_err("WRITE_SOURCE_ERROR", e.to_string()))?;

        // Kích hoạt tức thì Managed Auto-Flow Worker trong `TaskManager`
        self.autoflow_notify.notify_one();

        Ok(outcome(
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

pub struct MarkdownCreateTool {
    pub reports_dir: PathBuf,
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
            .ok_or_else(|| tool_err("INVALID_ARGUMENT", "Missing filename"))?;
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
            .map_err(|e| tool_err("IO_ERROR", e.to_string()))?;
        let full_path = self.reports_dir.join(&safe_name);
        let trimmed_body = body.trim();
        let content = if trimmed_body.starts_with("# ") {
            format!("{trimmed_body}\n")
        } else {
            format!("# {title}\n\n{trimmed_body}\n")
        };
        std::fs::write(&full_path, &content).map_err(|e| tool_err("IO_ERROR", e.to_string()))?;

        Ok(outcome(
            &ctx,
            format!("Saved report to {}", full_path.display()),
            json!({
                "filename": safe_name,
                "path": full_path.display().to_string(),
                "bytes": content.len()
            }),
        ))
    }
}

pub struct MarkdownReadTool {
    pub reports_dir: PathBuf,
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
            .ok_or_else(|| tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let stem = slugify(filename.trim_end_matches(".md"), 64);
        let safe_name = format!("{}.md", if stem.is_empty() { "report" } else { &stem });
        let full_path = self.reports_dir.join(&safe_name);
        let content = std::fs::read_to_string(&full_path)
            .map_err(|e| tool_err("READ_ERROR", e.to_string()))?;

        Ok(outcome(
            &ctx,
            content.clone(),
            json!({
                "filename": safe_name,
                "bytes": content.len()
            }),
        ))
    }
}

// ---------------------------------------------------------------------------
// 5. YouTube Playlist Tools cho `source_collector` (Flow 1)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct PlaylistVideoEntry {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub stem: String,
    /// Đường dẫn tương đối thực tế bên trong `sources/` sau khi `source_collector` phân loại theo chủ đề
    /// (ví dụ: `Some("algo-trading/mql5-dca/01-....md")`).
    pub saved_source_scoped_path: Option<String>,
}

pub struct YoutubeCollectorEnv {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
    pub work_dir: PathBuf,
    pub langs: String,
    pub limit: usize,
    pub playlist_url: Mutex<String>,
    pub videos: Mutex<Vec<PlaylistVideoEntry>>,
}

pub struct YtListPlaylistTool {
    pub env: Arc<YoutubeCollectorEnv>,
}

#[async_trait::async_trait]
impl HostTool for YtListPlaylistTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "yt_list_playlist".to_string(),
            description: "List all videos in a YouTube playlist URL using yt-dlp. Returns `index`, `video_id`, `file_stem`, and `title` so the agent can analyze the playlist theme and choose a topic folder (`<domain>/<topic>`) inside `sources/`.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "YouTube playlist http(s) URL"
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| tool_err("INVALID_ARGUMENT", "Thiếu hoặc rỗng: url"))?
            .to_string();

        *self.env.playlist_url.lock().unwrap() = url.clone();
        ctx.progress("Đang lấy danh sách video từ playlist bằng yt-dlp...");

        let mut raw_list = tokio::task::spawn_blocking(move || fetch_playlist(&url))
            .await
            .map_err(|e| tool_err("INTERNAL", e.to_string()))??;
        raw_list.truncate(self.env.limit);

        let entries: Vec<PlaylistVideoEntry> = raw_list
            .into_iter()
            .enumerate()
            .map(|(i, (id, title))| {
                let stem = format!("{:02}-{}", i + 1, slugify(&title, 36));
                PlaylistVideoEntry {
                    index: i + 1,
                    id,
                    title,
                    stem,
                    saved_source_scoped_path: None,
                }
            })
            .collect();

        let mut text = String::new();
        for v in &entries {
            text.push_str(&format!(
                "{}. video_id={} file_stem={} title={}\n",
                v.index, v.id, v.stem, v.title
            ));
        }

        let count = entries.len();
        ctx.progress(&format!("Tìm thấy {count} video trong playlist"));
        *self.env.videos.lock().unwrap() = entries;
        Ok(outcome(&ctx, text, json!({ "video_count": count })))
    }
}

pub struct YtFetchTranscriptTool {
    pub env: Arc<YoutubeCollectorEnv>,
}

#[async_trait::async_trait]
impl HostTool for YtFetchTranscriptTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "yt_fetch_transcript".to_string(),
            description: "Download the subtitles/transcript of a YouTube video using yt-dlp, save the Raw Source Markdown file into `sources/<topic_folder>/<file_stem>.md` organized by topic with hierarchical tags, and notify the background Wiki Auto-Flow worker.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["video_id", "topic_folder"],
                "properties": {
                    "video_id": {
                        "type": "string",
                        "description": "YouTube video ID from `yt_list_playlist`"
                    },
                    "topic_folder": {
                        "type": "string",
                        "description": "Topic folder path inside `sources/` in `kebab-case` (max 2 levels `<domain>/<topic>`, e.g. 'algo-trading/mql5-dca' or 'ai-engineering/rag')."
                    },
                    "title": {
                        "type": "string",
                        "description": "Video title from `yt_list_playlist`"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Hierarchical topic tags in `kebab-case` (e.g. ['topic/algo-trading/mql5-dca', 'mql5', 'mt5'])"
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::Mutating,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let video_id = args
            .get("video_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| tool_err("INVALID_ARGUMENT", "Thiếu hoặc rỗng: video_id"))?
            .to_string();
        let raw_topic_folder = args
            .get("topic_folder")
            .and_then(Value::as_str)
            .unwrap_or("general");
        let topic_folder = sanitize_topic_folder(raw_topic_folder);

        let known_entry = self
            .env
            .videos
            .lock()
            .unwrap()
            .iter()
            .find(|v| v.id == video_id)
            .cloned();

        let (stem, title, idx) = if let Some(entry) = known_entry {
            (entry.stem, entry.title, entry.index)
        } else {
            let custom_title = args
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&video_id)
                .to_string();
            let custom_stem = slugify(&custom_title, 36);
            (custom_stem, custom_title, 0)
        };

        let mut tags = Vec::new();
        if let Some(arr) = args.get("tags").and_then(Value::as_array) {
            for item in arr.iter().filter_map(Value::as_str) {
                let clean_tag = item.trim().to_string();
                if !clean_tag.is_empty() && !tags.contains(&clean_tag) {
                    tags.push(clean_tag);
                }
            }
        }

        ctx.progress(&format!(
            "Đang tải transcript bằng yt-dlp cho video `{video_id}` -> `sources/{topic_folder}/{stem}.md`..."
        ));
        let work = self.env.work_dir.clone();
        let langs = self.env.langs.clone();
        let vid = video_id.clone();
        let transcript = tokio::task::spawn_blocking(move || fetch_transcript(&work, &vid, &langs))
            .await
            .map_err(|e| tool_err("INTERNAL", e.to_string()))??;

        let playlist_url = self.env.playlist_url.lock().unwrap().clone();
        let youtube_url = format!("https://www.youtube.com/watch?v={video_id}");
        let written = self
            .env
            .engine
            .write_topic_source(WriteTopicSourceRequest {
                topic_folder: topic_folder.clone(),
                file_stem: Some(stem),
                title: title.clone(),
                raw_body: transcript,
                body_heading: Some("Raw Transcript".to_string()),
                tags,
                extra_frontmatter: vec![
                    ("video_id".to_string(), json!(video_id)),
                    ("youtube_url".to_string(), json!(youtube_url)),
                    ("playlist_url".to_string(), json!(playlist_url)),
                    ("playlist_index".to_string(), json!(idx)),
                ],
                header_bullets: vec![
                    ("YouTube URL".to_string(), format!("<{youtube_url}>")),
                    ("Playlist URL".to_string(), format!("<{playlist_url}>")),
                    ("Video ID".to_string(), format!("`{video_id}`")),
                    ("Thứ tự trong Playlist".to_string(), format!("#{idx}")),
                ],
                preview_chars: Some(COLLECTOR_PREVIEW_CHARS),
            })
            .map_err(|e| tool_err("WIKI_SOURCE_WRITE_FAILED", e.to_string()))?;

        if let Some(entry) = self
            .env
            .videos
            .lock()
            .unwrap()
            .iter_mut()
            .find(|v| v.id == video_id)
        {
            entry.saved_source_scoped_path = Some(written.info.scoped_path.clone());
        }

        // Báo tín hiệu cho Auto-Flow Worker của TaskManager (Worker sẽ tự đợi Task thu thập hoàn tất rồi chạy Ingest)
        self.env.autoflow_notify.notify_one();

        ctx.progress(&format!(
            "Đã lưu Raw Source theo chủ đề `{}` ({} ký tự, {} dòng, docid: {})",
            written.info.rel_path,
            written.body_char_count,
            written.info.line_count,
            written.info.docid
        ));

        Ok(outcome(
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
