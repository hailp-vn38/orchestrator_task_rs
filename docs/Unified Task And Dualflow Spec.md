# Đặc tả Kiến trúc Hợp nhất: `orchestrator_flow_rs` + Dual-Flow (`wiki_orch` & `daily_orch`) + Voice Agent

**Tài liệu kết hợp từ:**
- Phiên thiết kế Dual-Flow: [dual-flow-agent-architecture.md](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/docs/dual-flow-agent-architecture.md) ([d571f046](conversation://d571f046-40f4-491d-a91c-a4304d7cc129))
- Phiên thiết kế Background Task Manager cho Homelab Server (phiên hiện tại)
- Áp dụng chuẩn thiết kế: [`codebase-design`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/.agents/skills/codebase-design/SKILL.md) (Deep Modules) & [`domain-modeling`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/.agents/skills/domain-modeling/SKILL.md) (`GLOSSARY.md` + `docs/adr/`)

---

## 1. Thiết kế `TaskConfig` & Khởi tạo 2 Pha (Giải bài toán Phụ thuộc Vòng)

### 1.1. Bài toán Phụ thuộc Vòng lúc Khởi tạo (Chicken-and-Egg Problem)
Trong kiến trúc Dual-Flow:
- `daily_orch` cần đăng ký các `HostTool` (`reminder_*`, `task_*`) **trước** khi gọi [`OrchestratorBuilder::build()`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/src/orchestrator/builder.rs#L95) (vì `build()` kiểm tra mọi tool trong `AgentDefinition::allowed_tools` phải tồn tại).
- Ngược lại, `TaskManager` lại cần nhận `Arc<Orchestrator>` (`daily_orch` và `wiki_orch`) **sau** khi chúng đã `build()` xong để điều phối session và trích xuất mô tả Agent cho Voice Agent.

### 1.2. Giải pháp: Khởi tạo 2 Pha (`TaskManager::prepare` $\rightarrow$ `TaskManagerInit::start`)
Tách quá trình khởi tạo thành 2 pha rõ ràng với cấu hình thuần dữ liệu (`TaskConfig` + `TaskRuntimeConfig`), tuân thủ đúng triết lý của [`OrchestratorConfig`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/src/config.rs#L14-L27):

```rust
/// Cấu hình tĩnh của `orchestrator_flow_rs` (validate cục bộ, không chứa runtime handle).
#[derive(Debug, Clone)]
pub struct TaskConfig {
    /// Thư mục chứa `tasks.db` và `tasks.owner.lock` (tách biệt với store_dir của Orchestrator).
    pub store_dir: PathBuf,
    /// Scope mặc định khi caller không truyền (mặc định: owner="personal", workspace="homelab").
    pub default_scope: TaskScope,
    /// Múi giờ mặc định để tính lịch Cron và chuẩn hóa due_at (mặc định: "Asia/Ho_Chi_Minh").
    pub default_timezone: String,
    /// Cặp (flow_id, agent_id) mặc định khi Voice/API gọi `task_create` mà bỏ trống target_agent.
    pub default_target: Option<TargetAgent>,
    /// Các tham số vận hành (timeout, giới hạn dung lượng, chu kỳ quét).
    pub runtime: TaskRuntimeConfig,
}

#[derive(Debug, Clone)]
pub struct TaskRuntimeConfig {
    /// Chu kỳ tối đa của vòng lặp Scheduler khi không có lịch gần hơn (mặc định: 15s).
    pub scheduler_tick: Duration,
    /// Chu kỳ dự phòng quét Auto-Flow (ngoài tín hiệu `autoflow_notify` tức thì, mặc định: 60s).
    pub autoflow_poll_interval: Duration,
    /// Sức chứa kênh broadcast `TaskEvent` cho UI View Task (mặc định: 1024).
    pub event_capacity: usize,
    /// Số lượng item tối đa mỗi trang khi truy vấn danh sách task/schedule (mặc định: 50).
    pub page_size: u32,
    /// Giới hạn ký tự cho `output_preview` lưu trong bảng `tasks` (mặc định: 2000).
    pub output_preview_chars: usize,
    /// Thời gian chờ các watcher/worker dừng hẳn khi gọi `TaskManager::shutdown()` (mặc định: 15s).
    pub shutdown_grace: Duration,
}
```

#### Luồng ghép nối chuẩn trong Host Server:
```rust
// PHA 1: Prepare (Validate config, lấy OwnerLock trên store_dir, mở/migrate tasks.db)
let task_init = TaskManager::prepare(task_config).await?;

// Lấy bộ HostTool (reminder_create, reminder_list, reminder_complete, reminder_delete, ...)
// để đăng ký vào `daily_orch` TRƯỚC khi build `daily_orch`.
let mut daily_builder = Orchestrator::builder(daily_cfg);
for tool in task_init.reminder_host_tools() {
    daily_builder = daily_builder.register_tool(tool);
}
let daily_orch = Arc::new(daily_builder.build().await?);
let wiki_orch = Arc::new(wiki_builder.build().await?);

// PHA 2: Gắn các Orchestrator + WikiEngine Auto-Flow, reconcile các task bị gián đoạn và chạy nền
let task_mgr = task_init
    .register_flow("daily", daily_orch)
    .register_flow("wiki", wiki_orch)
    .with_wiki_autoflow(WikiAutoFlowBinding {
        engine: engine.clone(),
        notify: autoflow_notify.clone(),
        flow_id: "wiki".into(),
        agent_id: "wiki_root".into(),
    })
    .start()
    .await?;

// Lấy bộ Tool cho Voice Agent (lúc này đã có đầy đủ mô tả động của `daily_root`, `wiki_root`!)
let voice_tools = task_mgr.voice_tool_suite();
```

---

## 2. Thiết kế Deep Module & Phân tách File (`codebase-design`)

Tuân thủ nguyên tắc:
1. **Không viết file tập trung quá lớn:** Mỗi file chỉ đảm nhận một trách nhiệm rõ ràng.
2. **Tập trung 100% truy vấn SQLite vào `src/store/`:** Không có bất kỳ câu lệnh SQL (`sqlx::query`) nào nằm ngoài `src/store/`. Mọi write transaction bắt đầu bằng câu lệnh ghi (`BEGIN IMMEDIATE` thông qua write statement đầu tiên) giống [`SessionStore`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/src/store/mod.rs#L1-L5) để tránh `SQLITE_BUSY_SNAPSHOT` trong chế độ WAL.
3. **Deep Module (`TaskManager`):** Giao diện bên ngoài nhỏ gọn; ẩn toàn bộ logic phức tạp (subscribe/snapshot watcher, hàng đợi followup khi session bận, bộ lập lịch cron/timer, đồng bộ trạng thái lúc khởi động và cầu nối Auto-Flow) ở bên trong.

```text
/Users/lamphuchai/Desktop/project_rust/orchestrator_task_rs/
├── Cargo.toml
├── GLOSSARY.md                        # Từ điển miền (Domain Modeling, không chứa chi tiết cài đặt)
├── docs/
│   └── adr/
│       ├── 0001-separate-task-sqlite-store.md
│       ├── 0002-two-phase-initialization-for-host-tools.md
│       ├── 0003-managed-autoflow-as-first-class-tasks.md
│       └── 0004-unified-schedule-and-reminder-model.md
├── migrations/
│   └── 0001_init.sql                  # Schema cho tasks, task_followups, task_schedules
└── src/
    ├── lib.rs                         # Public surface (chỉ re-export các type thuộc Interface)
    ├── config.rs                      # TaskConfig, TaskRuntimeConfig, TargetAgent & validation
    ├── error.rs                       # TaskError, ErrorCode, ConfigReason
    ├── owner_lock.rs                  # File lock đảm bảo 1 process sở hữu tasks.db
    ├── types/                         # Chia nhỏ các Value Types của public interface
    │   ├── mod.rs
    │   ├── scope.rs                   # TaskScope (default owner="personal", workspace="homelab")
    │   ├── task.rs                    # TaskView, TaskStatus, TaskSource, CreateTaskRequest, TaskFilter
    │   ├── schedule.rs                # ScheduleView, ScheduleAction, ScheduleTrigger, ReminderFilter
    │   └── event.rs                   # TaskEvent, TaskSubscription (cho SSE/WS của UI View Task)
    ├── store/                         # TẬP TRUNG 100% SQLITE QUERIES & TRANSACTIONS
    │   ├── mod.rs                     # TaskStore pool lifecycle, WAL pragma, migration
    │   ├── codec.rs                   # Chuyển đổi row <-> domain types, JSON serialization
    │   ├── task_ops.rs                # Insert/update status, progress, preview, terminal state
    │   ├── task_query.rs              # List/filter/get tasks có phân trang cho UI & Voice
    │   ├── followup_ops.rs            # Enqueue followup, claim next pending followup
    │   ├── schedule_ops.rs            # Create/update/complete/delete schedule & reminder, claim due
    │   ├── schedule_query.rs          # List/filter schedules & reminders
    │   └── recovery_ops.rs            # Truy vấn các task đang 'running'/'queued' lúc startup để reconcile
    ├── manager/                       # Deep Module Implementation: TaskManager & TaskManagerInit
    │   ├── mod.rs                     # Struct TaskManager, TaskManagerInit
    │   ├── prepare.rs                 # Pha 1 (prepare) & Pha 2 (start + startup recovery)
    │   ├── dispatch.rs                # create_task, send_followup, cancel_task, retry_task
    │   ├── schedule_api.rs            # CRUD lịch hẹn & reminder
    │   ├── query_api.rs               # Read API cho UI View Task (list_tasks, get_task_detail, ...)
    │   └── lifecycle.rs               # Graceful shutdown nhiều pha (dừng worker -> đóng store -> nhả lock)
    ├── runtime/                       # Các Background Workers nội bộ (ẩn hoàn toàn sau TaskManager)
    │   ├── mod.rs
    │   ├── flow_registry.rs           # Quản lý map flow_id -> Arc<Orchestrator> & Agent Catalog động
    │   ├── watcher.rs                 # Lắng nghe Orchestrator::subscribe -> cập nhật DB + auto-followup
    │   ├── scheduler.rs               # Vòng lặp Timer/Cron -> bắn Notification hoặc gọi dispatch
    │   ├── autoflow.rs                # Lắng nghe autoflow_notify -> gọi WikiEngine -> tạo Auto-Flow Task
    │   └── event_bus.rs               # Broadcast TaskEvent (TaskCreated, ProgressUpdated, ReminderDue...)
    └── tools/                         # Dual-Mode Adapter: vừa Raw JSON (Voice) vừa impl HostTool (Flow 2)
        ├── mod.rs                     # TaskToolSuite & ToolSpec
        ├── catalog.rs                 # Sinh JSON Schema động nhúng mô tả Agent từ FlowRegistry
        ├── voice_handler.rs           # execute_raw() cho Voice Agent (task_create, task_list, task_cancel...)
        └── host_adapter.rs            # Struct impl HostTool cho reminder_create/list/complete/delete & task_*
```

---

## 3. Từ điển Miền (`GLOSSARY.md`) & Các Quyết định Kiến trúc (`docs/adr/`)

Theo đúng skill [`domain-modeling`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/.agents/skills/domain-modeling/SKILL.md):

### 3.1. Nội dung `GLOSSARY.md`
- **Task**: Một đơn vị công việc chạy nền có tiêu đề, nguồn kích hoạt, trạng thái hiển thị trên UI View Task và gắn với một Root Session của một Flow.
  _Avoid_: Job, Background Process, Thread, Worker
- **Flow**: Một instance `Orchestrator` độc lập với bộ quyền công cụ và tập Agent riêng biệt (ví dụ: `daily` cho tương tác hằng ngày và `wiki` cho đường ống tri thức).
  _Avoid_: Pipeline, Engine, Instance, Cluster
- **Followup**: Một yêu cầu bổ sung do người dùng gửi thêm vào một Task; được thực thi ngay nếu Session đang rảnh hoặc xếp hàng chờ nếu Session đang bận.
  _Avoid_: Message, Queued Input, Continuation, Reply
- **Schedule**: Một quy tắc thời gian (một lần tại `due_at` hoặc lặp lại theo `cron_expr`) dùng để phát thông báo nhắc nhở hoặc tự động khởi tạo một Task mới khi đến hạn.
  _Avoid_: Cronjob, Timer, Alarm
- **Reminder**: Một Schedule ở chế độ chỉ thông báo (`notify_only`), gắn kèm mức độ ưu tiên và danh sách liên kết tài liệu Wiki (`[[WikiLinks]]`).
  _Avoid_: Alert, Todo, Notification
- **Auto-Flow**: Quá trình tự động phát hiện nguồn tài liệu mới hoặc lỗi cấu trúc trong `WikiEngine` để khởi tạo một Task biên dịch và kiểm toán trên Flow `wiki`.
  _Avoid_: Sync, Background Loop, Daemon
- **Scope**: Cặp định danh `(owner_id, workspace_id)` cô lập quyền truy cập dữ liệu của Task và Schedule (mặc định là `("personal", "homelab")`).
  _Avoid_: Tenant, Account, Namespace

### 3.2. Danh mục 4 ADRs (`docs/adr/`)
Chỉ ghi lại các quyết định có đánh đổi thực sự, khó đảo ngược và cần ngữ cảnh rõ ràng:
1. **`0001-separate-task-sqlite-store.md`**: Tại sao tách `tasks.db` riêng khỏi `orchestrator.db` và `wiki.db`? (Tránh xung đột migration và write-lock contention với WAL của `Orchestrator`, đồng thời cho phép một `tasks.db` điều phối nhiều instance `Orchestrator` độc lập mà vẫn truy vấn danh sách Task cho UI trong 1 query).
2. **`0002-two-phase-initialization-for-host-tools.md`**: Tại sao tách khởi tạo thành `TaskManager::prepare` và `TaskManagerInit::start`? (Giải quyết phụ thuộc vòng khi `daily_orch` cần đăng ký và validate `HostTool` lúc `build()`, trước khi `Arc<Orchestrator>` tồn tại để cấp cho `TaskManager`).
3. **`0003-managed-autoflow-as-first-class-tasks.md`**: Tại sao biến vòng lặp ngầm `autoflow_notify` của Flow 1 thành các bản ghi `Task` chính thức trong `tasks.db`? (Đảm bảo mọi hoạt động gọi LLM tốn chi phí và thay đổi `wiki/` đều hiển thị minh bạch trên UI View Task, có thể theo dõi tiến độ subagent, hủy bỏ và tự động phục hồi sau khi khởi động lại).
4. **`0004-unified-schedule-and-reminder-model.md`**: Tại sao hợp nhất `ReminderTools` của Flow 2 và bộ lập lịch `Scheduler` vào chung bảng `task_schedules` với phân loại `notify_only` vs `spawn_task`? (Tránh duy trì hai vòng lặp timer và hai bảng SQLite có cùng cấu trúc thời gian/cron/scope; cho phép nâng cấp một lời nhắc nhở thành tác vụ tự động chạy Agent mà không đổi schema).
