//! Đọc cấu hình `.env.live` (tự động tìm ở thư mục hiện tại hoặc crate anh em)
//! và khởi tạo `WorkspaceLayout` dùng chung cho mọi example.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use orchestrator_flow_rs::WorkspaceLayout;

pub type ExampleResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Thư mục dùng chung mặc định cho tất cả các example (`target/homelab_shared_workspace`).
pub fn default_shared_workspace_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("homelab_shared_workspace")
}

pub fn resolve_shared_layout(custom_dir: Option<PathBuf>) -> ExampleResult<WorkspaceLayout> {
    let dir = custom_dir
        .or_else(|| std::env::var_os("HOMELAB_WORKSPACE_DIR").map(PathBuf::from))
        .unwrap_or_else(default_shared_workspace_dir);
    let layout = WorkspaceLayout::new(dir);
    layout.ensure_dirs()?;
    Ok(layout)
}

#[derive(Debug, Clone)]
pub struct LlmCredentials {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

pub fn load_llm_credentials(custom_env: Option<&Path>) -> ExampleResult<LlmCredentials> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        custom_env.map(PathBuf::from),
        std::env::var_os("LIVE_ENV_FILE").map(PathBuf::from),
        Some(manifest_dir.join(".env.live")),
        manifest_dir
            .parent()
            .map(|p| p.join("orchestrator_agent_rs/.env.live")),
        manifest_dir
            .parent()
            .map(|p| p.join("llm-wiki-rs/.env.live")),
    ];

    let mut map = HashMap::new();
    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            map = parse_env_file(&candidate);
            if !map.is_empty() {
                break;
            }
        }
    }

    let get_val = |keys: &[&str]| -> ExampleResult<String> {
        for k in keys {
            if let Ok(v) = std::env::var(k) {
                if !v.trim().is_empty() {
                    return Ok(v.trim().to_string());
                }
            }
            if let Some(v) = map.get(*k) {
                if !v.trim().is_empty() {
                    return Ok(v.trim().to_string());
                }
            }
        }
        Err(io::Error::other(format!(
            "Missing LLM credential key (tried {:?}) in .env.live or environment",
            keys
        ))
        .into())
    };

    Ok(LlmCredentials {
        base_url: get_val(&["G0_OPENAI_BASE_URL", "OPENAI_BASE_URL"])?,
        model: get_val(&["G0_OPENAI_MODEL", "OPENAI_MODEL"])?,
        api_key: get_val(&["G0_OPENAI_API_KEY", "OPENAI_API_KEY"])?,
    })
}
