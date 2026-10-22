//! Example 1: Luồng Voice Agent tạo Task nền trên Flow 2 (`daily_root`) -> `report_stager`
//! gọi `add_to_sources` -> kích hoạt tự động Managed Auto-Flow Task trên Flow 1 (`wiki_root`).
//!
//! Dùng chung thư mục dữ liệu: `target/homelab_shared_workspace`
//!
//! Chạy:
//!   cargo run --example flow_voice_and_autoflow

mod shared;

use std::collections::HashSet;
use std::time::Duration;

use llm_wiki::{TreeOptions, TreeScope};
use orchestrator_task_rs::TaskFilter;
use serde_json::json;

use crate::shared::env::ExampleResult;
use crate::shared::runtime::HomelabExampleApp;
use crate::shared::stream::stream_until_complete;

#[tokio::main]
async fn main() -> ExampleResult<()> {
    let app = HomelabExampleApp::boot(None, None).await?;
    let suite = app.task_mgr.voice_tool_suite();

    println!("\n=== [1. Danh mục Target Agent động cấp cho Voice Agent] ===");
    for entry in suite.available_agents() {
        println!(
            "  • {} (flow='{}'): {}",
            entry.target.agent_id, entry.target.flow_id, entry.description
        );
    }

    // Mở kênh stream sự kiện cho UI View Task trước khi kích hoạt
    let mut sub = app.task_mgr.subscribe()?;

    println!("\n=== [2. Voice Agent gọi `task_create` (Fire-and-Forget < 5ms)] ===");
    let voice_resp = suite
        .execute_raw(
            "task_create",
            json!({
                "title": "Tổng hợp kiến trúc Homelab TaskManager vào Wiki",
                "prompt": "Hãy giao cho subagent `report_stager` gọi `add_to_sources` một lần với:\n\
                           - `topic_folder`: \"software-architecture/homelab-task-manager\"\n\
                           - `file_stem`: \"task-manager-dualflow\"\n\
                           - `title`: \"Kiến trúc TaskManager và Dual-Flow trên Homelab\"\n\
                           - `tags`: [\"topic/software-architecture/homelab-task-manager\", \"rust\", \"sqlite\"]\n\
                           - `markdown_body`: bài tổng hợp kỹ thuật tiếng Việt (khoảng 35 dòng) trình bày kiến trúc 2 pha khởi tạo (`prepare`/`start`), `tasks.db` riêng biệt, cầu nối `add_to_sources` -> `autoflow_notify` và bộ lập lịch `task_schedules`.",
                "target_agent": "daily_root"
            }),
        )
        .await?;

    println!(
        "⚡ Voice Agent nhận phản hồi tức thì:\n{}",
        serde_json::to_string_pretty(&voice_resp)?
    );

    let first_task_id = voice_resp["task_id"]
        .as_str()
        .ok_or("missing task_id")?
        .to_string();

    println!(
        "\n=== [3. LIVE TASK STREAM (Flow 2 `daily_root` -> `add_to_sources` -> Flow 1 `Autoflow`)] ==="
    );
    let mut pending = HashSet::new();
    pending.insert(first_task_id);

    // Stream trực tiếp cả Task của Voice Agent lẫn Task `Autoflow` được kích hoạt tự động sau đó!
    let completed =
        stream_until_complete(&mut sub, pending, true, Duration::from_secs(360)).await?;

    println!("\n=== [4. Danh sách Tasks trên UI View Task (`tasks.db`)] ===");
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

    println!("\n=== [5. Cây thư mục Obsidian Wiki Vault sau khi Auto-Flow hoàn tất] ===");
    let tree = app
        .engine
        .get_tree(TreeOptions::for_scope(TreeScope::All))?;
    print!("{}", tree.to_ascii_tree());
    println!("(Đã hoàn tất {} task trong phiên này)", completed.len());

    app.shutdown().await?;
    Ok(())
}
