//! Example 3: Luồng nạp URL YouTube Playlist bằng `orchestrator_task_rs` (`TaskManager` + `WikiAutoFlowBinding`).
//!
//! Quy trình thực thi tự động qua hệ thống Quản lý Task nền:
//! 1. **Tạo Task Thu thập Playlist (`task_create` -> `wiki_root` -> `source_collector`)**:
//!    - Nhận URL YouTube Playlist, tạo Task nền trên `wiki_root`.
//!    - Subagent `source_collector` gọi `yt_list_playlist` và `yt_fetch_transcript` (qua `yt-dlp`)
//!      để tải phụ đề, phân loại chủ đề `<domain>/<topic>` và lưu Raw Source Markdown vào
//!      `sources/<domain>/<topic>/<slug>.md` kèm tín hiệu `autoflow_notify`.
//! 2. **Tự động kích hoạt Managed Auto-Flow Tasks (`TaskSource::Autoflow` trên `wiki_root`)**:
//!    - Ngay khi Task thu thập hoàn tất, `WikiAutoFlowBinding` trong `TaskManager` tự động kiểm tra
//!      `WikiEngine::build_next_autoflow_prompt(None)` và khởi tạo các `Task` chính thức (`source = Autoflow`)
//!      trong `tasks.db`:
//!      - **Chặng 1 (`Auto-Flow: Biên dịch nguồn mới vào Wiki`)**: điều phối `wiki_ingestor` biên dịch tuần tự
//!        từng file từ `sources/` sang `wiki/sources/`, `wiki/entities/`, `wiki/concepts/` với `[[WikiLinks]]` hai chiều.
//!      - **Chặng 2 (`Auto-Flow: Tổng hợp Synthesis & Kiểm toán Wiki Lint`)**: điều phối `wiki_curator`
//!        tạo trang tổng hợp `wiki/syntheses/` và chạy `wiki_lint` cho đến khi `is_healthy == true`.
//! 3. **Stream Trực tiếp & Xác minh Vault**:
//!    - Toàn bộ tiến độ của Task thu thập và các Task `Autoflow` được stream trực tiếp qua `TaskSubscription`
//!      và lưu lịch sử đầy đủ trong `tasks.db`.
//!
//! Chạy:
//!   cargo run --example flow_youtube_playlist_wiki -- <PLAYLIST_URL> [--limit N] [--lang en,vi] [--model <MODEL>]
//!   (Nếu không truyền `<PLAYLIST_URL>`, mặc định dùng playlist mẫu hoặc biến môi trường `PLAYLIST_URL`)

mod shared;

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use llm_wiki::{TreeOptions, TreeScope};
use orchestrator_task_rs::TaskFilter;
use serde_json::json;

use crate::shared::env::ExampleResult;
use crate::shared::runtime::{HomelabBootOptions, HomelabExampleApp};
use crate::shared::stream::stream_until_complete;

const DEFAULT_PLAYLIST_URL: &str =
    "https://youtube.com/playlist?list=PLeQQbTeiG3sCK5IWsgC873S6JCuVKhHBo&si=xrtoo-Wt6S-0J8lW";

struct CliArgs {
    playlist_url: String,
    boot: HomelabBootOptions,
    timeout_secs: u64,
}

fn parse_cli_args() -> ExampleResult<Option<CliArgs>> {
    let mut url: Option<String> = std::env::var("PLAYLIST_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let mut boot = HomelabBootOptions::default();
    let mut timeout_secs = 4800u64;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lang" | "--env-file" | "--workspace-dir" | "--vault-dir" | "--output-dir"
            | "--limit" | "--model" | "--timeout" => {
                let val = args
                    .next()
                    .ok_or_else(|| io::Error::other(format!("Thiếu giá trị cho {arg}")))?;
                match arg.as_str() {
                    "--lang" => boot.langs = val,
                    "--env-file" => boot.custom_env_file = Some(PathBuf::from(val)),
                    "--limit" => boot.limit = val.parse()?,
                    "--model" => boot.model_override = Some(val),
                    "--timeout" => timeout_secs = val.parse()?,
                    _ => boot.custom_workspace_dir = Some(PathBuf::from(val)),
                }
            }
            "--help" | "-h" => {
                println!(
                    "Cách dùng:\n  \
                     cargo run --example flow_youtube_playlist_wiki -- [PLAYLIST_URL] \
                     [--limit N] [--lang en,vi] [--model <MODEL>] \
                     [--env-file .env.live] [--workspace-dir target/homelab_shared_workspace] [--timeout 4800]"
                );
                return Ok(None);
            }
            s if !s.starts_with('-') && url.is_none() => url = Some(s.to_string()),
            other => {
                return Err(io::Error::other(format!(
                    "Tham số không hợp lệ: {other} (dùng --help)"
                ))
                .into());
            }
        }
    }

    Ok(Some(CliArgs {
        playlist_url: url.unwrap_or_else(|| DEFAULT_PLAYLIST_URL.to_string()),
        boot,
        timeout_secs,
    }))
}

#[tokio::main]
async fn main() -> ExampleResult<()> {
    let Some(cli) = parse_cli_args()? else {
        return Ok(());
    };

    let limit_label = if cli.boot.limit == usize::MAX {
        "all".to_string()
    } else {
        cli.boot.limit.to_string()
    };
    let langs_label = cli.boot.langs.clone();

    let app = HomelabExampleApp::boot_with_options(cli.boot).await?;
    let suite = app.task_mgr.voice_tool_suite();

    println!("\n=== [1. Cấu hình Luồng YouTube Playlist -> Wiki (`orchestrator_task_rs`)] ===");
    println!("  • Playlist URL : {}", cli.playlist_url);
    println!("  • Ngôn ngữ sub : {langs_label}");
    println!("  • Giới hạn     : {limit_label} video");
    println!("  • Target Agents:");
    for entry in suite.available_agents() {
        println!(
            "    - {} (flow='{}'): {}",
            entry.target.agent_id, entry.target.flow_id, entry.description
        );
    }

    // Mở kênh stream sự kiện trực tiếp trước khi tạo Task
    let mut sub = app.task_mgr.subscribe()?;

    println!(
        "\n=== [2. Khởi tạo Task thu thập Playlist trên `wiki_root` (`task_create` Fire-and-Forget)] ==="
    );
    let task_prompt = format!(
        "Hãy lấy thông tin chi tiết trong playlist và lưu vào source theo chủ đề.\nPlaylist URL: {}",
        cli.playlist_url
    );
    let create_resp = suite
        .execute_raw(
            "task_create",
            json!({
                "title": format!("Thu thập YouTube Playlist vào Wiki ({})", cli.playlist_url),
                "prompt": task_prompt,
                "target_agent": "wiki_root"
            }),
        )
        .await?;

    println!(
        "⚡ TaskManager phản hồi tức thì:\n{}",
        serde_json::to_string_pretty(&create_resp)?
    );

    let collector_task_id = create_resp["task_id"]
        .as_str()
        .ok_or("missing task_id")?
        .to_string();

    println!(
        "\n=== [3. LIVE TASK STREAM (`source_collector` -> `sources/` -> Managed `Autoflow` `wiki_ingestor` & `wiki_curator`)] ==="
    );
    let mut pending = HashSet::new();
    pending.insert(collector_task_id);

    // Stream trực tiếp Task thu thập Playlist VÀ toàn bộ các Task `Autoflow` được kích hoạt tự động sau đó!
    let completed = stream_until_complete(
        &mut sub,
        pending,
        true,
        Duration::from_secs(cli.timeout_secs),
    )
    .await?;

    println!("\n=== [4. Danh sách Tasks đã ghi nhận trong `tasks.db`] ===");
    let page = app.task_mgr.list_tasks(&TaskFilter::default()).await?;
    for t in &page.items {
        println!(
            "  - [{}] {:?} | source={:?} | flow={}/{} | title={:?}",
            &t.id[..8],
            t.status,
            t.source,
            t.flow_id,
            t.agent_id,
            t.title
        );
    }

    println!("\n=== [5. Xác minh Vault sau khi Auto-Flow hoàn tất (`WikiEngine`)] ===");
    verify_vault_on_disk(&app)?;
    println!(
        "\n[Hoàn tất {} task] Obsidian Vault: {}",
        completed.len(),
        app.layout.wiki_vault_dir().display()
    );

    app.shutdown().await?;
    Ok(())
}

fn verify_vault_on_disk(app: &HomelabExampleApp) -> ExampleResult<()> {
    let videos = app.yt_env.videos.lock().unwrap().clone();
    if videos.is_empty() {
        return Err(io::Error::other("Subagent chưa gọi yt_list_playlist").into());
    }

    for v in &videos {
        let scoped = v.saved_source_scoped_path.as_deref().ok_or_else(|| {
            io::Error::other(format!(
                "Video `{}` chưa được lưu vào `sources/` theo chủ đề",
                v.title
            ))
        })?;
        let src_file = app.engine.config().sources_dir().join(scoped);
        let wiki_file = app.engine.config().wiki_dir().join("sources").join(scoped);
        if !src_file.exists() {
            return Err(
                io::Error::other(format!("Thiếu Raw Source: {}", src_file.display())).into(),
            );
        }
        if !wiki_file.exists() {
            return Err(io::Error::other(format!(
                "Thiếu trang Wiki của tài liệu nguồn: {}",
                wiki_file.display()
            ))
            .into());
        }
    }

    let _ = app.engine.embed_pending(false)?;
    let _ = app.engine.rebuild_or_sync_index_md()?;
    let lint = app.engine.lint_wiki()?;
    let status = app.engine.status()?;
    let graph = app.engine.get_graph(None)?;

    println!(
        "- Vault Root (Obsidian Vault): {}",
        app.engine.config().data_root.display()
    );
    println!(
        "- Tầng 1 (`sources/`): {} file Raw Source Markdown (phân loại theo chủ đề)",
        status.sources_count
    );
    println!("- Tầng 2 (`wiki/`): {} trang Wiki", status.wiki_count);
    println!(
        "- Obsidian Graph View: {} nodes, {} edges ({} resolved, {} unresolved)",
        graph.nodes.len(),
        graph.edges.len(),
        status.resolved_graph_links,
        status.unresolved_graph_links
    );
    println!(
        "- Wiki Lint Report: is_healthy={}, uningested_sources={}, broken_links={}, orphan_wiki_pages={}, stale_wiki_pages={}",
        lint.is_healthy,
        lint.uningested_sources.len(),
        lint.broken_links.len(),
        lint.orphan_wiki_pages.len(),
        lint.stale_wiki_pages.len()
    );

    let tree = app
        .engine
        .get_tree(TreeOptions::for_scope(TreeScope::All))?;
    println!("\n=== [6. Cây thư mục Knowledge Base (`TreeScope::All`)] ===");
    print!("{}", tree.to_ascii_tree());

    Ok(())
}

#[cfg(test)]
mod tests {
    use llm_wiki::{slugify, vtt_to_text};

    #[test]
    fn test_slugify_and_vtt_cleaning_for_playlist() {
        assert_eq!(
            slugify("Chiến lược DCA của Nick Shawn", 60),
            "chien-luoc-dca-cua-nick-shawn"
        );
        let sample_vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nHello <c>world</c>\n\n00:00:03.000 --> 00:00:05.000\nHello world\nLine 2 &amp; more\n";
        assert_eq!(vtt_to_text(sample_vtt), "Hello world Line 2 & more");
    }
}
