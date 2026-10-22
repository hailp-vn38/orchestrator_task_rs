//! Tích hợp `yt-dlp` (chạy trong `spawn_blocking`) để lấy danh sách video từ YouTube Playlist
//! và tải phụ đề/transcript VTT sạch (`llm_wiki::vtt_to_text`).

use std::io;
use std::path::{Path, PathBuf};

pub use llm_wiki::vtt_to_text;
use orchestrator_agent_rs::ToolError;

use crate::shared::tools::tool_err;

fn http_url(url: &str) -> Result<(), ToolError> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(tool_err("INVALID_URL", "Chỉ hỗ trợ URL http(s)"))
    }
}

fn ytdlp_unavailable(e: io::Error) -> ToolError {
    tool_err(
        "YTDLP_UNAVAILABLE",
        format!("Không chạy được yt-dlp (cài bằng `brew install yt-dlp`): {e}"),
    )
}

fn stderr_tail(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let tail: Vec<char> = s.chars().rev().take(600).collect();
    tail.into_iter().rev().collect()
}

pub fn fetch_playlist(url: &str) -> Result<Vec<(String, String)>, ToolError> {
    http_url(url)?;
    let out = std::process::Command::new("yt-dlp")
        .args(["--flat-playlist", "--print", "%(id)s\t%(title)s", "--"])
        .arg(url)
        .output()
        .map_err(ytdlp_unavailable)?;
    if !out.status.success() {
        return Err(tool_err("YTDLP_FAILED", stderr_tail(&out.stderr)));
    }
    let list: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (id, title) = l.trim().split_once('\t')?;
            (!id.trim().is_empty()).then(|| (id.trim().to_string(), title.trim().to_string()))
        })
        .collect();
    if list.is_empty() {
        return Err(tool_err("EMPTY_PLAYLIST", "Playlist không có video nào"));
    }
    Ok(list)
}

pub fn fetch_transcript(work: &Path, id: &str, langs: &str) -> Result<String, ToolError> {
    let sub_langs = langs
        .split(',')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .flat_map(|l| [format!("{l}-orig"), l.to_string()])
        .collect::<Vec<_>>()
        .join(",");
    let dir = work.join(id);
    std::fs::create_dir_all(&dir).map_err(|e| tool_err("IO", e.to_string()))?;
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
        .map_err(|e| tool_err("IO", e.to_string()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "vtt"))
        .collect();
    if subs.is_empty() {
        return Err(if output.status.success() {
            tool_err(
                "NO_TRANSCRIPT",
                format!("Video {id} không có phụ đề cho: {sub_langs}"),
            )
        } else {
            tool_err("YTDLP_FAILED", stderr_tail(&output.stderr))
        });
    }
    let order: Vec<&str> = sub_langs.split(',').collect();
    subs.sort_by_key(|p| {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let rank = order
            .iter()
            .position(|l| name.contains(&format!(".{l}.")))
            .unwrap_or(usize::MAX);
        (rank, name)
    });
    let text =
        vtt_to_text(&std::fs::read_to_string(&subs[0]).map_err(|e| tool_err("IO", e.to_string()))?);
    if text.is_empty() {
        return Err(tool_err("NO_TRANSCRIPT", "Phụ đề rỗng"));
    }
    Ok(text)
}
