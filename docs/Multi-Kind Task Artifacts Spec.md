# Đặc tả Multi-Kind Task Artifacts & Bộ Công Cụ Đa Phương Tiện cho Subagent

Tài liệu này mô tả kiến trúc, kiểu dữ liệu, API truy vấn/tải xuống và bộ công cụ `HostTool` cho phép các Task trong `orchestrator_flow_rs` (`orchestrator_task_rs`) xuất bản, phân loại và phục vụ **Artifacts đa định dạng** (Báo cáo Markdown, Hình ảnh PNG/SVG, Video MP4, Âm thanh giọng đọc TTS WAV, Tài liệu PDF, và Dữ liệu cấu trúc JSON/CSV) từ cả Root Agent lẫn Subagent con.

---

## 1. Tổng quan Kiến trúc

```
┌────────────────────────────────────────────────────────────────────────────┐
│                         Root Agent (daily_root)                            │
│                    subagent(agent="media_creator" / "report_stager")       │
└───────────────────────────────────────┬────────────────────────────────────┘
                                        │
                                        ▼
┌────────────────────────────────────────────────────────────────────────────┐
│                   Subagent (media_creator / report_stager)                 │
│ ┌─────────────────┐ ┌──────────────────┐ ┌──────────────┐ ┌──────────────┐ │
│ │ markdown_create │ │   image_create   │ │ video_create │ │tts_synthesize│ │
│ │ (text/markdown) │ │ (image/png, svg) │ │ (video/mp4)  │ │ (audio/wav)  │ │
│ └────────┬────────┘ └────────┬─────────┘ └──────┬───────┘ └──────┬───────┘ │
│          │                   │                  │                │         │
│          └───────────────────┴─────────┬────────┴────────────────┘         │
│                                        ▼                                   │
│                  ToolContext::artifacts().put(ArtifactInput)               │
└──────────────────────────────────┬─────────────────────────────────────────┘
                                   │
                                   ▼
┌────────────────────────────────────────────────────────────────────────────┐
│          orchestrator_agent_rs :: ArtifactStore (Atomic Commit)            │
│  staging/<tmp>.part -> SHA-256 + fsync -> rename committed/<key> -> SQLite │
│  Gắn với root_session_id của Task + producer_session_id + invocation_id    │
└──────────────────────────────────┬─────────────────────────────────────────┘
                                   │
                                   ▼
┌────────────────────────────────────────────────────────────────────────────┐
│                 orchestrator_flow_rs :: TaskManager                        │
│  - get_task_detail / get_task_detail_any_scope -> TaskDetailView.artifacts │
│  - list_task_artifacts / list_task_artifacts_any_scope                     │
│  - open_task_artifact / read_task_artifact_bytes_any_scope (SHA-256 check) │
└────────────────────────────────────────────────────────────────────────────┘
```

### Các Bất biến Cốt lõi (Core Invariants)
1. **Quyền sở hữu theo Cây Phiên (`root_session_id`)**: Mọi Artifact do Root Agent hoặc bất kỳ Subagent con nào (`report_stager`, `media_creator`, `source_collector`, ...) tạo ra trong quá trình thực thi Task đều tự động gắn với `root_session_id == task.session_id`.
2. **Xuất bản Nguyên tử & Bất biến**: Dữ liệu được ghi vào `staging/`, tính SHA-256 tăng dần, `sync_all()`, đổi tên nguyên tử sang `committed/`, rồi mới ghi metadata vào bảng `artifacts` trong SQLite.
3. **Kiểm tra Toàn vẹn SHA-256 khi Đọc**: Mọi thao tác mở/đọc qua `TaskManager::open_task_artifact*` hoặc `TaskManager::read_task_artifact_bytes*` đều xác minh:
   - Artifact thuộc đúng `task.session_id` (ngăn chặn truy cập chéo giữa các Task trong cùng Workspace).
   - Kích thước file vật lý và chữ ký SHA-256 khớp tuyệt đối với metadata đã commit.

---

## 2. Phân loại & Kiểu dữ liệu (`src/types/artifact.rs`)

### 2.1. `TaskArtifactKind`
Phân loại ngữ nghĩa của Artifact được suy luận tự động từ `mime_type` và phần mở rộng của `name`:

| Giá trị Enum | Chuỗi JSON | MIME Types / Phần mở rộng tiêu biểu |
| :--- | :--- | :--- |
| `TaskArtifactKind::Report` | `"report"` | `text/markdown`, `text/plain`, `text/html` (`.md`, `.txt`, `.html`) |
| `TaskArtifactKind::Image` | `"image"` | `image/png`, `image/svg+xml`, `image/jpeg`, `image/webp`, `image/gif` |
| `TaskArtifactKind::Video` | `"video"` | `video/mp4`, `video/webm`, `video/quicktime` (`.mp4`, `.webm`, `.mov`, `.mkv`, `.avi`) |
| `TaskArtifactKind::Audio` | `"audio"` | `audio/wav`, `audio/mpeg`, `audio/ogg`, `audio/flac`, `audio/aac` |
| `TaskArtifactKind::Document` | `"document"` | `application/pdf`, `application/epub+zip` (`.pdf`, `.epub`) |
| `TaskArtifactKind::Data` | `"data"` | `application/json`, `text/csv`, `application/xml` (`.json`, `.csv`, `.xml`) |
| `TaskArtifactKind::Other` | `"other"` | Các định dạng nhị phân khác (`application/octet-stream`, ...) |

### 2.2. `TaskArtifactRole`
- `TaskArtifactRole::Primary` (`"primary"`): Sản phẩm bàn giao chính của Task (báo cáo hoàn chỉnh, ảnh minh họa, video clip, file giọng đọc TTS).
- `TaskArtifactRole::Intermediate` (`"intermediate"`): Dữ liệu trung gian hoặc số liệu phụ trợ giữa các bước.

### 2.3. `TaskArtifactView` & `TaskArtifactDownload`
```rust
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
    pub created_at: i64, // unix epoch ms
}

pub struct TaskArtifactDownload {
    pub metadata: TaskArtifactView,
    pub reader: orchestrator_agent_rs::ArtifactReader,
}
```
- `TaskDetailView` bao gồm trường `pub artifacts: Vec<TaskArtifactView>`.
- `TaskExecutionSummary` tự động tổng hợp:
  - `total_artifacts: usize`
  - `primary_artifacts: usize`
  - `report_artifacts: usize`
  - `image_artifacts: usize`
  - `video_artifacts: usize`
  - `audio_artifacts: usize`
  - `artifact_ids: Vec<String>`

---

## 3. Bộ Công cụ `HostTool` Xuất bản Artifacts cho Subagent

### 3.1. `markdown_create` (`MarkdownCreateTool`)
- Lưu file báo cáo vào thư mục `reports/<safe_name>.md` **đồng thời** xuất bản vào `ArtifactStore` với `mime_type = "text/markdown; charset=utf-8"`, `role = Primary`, `kind = Report`.
- Trả về `artifact_id`, `sha256`, `filename`, `path` trong `ToolOutcome.metadata`.

### 3.2. `image_create` (`ImageCreateTool`)
Cho phép Subagent tạo hình ảnh minh họa hoặc sơ đồ kỹ thuật và xuất bản thành `TaskArtifactKind::Image`:
- **Tham số đầu vào**:
  - `filename` (bắt buộc): Tên file ảnh đầu ra (VD: `"b200-banner.png"` hoặc `"nvlink-topology.svg"`).
  - `prompt` (bắt buộc): Mô tả chi tiết hình ảnh hoặc sơ đồ cần tạo.
  - `title` (tùy chọn): Tiêu đề hiển thị trên sơ đồ/thẻ hình ảnh.
  - `format` (tùy chọn): `"png"` hoặc `"svg"` (mặc định suy luận từ phần mở rộng của `filename`).
  - `width`, `height` (tùy chọn): Kích thước ảnh (mặc định `640x360`).
  - `svg_content` (tùy chọn): Mã nguồn `<svg>...</svg>` do Subagent tự thiết kế khi vẽ sơ đồ vector.
  - `role` (tùy chọn): `"primary"` (mặc định) hoặc `"intermediate"`.
- **Khả năng mở rộng Backend (`ImageGeneratorBackend`)**:
  - Mặc định đi kèm `DefaultImageGeneratorBackend` tự chủ (kết xuất ảnh raster PNG chuẩn RFC-2083 với zlib/IDAT + CRC32 và đồ họa vector SVG không cần GPU rời).
  - Host có thể cắm bất kỳ provider tạo ảnh bên ngoài nào (OpenAI Images, ComfyUI, SDXL, Flux) qua `WorkspaceToolKit::with_image_backend(Arc<dyn ImageGeneratorBackend>)`.

### 3.3. `video_create` (`VideoCreateTool`)
Cho phép Subagent tạo video minh họa (`video/mp4`) từ prompt và xuất bản thành `TaskArtifactKind::Video`:
- **Tham số đầu vào**:
  - `filename` (bắt buộc): Tên file video đầu ra (VD: `"b200-flythrough.mp4"`).
  - `prompt` (bắt buộc): Mô tả chi tiết bối cảnh và chuyển động của video cần tạo.
  - `title` (tùy chọn): Tiêu đề hiển thị của video.
  - `width`, `height` (tùy chọn): Độ phân giải video (mặc định `640x360`).
  - `duration_secs` (tùy chọn): Thời lượng video tính bằng giây (`1..=60`, mặc định `4`).
  - `role` (tùy chọn): `"primary"` (mặc định) hoặc `"intermediate"`.
- **Khả năng mở rộng Backend (`VideoGeneratorBackend`)**:
  - Mặc định đi kèm `DefaultVideoGeneratorBackend` tự chủ (đóng gói container ISO BMFF/MP4 `ftypisom` + `mdat` hợp lệ để kiểm thử không cần GPU rời).
  - Host có thể cắm bất kỳ provider tạo video bên ngoài nào qua `WorkspaceToolKit::with_video_backend(Arc<dyn VideoGeneratorBackend>)`.

### 3.4. `tts_synthesize` (`TtsSynthesizeTool`)
Cho phép Subagent chuyển đổi văn bản thành giọng nói (TTS) và xuất bản thành `TaskArtifactKind::Audio`:
- **Tham số đầu vào**:
  - `text` (bắt buộc): Nội dung văn bản cần đọc thành tiếng.
  - `filename` (tùy chọn): Tên file âm thanh đầu ra (mặc định `"tts-output.wav"`).
  - `voice` (tùy chọn): Mã giọng đọc (VD: `"vi-female"`, `"vi-default"`, `"alloy"`).
  - `language` (tùy chọn): Mã ngôn ngữ BCP-47 (mặc định `"vi-VN"`).
  - `speed` (tùy chọn): Tốc độ đọc từ `0.5` đến `2.0` (mặc định `1.0`).
  - `role` (tùy chọn): `"primary"` (mặc định) hoặc `"intermediate"`.
- **Khả năng mở rộng Backend (`TtsSynthesizerBackend`)**:
  - Mặc định đi kèm `DefaultTtsSynthesizerBackend` tạo file âm thanh RIFF/WAVE PCM 16-bit mono (`audio/wav`) hợp lệ.
  - Host có thể cắm engine TTS thực tế (Kokoro, Piper, VietTTS, OpenAI Speech, ElevenLabs) qua `WorkspaceToolKit::with_tts_backend(Arc<dyn TtsSynthesizerBackend>)`.

### 3.5. `artifact_create` (`ArtifactCreateTool`)
Công cụ đa năng cho phép Subagent xuất bản bất kỳ định dạng dữ liệu nào (`application/json`, `text/csv`, `text/html`, `image/svg+xml`, hoặc dữ liệu nhị phân `hex_bytes`) vào `ArtifactStore`.

---

## 4. API Truy vấn & Tải xuống trên `TaskManager`

```rust
// 1. Liệt kê danh sách Artifacts của một Task
let artifacts: Vec<TaskArtifactView> = task_mgr.list_task_artifacts(&task_id).await?;
let admin_artifacts: Vec<TaskArtifactView> = task_mgr.list_task_artifacts_any_scope(&task_id).await?;

// 2. Mở stream đọc Artifact kèm xác minh SHA-256
let download: TaskArtifactDownload = task_mgr.open_task_artifact(&task_id, &artifact_id).await?;
let text: String = download.read_to_string().await?;

// 3. Đọc trực tiếp toàn bộ bytes đã xác minh SHA-256 (phục vụ HTTP download endpoint)
let (meta, bytes): (TaskArtifactView, Vec<u8>) = task_mgr
    .read_task_artifact_bytes_any_scope(&task_id, &artifact_id)
    .await?;
```
