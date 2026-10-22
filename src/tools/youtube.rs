//! Bộ công cụ `HostTool` thu thập YouTube Playlist và Transcript bằng `yt-dlp`
//! (`yt_list_playlist`, `yt_fetch_transcript`) cho subagent `source_collector`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use llm_wiki::{sanitize_topic_folder, slugify, vtt_to_text, WikiEngine, WriteTopicSourceRequest};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Giới hạn ký tự cho bản xem trước ngắn khi `source_collector` lưu transcript vào `sources/`
/// (tránh làm đầy context window của `source_collector` khi tải nhiều video liên tiếp).
const COLLECTOR_PREVIEW_CHARS: usize = 600;

/// Thông tin một video trong YouTube Playlist đã được liệt kê hoặc tải phụ đề.
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

/// Môi trường trạng thái dùng chung giữa `YtListPlaylistTool` và `YtFetchTranscriptTool`.
pub struct YoutubeCollectorEnv {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
    pub work_dir: PathBuf,
    pub langs: String,
    pub limit: usize,
    pub playlist_url: Mutex<String>,
    pub videos: Mutex<Vec<PlaylistVideoEntry>>,
}

impl YoutubeCollectorEnv {
    /// Khởi tạo `YoutubeCollectorEnv` với ngôn ngữ phụ đề mặc định `"en,vi"` và không giới hạn số lượng video.
    pub fn new(
        engine: Arc<WikiEngine>,
        autoflow_notify: Arc<Notify>,
        work_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            engine,
            autoflow_notify,
            work_dir: work_dir.into(),
            langs: "en,vi".to_string(),
            limit: usize::MAX,
            playlist_url: Mutex::new(String::new()),
            videos: Mutex::new(Vec::new()),
        }
    }

    /// Tùy chỉnh danh sách ngôn ngữ phụ đề ưu tiên (ví dụ `"en,vi"`).
    pub fn with_langs(mut self, langs: impl Into<String>) -> Self {
        self.langs = langs.into();
        self
    }

    /// Giới hạn số lượng video tối đa lấy từ playlist.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Lấy bản sao danh sách các video đã quét/lưu trong phiên hiện tại.
    pub fn recorded_videos(&self) -> Vec<PlaylistVideoEntry> {
        self.videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Xuất cặp `HostTool` `[yt_list_playlist, yt_fetch_transcript]` gắn với `Arc<YoutubeCollectorEnv>`.
    pub fn host_tools(self: &Arc<Self>) -> Vec<Arc<dyn HostTool>> {
        youtube_host_tools(Arc::clone(self))
    }
}

/// Xuất cặp `HostTool` `[yt_list_playlist, yt_fetch_transcript]` từ `Arc<YoutubeCollectorEnv>`.
pub fn youtube_host_tools(env: Arc<YoutubeCollectorEnv>) -> Vec<Arc<dyn HostTool>> {
    vec![
        Arc::new(YtListPlaylistTool {
            env: Arc::clone(&env),
        }),
        Arc::new(YtFetchTranscriptTool { env }),
    ]
}

/// Công cụ `yt_list_playlist`: lấy danh sách video từ URL YouTube Playlist bằng `yt-dlp`.
pub struct YtListPlaylistTool {
    pub env: Arc<YoutubeCollectorEnv>,
}

impl YtListPlaylistTool {
    pub fn new(env: Arc<YoutubeCollectorEnv>) -> Self {
        Self { env }
    }
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
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Thiếu hoặc rỗng: url"))?
            .to_string();

        *self
            .env
            .playlist_url
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = url.clone();
        ctx.progress("Đang lấy danh sách video từ playlist bằng yt-dlp...");

        let mut raw_list = tokio::task::spawn_blocking(move || fetch_playlist(&url))
            .await
            .map_err(|e| host_tool_err("INTERNAL", e.to_string()))??;
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
        *self.env.videos.lock().unwrap_or_else(|e| e.into_inner()) = entries;
        Ok(host_tool_outcome(
            &ctx,
            text,
            json!({ "video_count": count }),
        ))
    }
}

/// Công cụ `yt_fetch_transcript`: tải phụ đề/transcript của một video YouTube bằng `yt-dlp`,
/// lưu vào `sources/<topic_folder>/<file_stem>.md` và kích hoạt `autoflow_notify`.
pub struct YtFetchTranscriptTool {
    pub env: Arc<YoutubeCollectorEnv>,
}

impl YtFetchTranscriptTool {
    pub fn new(env: Arc<YoutubeCollectorEnv>) -> Self {
        Self { env }
    }
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
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Thiếu hoặc rỗng: video_id"))?
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
            .unwrap_or_else(|e| e.into_inner())
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
            .map_err(|e| host_tool_err("INTERNAL", e.to_string()))??;

        let playlist_url = self
            .env
            .playlist_url
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
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
            .map_err(|e| host_tool_err("WIKI_SOURCE_WRITE_FAILED", e.to_string()))?;

        if let Some(entry) = self
            .env
            .videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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

fn http_url(url: &str) -> Result<(), ToolError> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(host_tool_err("INVALID_URL", "Chỉ hỗ trợ URL http(s)"))
    }
}

fn ytdlp_unavailable(e: io::Error) -> ToolError {
    host_tool_err(
        "YTDLP_UNAVAILABLE",
        format!("Không chạy được yt-dlp (cài bằng `brew install yt-dlp`): {e}"),
    )
}

fn stderr_tail(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let tail: Vec<char> = s.chars().rev().take(600).collect();
    tail.into_iter().rev().collect()
}

/// Gọi `yt-dlp --flat-playlist` để lấy danh sách `(video_id, title)` từ URL playlist.
pub fn fetch_playlist(url: &str) -> Result<Vec<(String, String)>, ToolError> {
    http_url(url)?;
    let out = std::process::Command::new("yt-dlp")
        .args(["--flat-playlist", "--print", "%(id)s\t%(title)s", "--"])
        .arg(url)
        .output()
        .map_err(ytdlp_unavailable)?;
    if !out.status.success() {
        return Err(host_tool_err("YTDLP_FAILED", stderr_tail(&out.stderr)));
    }
    let list: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (id, title) = l.trim().split_once('\t')?;
            (!id.trim().is_empty()).then(|| (id.trim().to_string(), title.trim().to_string()))
        })
        .collect();
    if list.is_empty() {
        return Err(host_tool_err(
            "EMPTY_PLAYLIST",
            "Playlist không có video nào",
        ));
    }
    Ok(list)
}

/// Gọi `yt-dlp` tải phụ đề VTT cho `video_id` theo thứ tự ngôn ngữ ưu tiên `langs` và làm sạch bằng `vtt_to_text`.
pub fn fetch_transcript(work: &Path, id: &str, langs: &str) -> Result<String, ToolError> {
    let sub_langs = langs
        .split(',')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .flat_map(|l| [format!("{l}-orig"), l.to_string()])
        .collect::<Vec<_>>()
        .join(",");
    let dir = work.join(id);
    std::fs::create_dir_all(&dir).map_err(|e| host_tool_err("IO", e.to_string()))?;
    let output = std::process::Command::new("yt-dlp")
        .args([
            "--skip-download",
            "--no-playlist",
            "--write-subs",
            "--write-auto-subs",
            "--sub-langs",
            &sub_langs,
            "--sub-format",
            "vtt",
            "-o",
        ])
        .arg(dir.join("%(id)s.%(ext)s"))
        .arg("--")
        .arg(format!("https://www.youtube.com/watch?v={id}"))
        .output()
        .map_err(ytdlp_unavailable)?;
    let mut subs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| host_tool_err("IO", e.to_string()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "vtt"))
        .collect();
    if subs.is_empty() {
        return Err(if output.status.success() {
            host_tool_err(
                "NO_TRANSCRIPT",
                format!("Video {id} không có phụ đề cho: {sub_langs}"),
            )
        } else {
            host_tool_err("YTDLP_FAILED", stderr_tail(&output.stderr))
        });
    }
    let order: Vec<&str> = sub_langs.split(',').collect();
    subs.sort_by_key(|p| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let rank = order
            .iter()
            .position(|l| name.contains(&format!(".{l}.")))
            .unwrap_or(usize::MAX);
        (rank, name)
    });
    let text = vtt_to_text(
        &std::fs::read_to_string(&subs[0]).map_err(|e| host_tool_err("IO", e.to_string()))?,
    );
    if text.is_empty() {
        return Err(host_tool_err("NO_TRANSCRIPT", "Phụ đề rỗng"));
    }
    Ok(text)
}
