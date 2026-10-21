# orchestrator-task

Thư viện Rust quản lý **Task**, **Followup**, **Schedule/Reminder** và **Wiki Auto-Flow** trên nhiều Flow của hệ thống Orchestrator. Tên package Cargo là `orchestrator-task`; tên crate dùng trong Rust là `orchestrator_task_rs`.

Thư viện được nhúng vào host chạy Tokio; repository hiện không cung cấp chương trình CLI hay server độc lập.

## Chức năng

- Tạo, theo dõi, hủy và truy vấn Task gắn với session của một Flow.
- Xếp hàng Followup và chuyển tiếp khi session sẵn sàng.
- Lập Schedule một lần hoặc theo cron, để nhắc nhở hoặc khởi tạo Task.
- Chuyển tín hiệu Wiki Auto-Flow thành Task được quản lý cùng các Task khác.
- Cung cấp công cụ dưới dạng `HostTool` cho Orchestrator và JSON schema/JSON execution cho Voice Agent.
- Lưu trạng thái trong SQLite riêng (`tasks.db`), kiểm soát quyền sở hữu bằng `tasks.owner.lock`, và reconcile Task khi khởi động lại.
- Phát sự kiện qua subscription để host tích hợp giao diện hoặc luồng cập nhật.

## Chuẩn bị môi trường

Cài Rust qua [rustup](https://rustup.rs/), cùng `rustfmt` và `clippy`. `Cargo.toml` khai báo Rust tối thiểu 1.89; CI dùng stable. Mốc 1.89 chưa được kiểm chứng riêng cho toàn bộ dependency trong lockfile.

Hai dependency nội bộ đang dùng `path`, vì vậy cần bố trí thư mục như sau:

```text
project_rust/
├── orchestrator_task_rs/     # Repository này
├── orchestrator_agent_rs/    # Package orchestrator-agent
└── llm-wiki-rs/              # Package llm-wiki-rs
```

Từ thư mục cha, chuẩn bị hai dependency ở các revision được CI sử dụng:

```sh
git clone https://github.com/hailp-vn38/orchestrator_agent_rs.git orchestrator_agent_rs
git -C orchestrator_agent_rs checkout 2178d207e341fc6e52e60de6854b1fcddc6752ca
git clone https://github.com/hailp-vn38/wiki-llm-rust.git llm-wiki-rs
git -C llm-wiki-rs checkout 3558ed7b6f1f9af58743ece05c04324a7748f68f
cd orchestrator_task_rs
rustup toolchain install stable --profile minimal --component rustfmt --component clippy
cargo +stable check --locked --all-targets
```

Host có thể thêm thư viện bằng dependency cục bộ:

```toml
[dependencies]
orchestrator-task = { path = "../orchestrator_task_rs" }
```

## Tích hợp vào host

Khởi tạo gồm hai pha để lấy `HostTool` trước khi build Orchestrator:

1. Gọi `TaskManager::prepare(config)` để mở store, chạy migration và lấy owner lock.
2. Lấy `reminder_host_tools()` hoặc `all_host_tools()` và đăng ký vào Orchestrator builder của host.
3. Build Orchestrator, gắn các Flow bằng `register_flow()`, rồi gọi `start()`.
4. Dùng `TaskManager` hoặc `voice_tool_suite()` để thao tác; gọi `shutdown().await` khi host dừng.

Ví dụ phần ghép nối, với `daily_orch` do host build sau khi đăng ký tools:

```rust,no_run
use std::sync::Arc;
use orchestrator_agent_rs::Orchestrator;
use orchestrator_task_rs::{TaskConfig, TaskManager, TaskManagerInit, TaskResult};

async fn prepare_tasks() -> TaskResult<TaskManagerInit> {
    let config = TaskConfig::new("./data/tasks")
        .with_default_timezone("Asia/Ho_Chi_Minh")
        .with_default_target("daily", "daily_root");
    TaskManager::prepare(config).await
}

// Host lấy init.all_host_tools(), đăng ký vào builder, rồi build daily_orch.
async fn start_tasks(
    init: TaskManagerInit,
    daily_orch: Arc<Orchestrator>,
) -> TaskResult<Arc<TaskManager>> {
    init.register_flow("daily", daily_orch).start().await
}
```

Flow phải chứa agent có thể làm root (`Primary` hoặc `All`); `daily_root` trong ví dụ phải là ID có thật. Để bật Wiki Auto-Flow, gắn thêm Flow wiki và `WikiAutoFlowBinding` qua `with_wiki_autoflow()` trước `start()`.

Scope mặc định là `owner_id = "personal"`, `workspace_id = "homelab"`; host có thể dùng các API `*_scoped` để chỉ định scope. Mỗi store chỉ được một `TaskManager` sở hữu tại một thời điểm. Dùng thư mục store riêng với store của Orchestrator và Wiki.

Bộ tools gồm `task_create`, `task_followup`, `task_list`, `task_cancel`, `reminder_create`, `reminder_list`, `reminder_complete`, `reminder_delete`. `voice_tool_suite().tool_specs()` trả schema; `execute_raw()` hoặc `execute_raw_scoped()` thực thi lời gọi JSON.

## Kiểm tra cục bộ

Chạy từ thư mục repository:

```sh
cargo +stable fmt --package orchestrator-task -- --check
cargo +stable check --locked --all-targets
cargo +stable clippy --locked --all-targets -- -D warnings
cargo +stable test --locked --all-targets
cargo +stable test --locked --doc
```

Dùng `fmt --package orchestrator-task` để chỉ kiểm tra định dạng package này, tránh kiểm tra cả source của các path dependency. Các lệnh trên không xác nhận tích hợp với provider LLM hoặc host thực tế.

## CI trên GitHub

[Workflow CI](.github/workflows/ci.yml) chạy khi push, mở/cập nhật pull request và khi kích hoạt thủ công. Workflow checkout cả ba repository theo bố trí thư mục ở trên, cài Rust stable, rồi kiểm tra format, build, Clippy, test và doc-test. Các bước phải thành công; lỗi sẽ làm job thất bại.

Hai dependency được ghim SHA trong workflow để cố định đầu vào. Khi nâng dependency, cập nhật SHA, kiểm tra lại `Cargo.lock` và chạy các lệnh kiểm tra cục bộ. Commit `Cargo.lock` cùng source để các lệnh `--locked` chạy được trên runner mới.

Nếu repository dependency là private, cấu hình Actions secret `DEPENDENCIES_READ_TOKEN` có quyền đọc cả hai repository. Với pull request từ fork, GitHub không cấp secret này; checkout dependency private sẽ thất bại. Nếu dependency public, workflow dùng token mặc định. Xem [hướng dẫn checkout nhiều repository](https://github.com/actions/checkout#checkout-multiple-repos-side-by-side) và [cú pháp GitHub Actions](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax).

## Cấu trúc source và tài liệu

| Đường dẫn | Vai trò |
| --- | --- |
| `src/lib.rs` | Public API và các kiểu được re-export |
| `src/manager/` | Vòng đời, API Task, truy vấn và Schedule |
| `src/runtime/` | Dispatch, watcher, scheduler, Auto-Flow và event bus |
| `src/store/` | SQLite và các truy vấn lưu trữ |
| `src/tools/` | JSON tools và adapter HostTool |
| `src/types/` | Các kiểu dữ liệu miền |
| `migrations/` | Schema SQLite, nhúng khi biên dịch |

- [Từ điển miền](GLOSSARY.md)
- [Đặc tả Task và Dual-Flow hợp nhất](docs/Unified%20Task%20And%20Dualflow%20Spec.md)
- [Đặc tả kiến trúc Dual-Flow](docs/Dual%20Flow%20Architecture%20Spec.md)
- [Các quyết định kiến trúc](docs/adr/)
- [Quy ước issue và spec cục bộ](docs/agents/issue-tracker.md)

Các đặc tả mô tả thiết kế; đối chiếu public API trong `src/lib.rs` khi tích hợp.
