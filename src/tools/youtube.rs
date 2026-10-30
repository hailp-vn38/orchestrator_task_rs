//! Bộ công cụ `HostTool` thu thập YouTube Playlist và Transcript bằng `yt-dlp`
//! (`yt_list_playlist`, `yt_fetch_transcript`) cho subagent `source_collector`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use llm_wiki::{
    clip_transcript, sanitize_topic_folder, slugify, vtt_to_text, WikiEngine,
    WriteTopicSourceRequest, WriteTopicSourceResponse,
};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Giới hạn ký tự cho bản xem trước ngắn khi lưu transcript vào `sources/`.
const COLLECTOR_PREVIEW_CHARS: usize = 600;

/// Giới hạn ký tự tối đa trả về trực tiếp khi đọc/tóm tắt transcript (`yt_fetch_transcript`).
const READONLY_TRANSCRIPT_MAX_CHARS: usize = 32_000;

/// Thông tin một video trong YouTube Playlist đã được liệt kê hoặc tải phụ đề.
#[derive(Clone, Debug)]
pub struct PlaylistVideoEntry {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub stem: String,
    /// Đường dẫn tương đối thực tế bên trong `sources/` sau khi lưu qua `add_to_sources`
    /// (ví dụ: `Some("algo-trading/mql5-dca/01-....md")`).
    pub saved_source_scoped_path: Option<String>,
}

/// Môi trường trạng thái dùng chung giữa `YtListPlaylistTool`, `YtFetchTranscriptTool` và `AddToSourcesTool`.
pub struct YoutubeCollectorEnv {
    pub engine: Arc<WikiEngine>,
    pub autoflow_notify: Arc<Notify>,
    pub work_dir: PathBuf,
    pub langs: String,
    pub limit: usize,
    pub playlist_url: Mutex<String>,
    pub videos: Mutex<Vec<PlaylistVideoEntry>>,
    pub transcripts: Mutex<HashMap<String, String>>,
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
            transcripts: Mutex::new(HashMap::new()),
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

    /// Tải (hoặc lấy từ bộ nhớ đệm) transcript cho `video_id`.
    pub async fn get_or_fetch_transcript(&self, video_id: &str) -> Result<String, ToolError> {
        if let Some(cached) = self
            .transcripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(video_id)
            .cloned()
        {
            return Ok(cached);
        }
        let work = self.work_dir.clone();
        let langs = self.langs.clone();
        let vid = video_id.to_string();
        let transcript = tokio::task::spawn_blocking(move || fetch_transcript(&work, &vid, &langs))
            .await
            .map_err(|e| host_tool_err("INTERNAL", e.to_string()))??;
        self.transcripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(video_id.to_string(), transcript.clone());
        Ok(transcript)
    }

    /// Lưu transcript của `video_id` vào `sources/<topic_folder>/<stem>.md` và kích hoạt `autoflow_notify`.
    pub async fn save_video_to_sources(
        &self,
        video_id: &str,
        raw_topic_folder: &str,
        custom_title: Option<&str>,
        custom_stem: Option<&str>,
        tags: Vec<String>,
    ) -> Result<WriteTopicSourceResponse, ToolError> {
        let topic_folder = sanitize_topic_folder(raw_topic_folder);
        let known_entry = self
            .videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|v| v.id == video_id)
            .cloned();

        let (default_stem, default_title, idx) = if let Some(entry) = known_entry {
            (entry.stem, entry.title, entry.index)
        } else {
            let fallback_title = custom_title
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(video_id)
                .to_string();
            let fallback_stem = slugify(&fallback_title, 36);
            (fallback_stem, fallback_title, 0)
        };

        let title = custom_title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&default_title)
            .to_string();
        let stem = custom_stem
            .map(|s| slugify(s, 64))
            .filter(|s| !s.is_empty())
            .unwrap_or(default_stem);

        let transcript = self.get_or_fetch_transcript(video_id).await?;
        let playlist_url = self
            .playlist_url
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let youtube_url = format!("https://www.youtube.com/watch?v={video_id}");
        let youtube_embed_md = format!("![{title}]({youtube_url})");

        let written = self
            .engine
            .write_topic_source(WriteTopicSourceRequest {
                topic_folder,
                file_stem: Some(stem),
                title,
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
                    ("YouTube URL".to_string(), youtube_embed_md),
                    ("Playlist URL".to_string(), format!("<{playlist_url}>")),
                    ("Video ID".to_string(), format!("`{video_id}`")),
                    ("Thứ tự trong Playlist".to_string(), format!("#{idx}")),
                ],
                preview_chars: Some(COLLECTOR_PREVIEW_CHARS),
            })
            .map_err(|e| host_tool_err("WIKI_SOURCE_WRITE_FAILED", e.to_string()))?;

        if let Some(entry) = self
            .videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|v| v.id == video_id)
        {
            entry.saved_source_scoped_path = Some(written.info.scoped_path.clone());
        }

        self.autoflow_notify.notify_one();
        Ok(written)
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

/// Công cụ `yt_fetch_transcript`: tải phụ đề/transcript của một video YouTube bằng `yt-dlp` ở chế độ chỉ đọc (`ReadOnly`)
/// và lưu tạm vào bộ nhớ đệm của `YoutubeCollectorEnv` (nếu cần lưu vào `sources/`, gọi công cụ hợp nhất `add_to_sources`).
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
            description: "Read-only tool: download and return the subtitles/transcript of a YouTube video using yt-dlp for summarization or Q&A. Does NOT write any file to `sources/`. (If the user explicitly asks to save the video into `sources/` or the Wiki, call `add_to_sources` with `video_id`).".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["video_id"],
                "properties": {
                    "video_id": {
                        "type": "string",
                        "description": "YouTube video ID from `yt_list_playlist` or URL"
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional video title from `yt_list_playlist`"
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let video_id = args
            .get("video_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Thiếu hoặc rỗng: video_id"))?
            .to_string();

        let known_entry = self
            .env
            .videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|v| v.id == video_id)
            .cloned();

        let title = if let Some(entry) = known_entry {
            entry.title
        } else {
            args.get("title")
                .and_then(Value::as_str)
                .unwrap_or(&video_id)
                .to_string()
        };

        let youtube_url = format!("https://www.youtube.com/watch?v={video_id}");

        ctx.progress(&format!(
            "Đang tải transcript bằng yt-dlp cho video `{video_id}` (chế độ ReadOnly)..."
        ));
        let transcript = self.env.get_or_fetch_transcript(&video_id).await?;

        let char_count = transcript.chars().count();
        let line_count = transcript.lines().count();
        let body = clip_transcript(&transcript, READONLY_TRANSCRIPT_MAX_CHARS);

        ctx.progress(&format!(
            "Đã tải transcript video `{video_id}` ({char_count} ký tự, {line_count} dòng)"
        ));

        let agent_output = format!(
            "VIDEO_ID: {video_id}\n\
             TITLE: {title}\n\
             YOUTUBE_URL: {youtube_url}\n\
             YOUTUBE_MARKDOWN: ![{title}]({youtube_url})\n\
             CONTENT_LENGTH_CHARS: {char_count}\n\
             CONTENT_LINES: {line_count}\n\n\
             === BEGIN TRANSCRIPT ===\n\
             {body}\n\
             === END TRANSCRIPT ==="
        );

        Ok(host_tool_outcome(
            &ctx,
            agent_output,
            json!({
                "video_id": video_id,
                "title": title,
                "youtube_url": youtube_url,
                "chars": char_count,
                "lines": line_count,
                "saved_to_sources": false,
                "autoflow_triggered": false
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
    let parsed_langs: Vec<&str> = langs
        .split(',')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let orig_langs = parsed_langs
        .iter()
        .map(|l| format!("{l}-orig"))
        .collect::<Vec<_>>()
        .join(",");
    let sub_langs = parsed_langs
        .iter()
        .flat_map(|l| [format!("{l}-orig"), (*l).to_string()])
        .collect::<Vec<_>>()
        .join(",");
    let dir = work.join(id);
    std::fs::create_dir_all(&dir).map_err(|e| host_tool_err("IO", e.to_string()))?;

    let collect_vtts = |d: &Path| -> Result<Vec<PathBuf>, ToolError> {
        Ok(std::fs::read_dir(d)
            .map_err(|e| host_tool_err("IO", e.to_string()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "vtt"))
            .collect())
    };

    let mut subs = collect_vtts(&dir)?;
    let mut last_output = None;

    // Ưu tiên tải phụ đề gốc (`*-orig`) trước để tránh bị YouTube trả HTTP 429 khi dịch tự động (`&tlang=`).
    for candidate_langs in [&orig_langs, &sub_langs] {
        if !subs.is_empty() || candidate_langs.is_empty() {
            break;
        }
        let output = std::process::Command::new("yt-dlp")
            .args([
                "--skip-download",
                "--no-playlist",
                "--write-subs",
                "--write-auto-subs",
                "--sub-langs",
                candidate_langs,
                "--sub-format",
                "vtt",
                "-o",
            ])
            .arg(dir.join("%(id)s.%(ext)s"))
            .arg("--")
            .arg(format!("https://www.youtube.com/watch?v={id}"))
            .output()
            .map_err(ytdlp_unavailable)?;
        last_output = Some(output);
        subs = collect_vtts(&dir)?;
    }

    if subs.is_empty() {
        return Err(match last_output {
            Some(output) if !output.status.success() => {
                host_tool_err("YTDLP_FAILED", stderr_tail(&output.stderr))
            }
            _ => host_tool_err(
                "NO_TRANSCRIPT",
                format!("Video {id} không có phụ đề cho: {sub_langs}"),
            ),
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
