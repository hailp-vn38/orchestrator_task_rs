//! Bộ công cụ `HostTool` thu thập YouTube Playlist và Transcript bằng `yt-dlp`
//! (`yt_list_playlist`, `yt_fetch_transcript`) cho subagent `source_collector`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use llm_wiki::{
    clip_transcript, sanitize_topic_folder, slugify, vtt_to_text, SourceMatch, WikiEngine,
    WriteTopicSourceRequest,
};
use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::store::{
    CollectionItemRow, TaskStore, ITEM_DUPLICATE, ITEM_FAILED, ITEM_NO_TRANSCRIPT, ITEM_SAVED,
};
use crate::tools::source_identity::{
    content_sha256, ingest_status_label, split_scoped_path, stem_free_of_collision, ExistingReason,
    SaveSourceOutcome, CONTENT_HASH_KEY, VIDEO_ID_KEY,
};
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Giới hạn ký tự cho bản xem trước ngắn khi lưu transcript vào `sources/`.
const COLLECTOR_PREVIEW_CHARS: usize = 600;

/// Giới hạn ký tự tối đa trả về trực tiếp khi đọc/tóm tắt transcript (`yt_fetch_transcript`).
const READONLY_TRANSCRIPT_MAX_CHARS: usize = 32_000;

/// Thời gian tối đa của một lệnh `yt-dlp`. Một lần tải transcript chạy tối đa 2 lệnh, nên luôn kết
/// thúc trước `tool_timeout` (300s) của `add_to_sources`: lỗi rõ ràng, không thành "outcome unknown".
const YTDLP_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Video lỗi từng này lần thì không còn nằm trong danh sách việc cần làm.
const MAX_ITEM_ATTEMPTS: i64 = 3;

const DEFAULT_STATUS_LIMIT: usize = 15;
const MAX_STATUS_LIMIT: usize = 50;

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
    ytdlp_bin: PathBuf,
    ytdlp_timeout: Duration,
    ledger: OnceLock<TaskStore>,
}

fn video_stem(title: &str, video_id: &str) -> String {
    format!("{}-{video_id}", slugify(title, 36))
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
            ytdlp_bin: PathBuf::from("yt-dlp"),
            ytdlp_timeout: YTDLP_COMMAND_TIMEOUT,
            ledger: OnceLock::new(),
        }
    }

    /// Chọn chương trình `yt-dlp` và thời gian tối đa của mỗi lệnh.
    pub fn with_ytdlp(mut self, bin: impl Into<PathBuf>, timeout: Duration) -> Self {
        self.ytdlp_bin = bin.into();
        self.ytdlp_timeout = timeout;
        self
    }

    pub(crate) fn attach_ledger(&self, ledger: TaskStore) {
        let _ = self.ledger.set(ledger);
    }

    async fn record_outcome(
        &self,
        video_id: &str,
        status: &str,
        source_path: Option<&str>,
        error: Option<&str>,
    ) {
        let Some(ledger) = self.ledger.get() else {
            return;
        };
        if let Err(e) = ledger
            .record_collection_outcome(video_id, status, source_path, error)
            .await
        {
            tracing::warn!("could not record collection outcome for {video_id}: {e}");
        }
    }

    async fn run_ytdlp(&self, args: Vec<std::ffi::OsString>) -> Result<Output, ToolError> {
        let mut command = tokio::process::Command::new(&self.ytdlp_bin);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(self.ytdlp_timeout, command.output()).await {
            Ok(result) => result.map_err(ytdlp_unavailable),
            Err(_) => Err(host_tool_err(
                "YTDLP_TIMEOUT",
                format!(
                    "yt-dlp không trả lời sau {}s; video này có thể thử lại sau",
                    self.ytdlp_timeout.as_secs()
                ),
            )),
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
        let transcript = self.fetch_transcript(video_id).await?;
        self.transcripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(video_id.to_string(), transcript.clone());
        Ok(transcript)
    }

    /// Lưu transcript của `video_id` vào `sources/<topic_folder>/` và kích hoạt `autoflow_notify`.
    /// Video đã có (theo `video_id`, hoặc theo nội dung nếu là bản đăng lại) không được tải hay ghi
    /// lại, trừ khi `force`.
    pub async fn save_video_to_sources(
        &self,
        video_id: &str,
        raw_topic_folder: &str,
        custom_title: Option<&str>,
        custom_stem: Option<&str>,
        tags: Vec<String>,
        force: bool,
    ) -> Result<SaveSourceOutcome, ToolError> {
        self.save_video_to_sources_with_autoflow(
            video_id,
            raw_topic_folder,
            custom_title,
            custom_stem,
            tags,
            force,
            true,
        )
        .await
    }

    /// Save a transcript while optionally suppressing the Auto-Flow wake. Callers that suppress
    /// the wake must persist a durable deferred-ingest record before this source becomes visible.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_video_to_sources_with_autoflow(
        &self,
        video_id: &str,
        raw_topic_folder: &str,
        custom_title: Option<&str>,
        custom_stem: Option<&str>,
        tags: Vec<String>,
        force: bool,
        notify_autoflow: bool,
    ) -> Result<SaveSourceOutcome, ToolError> {
        let known_source = self.find_source(VIDEO_ID_KEY, video_id)?;
        if let (false, Some(found)) = (force, known_source.clone()) {
            self.record_outcome(video_id, ITEM_SAVED, Some(&found.scoped_path), None)
                .await;
            return Ok(SaveSourceOutcome::Existing {
                found,
                reason: ExistingReason::SameIdentity,
            });
        }

        let known_entry = self
            .videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|v| v.id == video_id)
            .cloned();
        // Sau restart danh sách trong RAM mất, sổ theo dõi vẫn còn tiêu đề và playlist.
        let ledger_entry = match self.ledger.get() {
            Some(ledger) => ledger
                .collection_item_identity(video_id)
                .await
                .map_err(|e| host_tool_err("COLLECTION_LEDGER_FAILED", e.to_string()))?,
            None => None,
        };
        let (known_title, playlist_url, idx) = match (known_entry, ledger_entry) {
            (Some(entry), _) => (
                Some(entry.title),
                self.playlist_url
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                entry.index,
            ),
            (None, Some((title, playlist, position))) => (
                Some(title),
                playlist,
                usize::try_from(position).unwrap_or(0),
            ),
            (None, None) => (None, String::new(), 0),
        };
        let title = custom_title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or(known_title)
            .unwrap_or_else(|| video_id.to_string());

        // Chỉ lần lưu mới tính vào sổ: đọc transcript để tóm tắt không phải là bước thu thập.
        let transcript = match self.get_or_fetch_transcript(video_id).await {
            Ok(transcript) => transcript,
            Err(error) => {
                let status = if error.code == "NO_TRANSCRIPT" {
                    ITEM_NO_TRANSCRIPT
                } else {
                    ITEM_FAILED
                };
                self.record_outcome(video_id, status, None, Some(&error.message))
                    .await;
                return Err(error);
            }
        };
        let content_hash = content_sha256(&transcript);
        if !force {
            if let Some(found) = self.find_source(CONTENT_HASH_KEY, &content_hash)? {
                self.record_outcome(video_id, ITEM_DUPLICATE, Some(&found.scoped_path), None)
                    .await;
                return Ok(SaveSourceOutcome::Existing {
                    found,
                    reason: ExistingReason::SameContent,
                });
            }
        }

        let (topic_folder, stem) = match known_source.as_ref().filter(|_| force) {
            // Làm lại giữ nguyên đường dẫn cũ để Auto-Flow gộp vào trang wiki hiện có.
            Some(found) => split_scoped_path(&found.scoped_path),
            None => {
                let topic_folder = sanitize_topic_folder(raw_topic_folder);
                let requested = custom_stem
                    .map(|s| slugify(s, 64))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| video_stem(&title, video_id));
                let stem = stem_free_of_collision(
                    &self.engine,
                    &topic_folder,
                    &requested,
                    (VIDEO_ID_KEY, video_id),
                    video_id,
                );
                (topic_folder, stem)
            }
        };

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
                    (VIDEO_ID_KEY.to_string(), json!(video_id)),
                    (CONTENT_HASH_KEY.to_string(), json!(content_hash)),
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
        self.record_outcome(video_id, ITEM_SAVED, Some(&written.info.scoped_path), None)
            .await;

        if notify_autoflow {
            self.autoflow_notify.notify_one();
        }
        Ok(SaveSourceOutcome::Written(Box::new(written)))
    }

    fn find_source(&self, key: &str, value: &str) -> Result<Option<SourceMatch>, ToolError> {
        self.engine
            .find_source_by_frontmatter(key, value)
            .map_err(|e| host_tool_err("WIKI_SOURCE_LOOKUP_FAILED", e.to_string()))
    }

    /// Tóm tắt playlist và các video cần làm tiếp, lấy `sources/` làm sự thật cho video đã lưu.
    async fn collection_report(
        &self,
        collection_key: &str,
        limit: usize,
    ) -> Result<Option<CollectionReport>, ToolError> {
        let Some(ledger) = self.ledger.get() else {
            return Ok(None);
        };
        let items = ledger
            .list_collection_items(collection_key)
            .await
            .map_err(|e| host_tool_err("COLLECTION_LEDGER_FAILED", e.to_string()))?;
        if items.is_empty() {
            return Ok(None);
        }
        let in_vault = self
            .engine
            .sources_by_frontmatter(VIDEO_ID_KEY)
            .map_err(|e| host_tool_err("WIKI_SOURCE_LOOKUP_FAILED", e.to_string()))?;
        Ok(Some(CollectionReport::build(items, &in_vault, limit)))
    }

    async fn fetch_playlist(&self, url: &str) -> Result<Vec<(String, String)>, ToolError> {
        http_url(url)?;
        let out = self
            .run_ytdlp(
                ["--flat-playlist", "--print", "%(id)s\t%(title)s", "--", url]
                    .map(Into::into)
                    .to_vec(),
            )
            .await?;
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

    /// Tải phụ đề VTT cho `id` theo thứ tự ngôn ngữ ưu tiên và làm sạch bằng `vtt_to_text`.
    async fn fetch_transcript(&self, id: &str) -> Result<String, ToolError> {
        let parsed_langs: Vec<&str> = self
            .langs
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
        let dir = self.work_dir.join(id);
        std::fs::create_dir_all(&dir).map_err(|e| host_tool_err("IO", e.to_string()))?;

        let mut subs = collect_vtts(&dir)?;
        let mut last_output = None;

        // Ưu tiên tải phụ đề gốc (`*-orig`) trước để tránh bị YouTube trả HTTP 429 khi dịch tự động (`&tlang=`).
        for candidate_langs in [&orig_langs, &sub_langs] {
            if !subs.is_empty() || candidate_langs.is_empty() {
                break;
            }
            let args: Vec<std::ffi::OsString> = vec![
                "--skip-download".into(),
                "--no-playlist".into(),
                "--write-subs".into(),
                "--write-auto-subs".into(),
                "--sub-langs".into(),
                candidate_langs.into(),
                "--sub-format".into(),
                "vtt".into(),
                "-o".into(),
                dir.join("%(id)s.%(ext)s").into(),
                "--".into(),
                format!("https://www.youtube.com/watch?v={id}").into(),
            ];
            last_output = Some(self.run_ytdlp(args).await?);
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
}

fn collect_vtts(dir: &Path) -> Result<Vec<PathBuf>, ToolError> {
    Ok(std::fs::read_dir(dir)
        .map_err(|e| host_tool_err("IO", e.to_string()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "vtt"))
        .collect())
}

/// Trạng thái playlist trả cho agent: đủ để một session mới biết phải làm tiếp video nào.
struct CollectionReport {
    total: usize,
    in_sources: usize,
    duplicates: usize,
    no_transcript: usize,
    gave_up: usize,
    remaining: usize,
    next: Vec<CollectionItemRow>,
    statuses: HashMap<String, String>,
}

impl CollectionReport {
    fn build(
        items: Vec<CollectionItemRow>,
        in_vault: &HashMap<String, SourceMatch>,
        limit: usize,
    ) -> Self {
        let mut report = Self {
            total: items.len(),
            in_sources: 0,
            duplicates: 0,
            no_transcript: 0,
            gave_up: 0,
            remaining: 0,
            next: Vec::new(),
            statuses: HashMap::new(),
        };
        for item in items {
            let status = if let Some(found) = in_vault.get(&item.item_id) {
                report.in_sources += 1;
                ingest_status_label(found.ingest_state).to_string()
            } else if item.status == ITEM_DUPLICATE {
                report.duplicates += 1;
                "duplicate_of".to_string()
            } else if item.status == ITEM_NO_TRANSCRIPT {
                report.no_transcript += 1;
                "no_transcript".to_string()
            } else if item.attempts >= MAX_ITEM_ATTEMPTS {
                report.gave_up += 1;
                format!("gave_up(attempts={})", item.attempts)
            } else {
                report.remaining += 1;
                let label = if item.status == ITEM_FAILED {
                    format!("failed(attempts={})", item.attempts)
                } else {
                    "new".to_string()
                };
                if report.next.len() < limit {
                    report.next.push(item.clone());
                }
                label
            };
            report.statuses.insert(item.item_id, status);
        }
        report
    }

    fn summary(&self) -> String {
        format!(
            "TOTAL: {} | IN_SOURCES: {} | DUPLICATE: {} | REMAINING: {} | NO_TRANSCRIPT: {} | GAVE_UP: {}",
            self.total,
            self.in_sources,
            self.duplicates,
            self.remaining,
            self.no_transcript,
            self.gave_up
        )
    }
}

/// Xuất cặp `HostTool` `[yt_list_playlist, yt_fetch_transcript]` từ `Arc<YoutubeCollectorEnv>`.
pub fn youtube_host_tools(env: Arc<YoutubeCollectorEnv>) -> Vec<Arc<dyn HostTool>> {
    vec![
        Arc::new(YtListPlaylistTool {
            env: Arc::clone(&env),
        }),
        Arc::new(YtFetchTranscriptTool {
            env: Arc::clone(&env),
        }),
        Arc::new(CollectionStatusTool { env }),
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
            description: "List all videos in a YouTube playlist URL using yt-dlp and record them in the collection ledger. Returns `index`, `video_id`, `file_stem`, `status` (new, failed, no_transcript, gave_up, duplicate_of, pending_ingest, already_ingested) and `title`, so the agent can skip videos already in `sources/` and choose a topic folder (`<domain>/<topic>`).".to_string(),
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

        let mut raw_list = self.env.fetch_playlist(&url).await?;
        raw_list.truncate(self.env.limit);
        if let Some(ledger) = self.env.ledger.get() {
            ledger
                .upsert_collection_items(&url, &raw_list)
                .await
                .map_err(|e| host_tool_err("COLLECTION_LEDGER_FAILED", e.to_string()))?;
        }

        let entries: Vec<PlaylistVideoEntry> = raw_list
            .into_iter()
            .enumerate()
            .map(|(i, (id, title))| {
                let stem = video_stem(&title, &id);
                PlaylistVideoEntry {
                    index: i + 1,
                    id,
                    title,
                    stem,
                    saved_source_scoped_path: None,
                }
            })
            .collect();

        let report = self.env.collection_report(&url, 0).await?;
        let mut text = report
            .as_ref()
            .map(|r| format!("{}\n", r.summary()))
            .unwrap_or_default();
        for v in &entries {
            let status = report
                .as_ref()
                .and_then(|r| r.statuses.get(&v.id))
                .map(String::as_str)
                .unwrap_or("new");
            text.push_str(&format!(
                "{}. video_id={} file_stem={} status={status} title={}\n",
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
        let transcript = match self.env.get_or_fetch_transcript(&video_id).await {
            Ok(transcript) => transcript,
            Err(error) => {
                let status = if error.code == "NO_TRANSCRIPT" {
                    ITEM_NO_TRANSCRIPT
                } else {
                    ITEM_FAILED
                };
                self.env
                    .record_outcome(&video_id, status, None, Some(&error.message))
                    .await;
                return Err(error);
            }
        };

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

/// Công cụ `collection_status`: đọc sổ theo dõi của playlist, không gọi mạng. Session mới hoặc Task
/// làm lại dùng nó để biết còn video nào, thay vì dựa vào context đã bị nén.
pub struct CollectionStatusTool {
    pub env: Arc<YoutubeCollectorEnv>,
}

#[async_trait::async_trait]
impl HostTool for CollectionStatusTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "collection_status".to_string(),
            description: "Read the collection ledger of a YouTube playlist already listed by `yt_list_playlist`: totals plus the next videos still to process. Makes no network call. Use it to pick the next batch after a restart, in a new session, or when retrying a job.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["playlist_url"],
                "properties": {
                    "playlist_url": {
                        "type": "string",
                        "description": "The same playlist URL passed to `yt_list_playlist`."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many remaining videos to return (default 15, max 50)."
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let url = args
            .get("playlist_url")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Thiếu hoặc rỗng: playlist_url"))?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_STATUS_LIMIT, |n| n as usize)
            .clamp(1, MAX_STATUS_LIMIT);
        let report = self
            .env
            .collection_report(url, limit)
            .await?
            .ok_or_else(|| {
                host_tool_err(
                    "UNKNOWN_COLLECTION",
                    "Playlist chưa có trong sổ theo dõi; gọi yt_list_playlist trước",
                )
            })?;
        let mut text = format!("{}\nNEXT:\n", report.summary());
        for item in &report.next {
            text.push_str(&format!(
                "{}. video_id={} attempts={} title={}\n",
                item.position, item.item_id, item.attempts, item.title
            ));
            if let Some(error) = &item.last_error {
                text.push_str(&format!("   last_error={}\n", clip_transcript(error, 200)));
            }
        }
        Ok(host_tool_outcome(
            &ctx,
            text,
            json!({
                "total": report.total,
                "in_sources": report.in_sources,
                "duplicates": report.duplicates,
                "remaining": report.remaining,
                "no_transcript": report.no_transcript,
                "gave_up": report.gave_up,
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
