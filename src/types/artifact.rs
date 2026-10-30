//! Value types cho Multi-Kind Task Artifacts (Report, Image, Audio/TTS, Document, Data).

use orchestrator_agent_rs::{ArtifactReader, ArtifactRole, FamilyArtifact};
use serde::{Deserialize, Serialize};

use crate::error::TaskResult;
use crate::types::TaskStatus;

const STREAM_READ_CHUNK_BYTES: usize = 64 * 1024;

/// Phân loại ngữ nghĩa của Artifact được sinh ra trong một Task (từ Root Agent hoặc Subagent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskArtifactKind {
    /// Báo cáo Markdown, văn bản tổng hợp hoặc trang HTML (`text/markdown`, `text/plain`, `text/html`).
    Report,
    /// Hình ảnh minh họa, sơ đồ, biểu đồ hoặc ảnh render (`image/png`, `image/jpeg`, `image/webp`, `image/svg+xml`, `image/gif`).
    Image,
    /// Video minh họa hoặc clip tổng hợp (`video/mp4`, `video/webm`, `video/quicktime`).
    Video,
    /// Âm thanh tổng hợp hoặc giọng đọc TTS (`audio/wav`, `audio/mpeg`, `audio/ogg`, `audio/flac`, `audio/aac`).
    Audio,
    /// Tài liệu định dạng chuẩn (`application/pdf`, `application/epub+zip`, ...).
    Document,
    /// Dữ liệu có cấu trúc (`application/json`, `text/csv`, `application/xml`, ...).
    Data,
    /// Tệp đính kèm hoặc dữ liệu nhị phân khác.
    Other,
}

impl TaskArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Data => "data",
            Self::Other => "other",
        }
    }

    /// Phân tích chuỗi tên loại Artifact (`report`, `image`, `video`, `audio`, `document`, `data`, `other`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "report" | "reports" | "markdown" => Some(Self::Report),
            "image" | "images" => Some(Self::Image),
            "video" | "videos" => Some(Self::Video),
            "audio" | "tts" => Some(Self::Audio),
            "document" | "documents" | "pdf" => Some(Self::Document),
            "data" | "json" | "csv" => Some(Self::Data),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// Tự động suy luận `TaskArtifactKind` từ `mime_type` và phần mở rộng của `name`.
    pub fn from_mime_and_name(mime_type: &str, name: &str) -> Self {
        let base_mime = mime_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let lower_name = name.trim().to_ascii_lowercase();

        if base_mime.starts_with("image/") {
            return Self::Image;
        }
        if base_mime.starts_with("video/") {
            return Self::Video;
        }
        if base_mime.starts_with("audio/") {
            return Self::Audio;
        }
        if matches!(
            base_mime.as_str(),
            "application/pdf" | "application/epub+zip" | "application/msword"
        ) || base_mime.starts_with("application/vnd.openxmlformats-officedocument")
        {
            return Self::Document;
        }
        if matches!(
            base_mime.as_str(),
            "text/markdown" | "text/x-markdown" | "text/html"
        ) {
            return Self::Report;
        }
        if matches!(
            base_mime.as_str(),
            "application/json"
                | "application/ld+json"
                | "application/x-ndjson"
                | "text/csv"
                | "text/tab-separated-values"
                | "application/xml"
                | "text/xml"
        ) {
            return Self::Data;
        }

        // Kiểm tra theo phần mở rộng của tên file trước khi fallback `text/plain`
        if lower_name.ends_with(".png")
            || lower_name.ends_with(".jpg")
            || lower_name.ends_with(".jpeg")
            || lower_name.ends_with(".webp")
            || lower_name.ends_with(".svg")
            || lower_name.ends_with(".gif")
        {
            return Self::Image;
        }
        if lower_name.ends_with(".mp4")
            || lower_name.ends_with(".webm")
            || lower_name.ends_with(".mov")
            || lower_name.ends_with(".mkv")
            || lower_name.ends_with(".avi")
        {
            return Self::Video;
        }
        if lower_name.ends_with(".wav")
            || lower_name.ends_with(".mp3")
            || lower_name.ends_with(".ogg")
            || lower_name.ends_with(".flac")
            || lower_name.ends_with(".m4a")
            || lower_name.ends_with(".aac")
            || lower_name.ends_with(".opus")
        {
            return Self::Audio;
        }
        if lower_name.ends_with(".pdf") || lower_name.ends_with(".epub") {
            return Self::Document;
        }
        if lower_name.ends_with(".json")
            || lower_name.ends_with(".jsonl")
            || lower_name.ends_with(".csv")
            || lower_name.ends_with(".tsv")
            || lower_name.ends_with(".xml")
        {
            return Self::Data;
        }
        if lower_name.ends_with(".md")
            || lower_name.ends_with(".markdown")
            || lower_name.ends_with(".txt")
            || lower_name.ends_with(".html")
            || base_mime == "text/plain"
        {
            return Self::Report;
        }

        Self::Other
    }
}

/// Vai trò hiển thị của một Task Artifact (`primary` cho sản phẩm bàn giao chính, `intermediate` cho dữ liệu trung gian).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskArtifactRole {
    Primary,
    Intermediate,
}

impl TaskArtifactRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Intermediate => "intermediate",
        }
    }

    /// Phân tích chuỗi vai trò Artifact (`primary`, `intermediate`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "primary" => Some(Self::Primary),
            "intermediate" => Some(Self::Intermediate),
            _ => None,
        }
    }

    pub fn from_agent_role(role: ArtifactRole) -> Self {
        match role {
            ArtifactRole::Primary => Self::Primary,
            ArtifactRole::Intermediate => Self::Intermediate,
            _ => Self::Intermediate,
        }
    }

    pub fn to_agent_role(self) -> ArtifactRole {
        match self {
            Self::Primary => ArtifactRole::Primary,
            Self::Intermediate => ArtifactRole::Intermediate,
        }
    }
}

/// Thông tin chi tiết của một Artifact đã commit trong Task (kèm nguồn gốc phiên/subagent/tool tạo ra).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArtifactView {
    pub id: String,
    pub task_id: String,
    pub root_session_id: String,
    pub producer_session_id: String,
    pub producer_agent_id: String,
    pub turn_id: String,
    pub producer_invocation_id: Option<String>,
    pub producer_tool_name: Option<String>,
    pub name: String,
    pub mime_type: String,
    pub kind: TaskArtifactKind,
    pub role: TaskArtifactRole,
    pub size_bytes: u64,
    pub sha256: String,
    /// Thời điểm tạo (unix epoch milliseconds, đồng bộ với `TaskView.created_at`).
    pub created_at: i64,
}

impl TaskArtifactView {
    pub fn from_family_artifact(task_id: &str, fa: &FamilyArtifact) -> Self {
        let kind = TaskArtifactKind::from_mime_and_name(&fa.artifact.mime_type, &fa.artifact.name);
        let role = TaskArtifactRole::from_agent_role(fa.artifact.role);
        let created_at = if fa.created_at < 1_000_000_000_000 {
            fa.created_at.saturating_mul(1000)
        } else {
            fa.created_at
        };
        Self {
            id: fa.artifact.id.clone(),
            task_id: task_id.to_owned(),
            root_session_id: fa.root_session_id.clone(),
            producer_session_id: fa.producer_session_id.clone(),
            producer_agent_id: fa.producer_agent_id.clone(),
            turn_id: fa.turn_id.clone(),
            producer_invocation_id: fa.producer_invocation_id.clone(),
            producer_tool_name: fa.producer_tool_name.clone(),
            name: fa.artifact.name.clone(),
            mime_type: fa.artifact.mime_type.clone(),
            kind,
            role,
            size_bytes: fa.artifact.size_bytes,
            sha256: fa.artifact.sha256.clone(),
            created_at,
        }
    }
}

/// Handle đọc/stream một Task Artifact đã commit kèm xác minh toàn vẹn SHA-256 trên dòng dữ liệu.
#[derive(Debug)]
pub struct TaskArtifactDownload {
    pub metadata: TaskArtifactView,
    pub reader: ArtifactReader,
}

impl TaskArtifactDownload {
    /// Đọc toàn bộ nội dung nhị phân của Artifact vào bộ nhớ và xác minh chữ ký SHA-256 ở cuối luồng.
    pub async fn read_all_bytes(mut self) -> TaskResult<Vec<u8>> {
        let cap = usize::try_from(self.metadata.size_bytes).unwrap_or(0);
        let mut buf = Vec::with_capacity(cap);
        while let Some(chunk) = self.reader.read_chunk(STREAM_READ_CHUNK_BYTES).await? {
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    /// Đọc toàn bộ nội dung văn bản UTF-8 của Artifact và xác minh chữ ký SHA-256.
    pub async fn read_to_string(self) -> TaskResult<String> {
        Ok(self.reader.read_to_string().await?)
    }
}

/// Bộ lọc truy vấn danh mục Artifact toàn cục (`Artifact Catalog`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskArtifactFilter {
    pub kind: Option<TaskArtifactKind>,
    pub role: Option<TaskArtifactRole>,
    pub task_id: Option<String>,
    pub flow_id: Option<String>,
    pub producer_agent_id: Option<String>,
    pub query: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Thống kê tổng hợp của danh mục Artifact (`Artifact Catalog`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArtifactCatalogSummary {
    pub total_artifacts: u64,
    pub primary_artifacts: u64,
    pub intermediate_artifacts: u64,
    pub report_artifacts: u64,
    pub image_artifacts: u64,
    pub video_artifacts: u64,
    pub audio_artifacts: u64,
    pub document_artifacts: u64,
    pub data_artifacts: u64,
    pub other_artifacts: u64,
    pub total_size_bytes: u64,
}

impl TaskArtifactCatalogSummary {
    pub fn record(&mut self, artifact: &TaskArtifactView) {
        self.total_artifacts += 1;
        self.total_size_bytes = self.total_size_bytes.saturating_add(artifact.size_bytes);
        match artifact.role {
            TaskArtifactRole::Primary => self.primary_artifacts += 1,
            TaskArtifactRole::Intermediate => self.intermediate_artifacts += 1,
        }
        match artifact.kind {
            TaskArtifactKind::Report => self.report_artifacts += 1,
            TaskArtifactKind::Image => self.image_artifacts += 1,
            TaskArtifactKind::Video => self.video_artifacts += 1,
            TaskArtifactKind::Audio => self.audio_artifacts += 1,
            TaskArtifactKind::Document => self.document_artifacts += 1,
            TaskArtifactKind::Data => self.data_artifacts += 1,
            TaskArtifactKind::Other => self.other_artifacts += 1,
        }
    }
}

/// Một mục trong danh mục Artifact toàn cục (`Artifact Catalog`), kết hợp metadata của [`TaskArtifactView`]
/// với ngữ cảnh của Task sở hữu (`task_title`, `task_prompt`, `task_status`, `flow_id`, `root_agent_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArtifactCatalogEntry {
    pub artifact: TaskArtifactView,
    pub task_title: String,
    pub task_prompt: String,
    pub task_status: TaskStatus,
    pub flow_id: String,
    pub root_agent_id: String,
    pub owner_id: String,
    pub workspace_id: String,
}

/// Trang kết quả phân trang của danh mục Artifact toàn cục (`Artifact Catalog`) kèm thống kê tổng hợp.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArtifactCatalogPage {
    pub items: Vec<TaskArtifactCatalogEntry>,
    pub total: u64,
    pub summary: TaskArtifactCatalogSummary,
}
