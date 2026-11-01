//! Định danh của một nguồn trong `sources/`: nguồn được nhận ra theo nội dung hoặc ID gốc, không
//! theo tiêu đề hay tên file, nên lưu lại không tạo bản trùng và không ghi đè nguồn khác.

use llm_wiki::{
    parse_frontmatter, SourceIngestState, SourceMatch, WikiEngine, WriteTopicSourceResponse,
};
use orchestrator_agent_rs::{ToolContext, ToolError, ToolOutcome};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

pub(crate) const CONTENT_HASH_KEY: &str = "content_sha256";
pub(crate) const VIDEO_ID_KEY: &str = "video_id";

/// Kết quả của một lần lưu vào `sources/`.
#[derive(Debug)]
pub enum SaveSourceOutcome {
    Written(Box<WriteTopicSourceResponse>),
    /// Không ghi gì vì nguồn này đã có trong `sources/`.
    Existing {
        found: SourceMatch,
        reason: ExistingReason,
    },
}

/// Lý do một nguồn được coi là đã tồn tại.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingReason {
    SameIdentity,
    /// ID khác nhưng nội dung giống hệt, ví dụ video đăng lại.
    SameContent,
}

pub(crate) fn content_sha256(body: &str) -> String {
    Sha256::digest(body.trim().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn ingest_status_label(state: SourceIngestState) -> &'static str {
    match state {
        SourceIngestState::Ingested => "already_ingested",
        SourceIngestState::NotIngested | SourceIngestState::Incomplete => "pending_ingest",
    }
}

/// Tên file không đụng nguồn khác: nếu `stem` đang thuộc về một định danh khác thì thêm `suffix`.
pub(crate) fn stem_free_of_collision(
    engine: &WikiEngine,
    topic_folder: &str,
    stem: &str,
    identity: (&str, &str),
    suffix: &str,
) -> String {
    let path = engine
        .config()
        .sources_dir()
        .join(topic_folder)
        .join(format!("{stem}.md"));
    let Ok(existing) = std::fs::read_to_string(path) else {
        return stem.to_string();
    };
    let same_identity = parse_frontmatter(&existing)
        .json_map
        .and_then(|map| map.get(identity.0).cloned())
        .is_some_and(|value| value.as_str() == Some(identity.1));
    if same_identity {
        stem.to_string()
    } else {
        format!("{stem}-{suffix}")
    }
}

/// Nơi lưu một nguồn được nhận ra theo nội dung.
pub(crate) enum ContentTarget {
    Existing(SourceMatch),
    Write { topic_folder: String, stem: String },
}

/// Quyết định nơi lưu nội dung mới: cùng nội dung thì báo đã có, hoặc ghi lại đúng đường dẫn cũ khi
/// `force`; nội dung khác không bao giờ ghi đè nguồn đang dùng cùng tên file.
pub(crate) fn resolve_content_target(
    engine: &WikiEngine,
    topic_folder: String,
    stem: String,
    content_hash: &str,
    force: bool,
) -> Result<ContentTarget, ToolError> {
    let known = engine
        .find_source_by_frontmatter(CONTENT_HASH_KEY, content_hash)
        .map_err(|e| host_tool_err("WIKI_SOURCE_LOOKUP_FAILED", e.to_string()))?;
    Ok(match known {
        Some(found) if !force => ContentTarget::Existing(found),
        Some(found) => {
            let (topic_folder, stem) = split_scoped_path(&found.scoped_path);
            ContentTarget::Write { topic_folder, stem }
        }
        None => {
            let stem = stem_free_of_collision(
                engine,
                &topic_folder,
                &stem,
                (CONTENT_HASH_KEY, content_hash),
                &content_hash[..12],
            );
            ContentTarget::Write { topic_folder, stem }
        }
    })
}

pub(crate) fn split_scoped_path(scoped_path: &str) -> (String, String) {
    let clean = scoped_path.trim_end_matches(".md");
    match clean.rsplit_once('/') {
        Some((folder, stem)) => (folder.to_string(), stem.to_string()),
        None => (String::new(), clean.to_string()),
    }
}

/// Kết quả trả cho agent khi không ghi gì: nêu rõ trạng thái để agent chuyển sang nguồn tiếp theo.
pub(crate) fn existing_source_tool_outcome(
    ctx: &ToolContext,
    found: &SourceMatch,
    reason: ExistingReason,
) -> ToolOutcome {
    let status = match reason {
        ExistingReason::SameContent => "duplicate_of",
        ExistingReason::SameIdentity => ingest_status_label(found.ingest_state),
    };
    host_tool_outcome(
        ctx,
        format!(
            "SOURCE_ALREADY_EXISTS\nSTATUS: {status}\nSOURCE_SCOPED_PATH: {}\nKhông ghi file mới; chuyển sang nguồn tiếp theo. Chỉ dùng force=true khi người dùng yêu cầu làm lại.",
            found.scoped_path
        ),
        json!({
            "created": false,
            "status": status,
            "source_rel_path": found.scoped_path,
            "vault_rel_path": found.rel_path,
            "autoflow_triggered": false,
        }),
    )
}
