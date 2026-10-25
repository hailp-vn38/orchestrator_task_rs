//! Example 3: Luồng xử lý tài liệu PDF vào LLM Wiki (`PDF -> anydoc -> source_collector -> Managed Auto-Flow`)
//!
//! Dùng chung thư mục dữ liệu với các example khác: `target/homelab_shared_workspace`
//! (và dùng chung 1 DB `stores/orchestrator/orchestrator.db` + `stores/tasks/tasks.db`).
//!
//! Cách chạy:
//!   # 1. Tự tạo file PDF kỹ thuật mẫu và chạy trọn luồng có live stream:
//!   ./scripts/check.sh run --example flow_pdf_to_wiki
//!
//!   # 2. Hoặc truyền đường dẫn tới 1 hoặc nhiều file PDF bất kỳ trên máy:
//!   ./scripts/check.sh run --example flow_pdf_to_wiki -- /path/to/paper.pdf

mod shared;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use llm_wiki::{TreeOptions, TreeScope};
use orchestrator_flow_rs::{CreateTaskRequest, TaskFilter, TaskSource};

use crate::shared::env::ExampleResult;
use crate::shared::pdf::build_sample_technical_pdf;
use crate::shared::runtime::HomelabExampleApp;
use crate::shared::stream::stream_until_complete;

#[tokio::main]
async fn main() -> ExampleResult<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut pdf_paths: Vec<PathBuf> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(|arg| {
            let p = PathBuf::from(&arg);
            if p.exists() {
                return p;
            }
            if let Some(parent) = manifest_dir.parent() {
                let sibling_p = parent.join("llm-wiki-rs").join(&p);
                if sibling_p.exists() {
                    return sibling_p;
                }
            }
            p
        })
        .collect();

    let app = HomelabExampleApp::boot(None, None).await?;

    if pdf_paths.is_empty() {
        let pdf_dir = app.layout.root_dir().join("pdfs");
        std::fs::create_dir_all(&pdf_dir)?;
        let sample_path = pdf_dir.join("hybrid-search-karpathy-wiki.pdf");
        std::fs::write(&sample_path, build_sample_technical_pdf())?;
        println!(
            "\n📄 [Sample PDF] Đã khởi tạo file PDF kỹ thuật mẫu tại: {}",
            sample_path.display()
        );
        pdf_paths.push(sample_path);
    } else {
        println!("\n📄 [Input PDFs] Danh sách file PDF đầu vào:");
        for p in &pdf_paths {
            println!("  • {}", p.display());
        }
    }

    app.pdf_state.set_input_paths(pdf_paths.clone());

    let mut sub = app.task_mgr.subscribe()?;

    let pdf_list_str = pdf_paths
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}. `{}`", i + 1, p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    println!(
        "\n=== [1. Tạo Task xử lý PDF trên `daily_root` (`anydoc` -> `sources/` -> Managed Auto-Flow)] ==="
    );
    let task = app
        .task_mgr
        .create_task(
            CreateTaskRequest::new(
                "Trích xuất PDF bằng anydoc và nạp vào LLM Wiki",
                format!(
                    "Hãy điều phối subagent `source_collector` xử lý các tài liệu PDF sau:\n{pdf_list_str}\n\n\
                     Yêu cầu cho `source_collector`:\n\
                     1. Gọi `anydoc_inspect_pdf` để chuyển đổi PDF sang Markdown và xem tiêu đề, đề mục (outline) cùng bản preview.\n\
                     2. Xác định thư mục `<domain>/<topic>` chuẩn 2 cấp kebab-case (ví dụ `ai-engineering/hybrid-search`) và bộ tags phân cấp.\n\
                     3. Gọi `anydoc_save_to_sources` để lưu bản Markdown vào `sources/<domain>/<topic>/<file_stem>.md` (tự động kích hoạt Managed Auto-Flow).\n\
                     Sau khi `source_collector` lưu xong vào `sources/`, hãy kết thúc lượt chạy này để hệ thống Auto-Flow đảm nhận bước biên dịch tiếp theo."
                ),
            )
            .with_target("daily_root")
            .with_source(TaskSource::Api),
        )
        .await?;

    let mut pending = HashSet::new();
    pending.insert(task.id.clone());

    println!(
        "\n=== [2. LIVE TASK STREAM (Collect PDF -> Auto-Flow Ingest -> Auto-Flow Synthesis & Lint)] ==="
    );
    let completed =
        stream_until_complete(&mut sub, pending, true, Duration::from_secs(1800)).await?;

    println!("\n=== [3. Danh sách Tasks trong `tasks.db` sau khi xử lý PDF] ===");
    let page = app.task_mgr.list_tasks(&TaskFilter::default()).await?;
    for t in &page.items {
        println!(
            "  • [{}] {:?} | source={:?} | {}/{} | {}",
            &t.id[..8],
            t.status,
            t.source,
            t.flow_id,
            t.agent_id,
            t.title
        );
    }

    println!("\n=== [4. Cây thư mục Obsidian Wiki Vault sau khi nạp PDF] ===");
    let tree = app
        .engine
        .get_tree(TreeOptions::for_scope(TreeScope::All))?;
    print!("{}", tree.to_ascii_tree());
    println!("(Đã hoàn tất {} task trong phiên này)", completed.len());

    app.shutdown().await?;
    Ok(())
}
