# Đặc tả Kiến trúc Hệ thống Đa Agent 2 Luồng (Dual-Flow Architecture)
**Thư viện nền tảng:** [`orchestrator_agent_rs`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/src/lib.rs) + [`llm-wiki-rs`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/src/lib.rs)

> [!IMPORTANT]
> **Quyết định Kiến trúc Cốt lõi (Phương án 1):**
> - Sử dụng **1 instance [`Arc<WikiEngine>`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/src/lib.rs#L140)** duy nhất quản lý Obsidian Vault (`sources/`, `wiki/`, `SCHEMA.md`) và SQLite Index (FTS5 BM25 + Vector + Graph).
> - Khởi tạo **2 luồng (`daily` & `wiki`)** tách biệt hoàn toàn về tập Tool và danh sách Subagent:
>   1. **`daily_orch` / `daily_root` (Flow 1 — Thu thập Thông tin & Trợ lý Đa chức năng):** Tiếp nhận toàn bộ yêu cầu từ người dùng/Voice Agent, sở hữu đầy đủ công cụ thu thập nguồn ngoài (`yt_*`, `anydoc_*`, `fetch_html`/`html_*`), đọc trên `wiki/`, tổng hợp đẩy dữ liệu vào `sources/` (`add_to_sources`), xuất báo cáo rời (`markdown_*`), tạo ảnh/âm thanh TTS (`image_create`, `tts_synthesize`, `artifact_create`), và quản lý lời nhắc (`reminder_*`).
>   2. **`wiki_orch` / `wiki_root` (Flow 2 — Đường ống Tri thức Wiki Tự động):** Chạy ngầm tự động (`WikiAutoFlowBinding`) khi có file mới thêm vào `sources/`, chỉ sở hữu `wiki_ingestor` và `wiki_curator` cùng 14 tool nội bộ `wiki_*` để tạo mới hoặc cập nhật (`patch`/`merge`) các trang trong `wiki/` và kiểm toán `wiki_lint` (hoàn toàn không chứa tool thu thập ngoại vi).

---

## 1. Sơ đồ Kiến trúc Tổng thể

```mermaid
flowchart TB
    subgraph IDE["Giao diện IDE / Voice Agent (User Workspace)"]
        ChatUI["Khung Chat, Thu thập Nguồn & Tác vụ Đa chức năng"]
    end

    subgraph SharedVault["Tầng Dữ liệu Dùng chung: Arc&lt;WikiEngine&gt;"]
        SourcesDir[("sources/&lt;domain&gt;/&lt;topic&gt;/&lt;slug&gt;.md\n(Tầng đệm Nguồn thô & Bản tổng hợp)")]
        WikiDir[("wiki/\n- sources/\n- entities/\n- concepts/\n- syntheses/\n- index.md / log.md")]
        SQLiteDB[("SQLite QMD Index\n(FTS5 + Vector + Graph Links)")]
    end

    subgraph Flow1["FLOW 1: daily_orch (Information Collection & Multi-Functional Assistant)"]
        DRoot["daily_root (Primary)\nTools: [subagent]"]
        SCol["source_collector (Subagent)\nTools: yt_*, anydoc_*, fetch_html, add_to_sources"]
        WRes["wiki_researcher (Subagent)\nRead-Only Wiki + Web HTML"]
        RStager["report_stager (Subagent)\nTools: add_to_sources, markdown_*"]
        MCreator["media_creator (Subagent)\nTools: image_create, tts_synthesize, artifact_create"]
        RPlan["reminder_planner (Subagent)\nTools: reminder_*"]

        DRoot --> SCol
        DRoot --> WRes
        DRoot --> RStager
        DRoot --> MCreator
        DRoot --> RPlan
    end

    subgraph Bridge["Cầu nối Bất đồng bộ (Zero-Blocking Trigger)"]
        Notify(("tokio::sync::Notify\nautoflow_notify"))
    end

    subgraph Flow2["FLOW 2: wiki_orch (Automated Wiki Pipeline on new sources/)"]
        WWorker["Auto-Flow Watcher Loop\nbuild_next_autoflow_prompt()"]
        WRoot["wiki_root (Primary)\nTools: [subagent]"]
        WIng["wiki_ingestor (Subagent)\nĐánh giá index.md -> Create / Patch Wiki"]
        WCur["wiki_curator (Subagent)\nSyntheses + wiki_lint Repair"]

        WWorker --> WRoot
        WRoot --> WIng
        WRoot --> WCur
    end

    ChatUI -->|"Thu thập URL/Playlist/PDF, Hỏi đáp, Báo cáo, Media, Reminder"| DRoot

    WRes -->|"Chỉ đọc (wiki_query, wiki_get)"| SQLiteDB
    SCol -->|"1a. Ghi nguồn ngoài (yt_*, anydoc_*)"| SourcesDir
    SCol -.->|"2a. notify_one()"| Notify
    RStager -->|"1b. Ghi file tổng hợp qua add_to_sources"| SourcesDir
    RStager -.->|"2b. notify_one() (< 1ms)"| Notify
    Notify -.->|"3. Đánh thức Worker"| WWorker

    WWorker -->|"Quét uningested_sources"| SourcesDir
    WIng -->|"Tạo mới / Cập nhật (Patch)"| WikiDir
    WCur -->|"Tổng hợp & Kiểm toán"| WikiDir
    WikiDir <--> SQLiteDB
```

---

## 2. Danh mục Toàn bộ Tools (Built-in, Native & Custom)

### 2.1. Built-in Tools của `orchestrator_agent_rs` ([`src/tool.rs`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/src/tool.rs#L21))

| Tên Tool | Hiệu ứng | Mô tả | Đăng ký ở đâu? |
| :--- | :--- | :--- | :--- |
| `subagent` | Built-in | Uỷ quyền tác vụ cho một subagent con (`agent`, `description`, `prompt`, tùy chọn `session_id`). | `wiki_orch` & `daily_orch` (chỉ cấp cho `wiki_root` và `daily_root`) |
| `skill` | Built-in | Tải nội dung hướng dẫn từ file `SKILL.md` theo `id`. | `wiki_orch` & `daily_orch` (cấp cho các subagent) |
| `artifact_read` | Built-in | Đọc dữ liệu artifact theo chunk/cursor. | Runtime tự cấp khi cần |

---

### 2.2. Native Wiki Tools từ `llm-wiki-rs` ([`src/tools/definitions.rs`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/src/tools/definitions.rs#L27-L430))

Được bọc qua `WikiEngineBridgeTool` và phân bổ nghiêm ngặt giữa 2 `Orchestrator`:

| Tên Tool | Hiệu ứng | Mô tả chức năng | `daily_orch` (Flow 1) | `wiki_orch` (Flow 2) |
| :--- | :--- | :--- | :---: | :---: |
| `wiki_tree` | `ReadOnly` | Lấy cây thư mục `sources`, `wiki`, hoặc `all` (JSON + ASCII). | Có | Có |
| `wiki_query` | `ReadOnly` | Tìm kiếm QMD Hybrid Search (`lex` BM25 + `vec` + `hyde` + RRF + Rerank + Metadata filter). | Có | Có |
| `wiki_get` | `ReadOnly` | Đọc nội dung 1 file theo đường dẫn hoặc `#docid`, hỗ trợ đọc phân trang `from_line`/`max_lines`. | Có | Có |
| `wiki_multi_get` | `ReadOnly` | Đọc hàng loạt file theo glob pattern hoặc danh sách đường dẫn/`#docid`. | Có | Có |
| `wiki_metadata` | `ReadOnly` | Khám phá các khóa/giá trị YAML frontmatter để dựng bộ lọc tìm kiếm. | Có | Có |
| `wiki_status` | `ReadOnly` | Kiểm tra sức khỏe index, số lượng tài liệu và thống kê liên kết đồ thị. | Có | Có |
| `wiki_graph_neighbors` | `ReadOnly` | Xem `backlinks` và `outgoing_links` (`[[WikiLinks]]`) của một trang hoặc toàn bộ đồ thị. | Có | Có |
| `wiki_log` | `Mutating` | Đọc (`mode="read"`) hoặc ghi (`mode="append"`) nhật ký `wiki/log.md`. | Chỉ đọc (hoặc ẩn) | Có |
| `wiki_ingest_source` | `Mutating` | Biên dịch file từ `sources/<path>.md` sang `wiki/sources/<path>.md` (hoặc cập nhật `target_wiki_path`). | **Không** | **Có** (`wiki_ingestor`) |
| `wiki_write_file` | `Mutating` | Tạo mới hoặc ghi đè file Markdown trong `wiki/` hoặc `sources/`. | **Không** | **Có** |
| `wiki_patch_file` | `Mutating` | Sửa đoạn văn bản (`target_text` $\rightarrow$ `replacement_text`) hoặc nối thêm (`append_text`) vào file Wiki hiện có. | **Không** | **Có** |
| `wiki_file_synthesis` | `Mutating` | Lưu bài tổng hợp liên nguồn vào `wiki/syntheses/` và cập nhật `index.md`. | **Không** *(đi qua `add_to_sources`)* | **Có** (`wiki_curator`) |
| `wiki_lint` | `Mutating` | Kiểm toán toàn bộ Vault: tìm `uningested_sources`, `broken_links`, `orphan_wiki_pages`, `unlinked_mentions`. | **Không** | **Có** (`wiki_curator`) |
| `wiki_delete_file` | `Mutating` | Xóa file trùng lặp/lỗi thời và tự dọn thư mục rỗng. | **Không** | **Có** (`wiki_curator`) |
| `wiki_schema` | `Mutating` | Đọc hoặc cập nhật quy ước `SCHEMA.md`. | **Không** | **Có** (`wiki_curator`) |

---

### 2.3. Custom Bridge Tool (Cầu nối Flow 1 $\rightarrow$ Flow 2): `add_to_sources`

Đây là công cụ ở **Flow 1** dùng để lưu bản tổng hợp thảo luận/nghiên cứu vào `sources/` nhằm kích hoạt đường ống Wiki ở **Flow 2**:
- **Nơi đăng ký:** Đăng ký trong `daily_orch` và cấp cho `report_stager` (và `source_collector`).
- **Cơ chế hoạt động & Cổng bảo vệ (học từ Javis OS Self-Learning):**
  1. **Provenance Gate (`provenance`):** Hỗ trợ `"user"`, `"source"`, `"verified_synthesis"` (mặc định) và `"assistant"`. Chặn cứng (`PROVENANCE_BLOCKED`) nếu `provenance == "assistant"` để ngăn AI tự suy diễn rồi hợp thức hóa thành nguồn tri thức chuẩn.
  2. **Secret & Prompt-Injection Gate:** Quét tĩnh `title` và `markdown_body` (0 token) để từ chối khóa bí mật/token hoặc chỉ thị prompt injection (`CONTENT_POLICY_BLOCKED`) và kiểm tra độ dài thực chất tối thiểu (`>= 20` ký tự, `INSUFFICIENT_SUBSTANCE`).
  3. **Conflict & Open Questions Metadata:** Tự động ghi `provenance`, `origin: "daily-flow-staged"`, `target_hint`, `conflict_with` vào YAML frontmatter và nối mục `## Mâu thuẫn & Câu hỏi mở` (`> [!WARNING] Contradiction / Caveat`) khi có `conflict_with` hoặc `open_questions`.
  4. Gọi [`WikiEngine::write_topic_source`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/src/lib.rs#L258-L298) để tạo file Markdown chuẩn tại `sources/<domain>/<topic>/<file_stem>.md`.
  5. Gọi `autoflow_notify.notify_one()` để đánh thức Background Worker của **Flow 2** (có bảo vệ bằng `AutoflowVaultSignature` chống lặp vô tận khi Vault không đổi).
  6. Trả về đường dẫn `source_rel_path` và `#docid` cho Flow 1 để báo cáo lại cho người dùng.

#### Đặc tả JSON Schema của `add_to_sources`:
```json
{
  "type": "object",
  "required": ["topic_folder", "title", "markdown_body"],
  "properties": {
    "topic_folder": {
      "type": "string",
      "description": "Thư mục chủ đề 2 cấp trong sources/ theo chuẩn kebab-case (<domain>/<topic>, ví dụ: 'software-architecture/agent-workflows', 'algo-trading/risk-management')."
    },
    "file_stem": {
      "type": "string",
      "description": "Tên file kebab-case không gồm đuôi .md (tùy chọn, nếu bỏ trống sẽ tự sinh từ title)."
    },
    "title": {
      "type": "string",
      "description": "Tiêu đề rõ ràng của bản tổng hợp thảo luận / tài liệu."
    },
    "markdown_body": {
      "type": "string",
      "description": "Toàn bộ nội dung Markdown đã được tổng hợp chi tiết từ cuộc thảo luận (>= 20 ký tự thực chất, không chứa secret hay prompt injection)."
    },
    "tags": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Danh sách tag phân cấp kebab-case (ví dụ: ['topic/software-architecture/agent-workflows', 'rust', 'orchestrator'])."
    },
    "provenance": {
      "type": "string",
      "enum": ["user", "source", "verified_synthesis", "assistant"],
      "description": "Nguồn gốc tri thức: 'user', 'source', 'verified_synthesis' (mặc định), hoặc 'assistant' (bị chặn nếu chỉ là suy đoán chưa xác minh của AI)."
    },
    "target_hint": {
      "type": "string",
      "description": "Gợi ý tùy chọn cho Flow 2 (wiki_ingestor) về các trang Wiki hiện có liên quan mật thiết cần được cập nhật/bổ sung."
    },
    "conflict_with": {
      "type": "string",
      "description": "Trang Wiki hiện có chứa nội dung mâu thuẫn với bản tổng hợp này (để Flow 2 bảo toàn cả hai quan điểm thay vì ghi đè)."
    },
    "open_questions": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Danh sách các câu hỏi mở hoặc giả thuyết chưa được xác minh cần ghi nhận riêng."
    }
  },
  "additionalProperties": false
}
```

---

### 2.4. Custom Tools cho Flow 1 (Báo cáo rời, Đa phương tiện & Nhắc nhở Hằng ngày)

#### A. Nhóm Tool Xuất Báo cáo Rời (tái sử dụng từ [`examples/video_summary.rs`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/examples/video_summary.rs#L246-L411))
Lưu báo cáo ra thư mục làm việc hằng ngày (ví dụ `workspace/reports/`) mà không làm bẩn `sources/` hay `wiki/`:
1. `markdown_create` (`Mutating`): Tạo file báo cáo Markdown mới (`filename`, `title`, `source_url`, `summary`, `key_points`).
2. `markdown_append_section` (`Mutating`): Nối thêm mục `## heading` + `body` vào báo cáo.
3. `markdown_read` (`ReadOnly`): Đọc lại file báo cáo để xác minh nội dung sau khi ghi.

#### B. Nhóm Tool Quản lý Nhắc nhở & Task Hằng ngày (`ReminderTools` — xây dựng mới cho `reminder_planner`)
Lưu trữ vào bảng SQLite riêng của IDE (hoặc file `reminders.json`):

| Tên Tool | Hiệu ứng | JSON Schema Arguments (`properties`) | Mô tả |
| :--- | :--- | :--- | :--- |
| `reminder_create` | `Mutating` | `title` *(req)*, `due_at` *(req, ISO-8601)*, `priority` (`"low"\|"medium"\|"high"`), `notes`, `related_wiki_links` (`string[]`), `repeat_cron` | Tạo thông báo nhắc nhở mới có gắn kèm các link `[[WikiLinks]]` liên quan. |
| `reminder_list` | `ReadOnly` | `status` (`"pending"\|"completed"\|"all"`), `from_date`, `to_date`, `limit` | Liệt kê danh sách các nhắc nhở sắp tới hoặc đang chờ xử lý. |
| `reminder_complete` | `Mutating` | `reminder_id` *(req)*, `resolution_note` | Đánh dấu hoàn thành một nhắc nhở. |
| `reminder_delete` | `Mutating` | `reminder_id` *(req)* | Xóa một nhắc nhở đã hủy. |

---

### 2.5. Custom Tools Thu thập Nguồn ngoài (External Collectors — thuộc Flow 1 `source_collector`)

Kế thừa nguyên bản từ các `examples/` đã có và cấp cho Flow 1 (`daily_orch` / `source_collector`):
1. **Nhóm Web HTML ([`orchestrator_agent_rs/examples/url_summary/tools.rs`](file:///Users/lamphuchai/Desktop/project_rust/orchestrator_agent_rs/examples/url_summary/tools.rs)):**
   - `fetch_html`, `html_extract_metadata`, `html_extract_article`, `html_extract_feed`, `html_extract_links`, `html_select_elements` (`ReadOnly` — cấp cho cả `source_collector` và `wiki_researcher` của Flow 1).
2. **Nhóm YouTube ([`llm-wiki-rs/examples/youtube_playlist_wiki/tools.rs`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/examples/youtube_playlist_wiki/tools.rs)):**
   - `yt_list_playlist` (`ReadOnly`), `yt_fetch_transcript` (`Mutating` — lưu vào `sources/` và kích hoạt Flow 2). Chỉ cấp cho `source_collector` (Flow 1).
3. **Nhóm PDF / Office `anydoc` ([`llm-wiki-rs/examples/pdf_anydoc_wiki/tools.rs`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/examples/pdf_anydoc_wiki/tools.rs)):**
   - `anydoc_inspect_pdf` (`ReadOnly`), `anydoc_save_to_sources` (`Mutating` — lưu vào `sources/` và kích hoạt Flow 2). Chỉ cấp cho `source_collector` (Flow 1).

---

## 3. Thiết kế Hệ thống Skills (`SKILL.md`)

Hệ thống sử dụng **6 Skills** đặt trong thư mục `skills/` (4 skills đã có sẵn trong [`llm-wiki-rs/skills/`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/skills) và 2 skills dành cho Flow 1).

### 3.1. 4 Skills Hiện có (Giữ nguyên)
1. **[`wiki-taxonomy`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/skills/wiki-taxonomy/SKILL.md)**: Quy tắc phân tầng `<domain>/<topic>`, ngưỡng tạo trang rời (`Page-worthy`), quy tắc chống phân mảnh (Anti-Fragmentation), và chuẩn bộ tag 3 tầng (`wiki/*`, `topic/<domain>/<topic>`, `<keyword>`).
2. **[`wiki-ingest`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/skills/wiki-ingest/SKILL.md)**: Quy trình 4 bước biên dịch file từ `sources/` sang `wiki/sources/`, tạo mới hoặc hợp nhất (`merge`/`patch`) `wiki/entities/` & `wiki/concepts/`, và nối liên kết hai chiều.
3. **[`wiki-query`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/skills/wiki-query/SKILL.md)**: Chiến lược truy vấn QMD Hybrid Search (`lex` + `vec` + `hyde`), lần theo đồ thị `wiki_graph_neighbors` và chế độ trích dẫn chuẩn (`Standard`, `Smart`, `Faithful`).
4. **[`wiki-lint`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/skills/wiki-lint/SKILL.md)**: Quy trình kiểm toán cấu trúc & ngữ nghĩa, sửa `broken_links`, `orphan_wiki_pages`, `unlinked_mentions` và gộp các trang trùng lặp.

---

### 3.2. 2 Skills Dành cho Flow 1

#### Skill 5: `skills/daily-synthesis-staging/SKILL.md` (Dành cho `report_stager`)
```markdown
---
name: daily-synthesis-staging
description: "Workflow for synthesizing a user chat/discussion into a structured Markdown document and either staging it into `sources/<domain>/<topic>/` via `add_to_sources` (for Wiki ingestion) or exporting a standalone report."
---

# Daily Discussion Synthesis & Wiki Staging Skill (`daily-synthesis-staging`)

Use this skill when the user asks to summarize/synthesize a conversation, research thread, or daily report, especially when they ask to **"add/save it to the Wiki"**.

## 1. Distinguish Target Destination
1. **Case A — User wants to add/update knowledge in the Wiki**:
   - First call `wiki_tree(scope="sources")` and `wiki_get(file="wiki/index.md")` to inspect existing `<domain>/<topic>` folders and existing canonical entity/concept names.
   - Reuse an existing `<domain>/<topic>` folder whenever the discussion fits an established theme; otherwise define a clean `kebab-case` `<domain>/<topic>` path (max 2 levels).
   - Call `add_to_sources` with a comprehensive, self-contained Markdown synthesis.
2. **Case B — User wants a standalone Daily/Weekly Report file**:
   - Call `markdown_create` to write the report file, then call `markdown_read` to verify.

## 2. Structure of `markdown_body` When Calling `add_to_sources`
Because `Flow 2 (wiki_ingestor)` will read this file from `sources/` without seeing your prior chat history, the `markdown_body` MUST be 100% self-contained:
1. **Context & Objective**: What problem, question, or topic was discussed and why.
2. **Core Decisions, Rules & Technical Findings**: Preserve all concrete parameters, formulas, code snippets, architectural choices, and comparison tables verbatim.
3. **Explicit Links to Existing Wiki Subjects**: Mention existing Wiki entities/concepts by their canonical names and note whether this synthesis **extends**, **updates**, or **contradicts** prior pages.
4. **Open Questions / Caveats**: Any unresolved risks or edge cases.
```

#### Skill 6: `skills/daily-task-reminder/SKILL.md` (Dành cho `reminder_planner`)
```markdown
---
name: daily-task-reminder
description: "Workflow for extracting actionable tasks, deadlines, recurring schedules, and linked Wiki references to create/manage user reminders."
---

# Daily Task & Reminder Planning Skill (`daily-task-reminder`)

Use this skill when creating, listing, or updating reminders and action items for the user.

## Rules
1. **Check Existing Reminders First**: Before creating a new reminder, call `reminder_list(status="pending")` to avoid creating duplicate reminders for the same task.
2. **Normalize Time & Recurrence**:
   - Resolve relative time expressions (e.g. "9h sáng mai", "thứ Hai tuần sau") against the current local timestamp provided in the prompt into strict ISO-8601 format (`YYYY-MM-DDTHH:MM:SS+07:00`).
3. **Attach Wiki Context (`related_wiki_links`)**:
   - If the reminder is about reviewing, implementing, or following up on a topic in the Wiki, include the exact `[[wiki/sources/...]]`, `[[concepts/...]]`, or `[[syntheses/...]]` links in `related_wiki_links` so the user can click straight into the document when the notification fires.
```

---

## 4. Cấu hình Chi tiết & System Prompts cho 9 Agents

### Bảng Tổng hợp Cấu hình `AgentDefinition`

| Flow | Agent ID | `AgentMode` | `allowed_tools` | `allowed_skills` | `steps` | `temp` | `max_output_tokens` |
| :--- | :--- | :--- | :--- | :--- | :---: | :---: | :---: |
| **Flow 1** | **`daily_root`** | `Primary` | `["subagent"]` | `[]` | `20` | `0.2` | `8192` |
| **Flow 1** | **`source_collector`** | `Subagent` | `["skill", "anydoc_inspect_pdf", "anydoc_save_to_sources", "yt_list_playlist", "yt_fetch_transcript", "fetch_html", "html_extract_metadata", "html_extract_article", "html_extract_feed", "html_extract_links", "html_select_elements", "add_to_sources", "wiki_tree"]` | `["wiki-taxonomy"]` | `35` | `0.1` | `8192` |
| **Flow 1** | **`wiki_researcher`** | `Subagent` | `["skill", "wiki_query", "wiki_get", "wiki_multi_get", "wiki_tree", "wiki_metadata", "wiki_graph_neighbors", "wiki_status", "fetch_html", "html_extract_metadata", "html_extract_article", "html_extract_feed", "html_extract_links", "html_select_elements"]` | `["wiki-query"]` | `20` | `0.1` | `8192` |
| **Flow 1** | **`report_stager`** | `Subagent` | `["skill", "wiki_tree", "wiki_get", "add_to_sources", "markdown_create", "markdown_read"]` | `["wiki-taxonomy", "daily-synthesis-staging"]` | `15` | `0.2` | `16384` |
| **Flow 1** | **`media_creator`** | `Subagent` | `["image_create", "tts_synthesize", "artifact_create"]` | `[]` | `12` | `0.2` | `8192` |
| **Flow 1** | **`reminder_planner`** | `Subagent` | `["skill", "reminder_create", "reminder_list", "reminder_complete", "reminder_delete"]` | `["daily-task-reminder"]` | `10` | `0.1` | `4096` |
| **Flow 2** | **`wiki_root`** | `Primary` | `["subagent"]` | `[]` | `40` | `0.1` | `4096` |
| **Flow 2** | **`wiki_ingestor`** | `Subagent` | `["skill", "wiki_tree", "wiki_get", "wiki_multi_get", "wiki_query", "wiki_metadata", "wiki_ingest_source", "wiki_write_file", "wiki_patch_file"]` | `["wiki-taxonomy", "wiki-ingest"]` | `35` | `0.2` | `16384` |
| **Flow 2** | **`wiki_curator`** | `Subagent` | `["skill", "wiki_tree", "wiki_get", "wiki_multi_get", "wiki_query", "wiki_file_synthesis", "wiki_write_file", "wiki_patch_file", "wiki_delete_file", "wiki_lint", "wiki_schema"]` | `["wiki-taxonomy", "wiki-query", "wiki-lint"]` | `35` | `0.2` | `16384` |

---

### 4.1. System Prompts cho Flow 1 (`daily_orch` / `daily_root`)

Các hằng số chuẩn được xuất từ [`src/prompts.rs`](../src/prompts.rs):
- `DAILY_ROOT_PROMPT` (`prompts/daily_root.md`)
- `SOURCE_COLLECTOR_PROMPT` (`prompts/source_collector.md`)
- `WIKI_RESEARCHER_PROMPT` (`prompts/wiki_researcher.md`)
- `REPORT_STAGER_PROMPT` (`prompts/report_stager.md`)
- `MEDIA_CREATOR_PROMPT` (`prompts/media_creator.md`)
- `REMINDER_PLANNER_PROMPT` (`prompts/reminder_planner.md`)

---

### 4.2. System Prompts cho Flow 2 (`wiki_orch` / `wiki_root`)

Sử dụng bộ 3 Prompts chuẩn cho đường ống biên dịch/kiểm toán Wiki tự động:
1. **`wiki_root`**: Dùng `WIKI_ROOT_PROMPT` — chỉ gọi tuần tự từng file qua `wiki_ingestor`, sau đó gọi `wiki_curator` (hoàn toàn không thu thập nguồn ngoài).
2. **`wiki_ingestor`**: Dùng [`WIKI_INGESTOR_PROMPT`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/src/prompts/subagents.rs#L65-L107) — đánh giá file mới trong `sources/<source_rel_path>` đối chiếu với `wiki/index.md`:
   - **Nếu file mới là bản cập nhật trực tiếp cho một trang nguồn đã có**: truyền `target_wiki_path` vào `wiki_ingest_source` để cập nhật trang đó và giữ nguyên lịch sử `sources:` trong frontmatter.
   - **Nếu file mới là một bài độc lập**: tạo `wiki/sources/<source_rel_path>`, đồng thời với mỗi thực thể/khái niệm liên quan:
     - Chưa có trong `wiki/index.md` $\rightarrow$ gọi `wiki_write_file` tạo trang mới tại `wiki/entities/<domain>/<slug>.md` hoặc `wiki/concepts/<domain>/<slug>.md`.
     - Đã có trong `wiki/index.md` $\rightarrow$ gọi `wiki_patch_file` (`append_text` hoặc thay thế) để **cập nhật bổ sung kiến thức mới** vào trang cũ và nối `[[WikiLinks]]` hai chiều.
3. **`wiki_curator`**: Dùng [`WIKI_CURATOR_PROMPT`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/src/prompts/subagents.rs#L110-L136) — tạo/cập nhật bài tổng hợp `wiki/syntheses/<domain>/<topic>-synthesis.md` và chạy vòng lặp sửa lỗi `wiki_lint` đến khi `is_healthy: true`.

---

## 5. Luồng Thực thi Chi tiết cho 6 Trường hợp Sử dụng (Use Cases)

### Trường hợp 1: Đang Chat & Thảo luận ở Flow 1 $\rightarrow$ Yêu cầu *"Tổng hợp và thêm vào Wiki"* (Luồng Trọng tâm)

```mermaid
flowchart LR
    A["1. User chốt thảo luận:\n'Tổng hợp & thêm vào Wiki'"] --> B["2. daily_root giao\nreport_stager"]
    B --> C["3. report_stager kiểm tra\nwiki_tree & index.md"]
    C --> D["4. Gọi add_to_sources\nGhi sources/<domain>/<topic>/<slug>.md"]
    D --> E["5. Bắn autoflow_notify\nTrả lời User ngay lập tức"]
    E --> F["6. Flow 2 thức dậy:\nbuild_next_autoflow_prompt()"]
    F --> G["7. wiki_ingestor:\nĐánh giá Create vs Patch\ntrên wiki/ hiện có"]
    G --> H["8. wiki_curator:\nCập nhật syntheses/ &\nchạy wiki_lint == true"]
```

1. **Bước 1 (Tại `daily_orch` — Flow 1):** Bạn và `daily_root` trao đổi qua nhiều lượt. Khi bạn nhắn: *"Hãy tổng hợp toàn bộ nội dung vừa thảo luận và thêm vào Wiki"*.
2. **Bước 2 (Đóng gói vào `sources/`):**
   - `daily_root` gom toàn bộ các kết luận trong phiên chat và gọi `subagent(agent="report_stager")`.
   - `report_stager` gọi `wiki_tree(scope="sources")` và `wiki_get(file="wiki/index.md")` để xem trong Vault đã có `<domain>/<topic>` nào phù hợp chưa.
   - `report_stager` gọi `add_to_sources(topic_folder="<domain>/<topic>", title="...", markdown_body="...", tags=[...])`.
   - `AddToSourcesTool` lưu file vào `sources/<domain>/<topic>/<slug>.md`, đánh chỉ mục SQLite cho file nguồn đó, và gọi `autoflow_notify.notify_one()`.
   - `daily_root` trả lời bạn ngay trên khung chat: *"Đã lưu bản tổng hợp vào `sources/<domain>/<topic>/<slug>.md` (docid `#...`). Tiến trình cập nhật Wiki (Flow 2) đang chạy ngầm."*
3. **Bước 3 (Kích hoạt tự động Flow 2 tại `wiki_orch`):**
   - Background Worker của Flow 2 nhận tín hiệu `autoflow_notify`, gọi [`engine.build_next_autoflow_prompt(None)`](file:///Users/lamphuchai/Desktop/project_rust/llm-wiki-rs/src/lib.rs#L948-L1010).
   - Hàm này phát hiện file mới trong `lint.uningested_sources` và sinh prompt `[AUTO-FLOW: NEW SOURCES DETECTED]` kèm **Compact Wiki Index** ưu tiên các trang cùng `<domain>/<topic>`.
   - Worker tạo một Task `Autoflow` cho `wiki_root` và submit prompt.
4. **Bước 4 (Đánh giá, Tạo mới & Cập nhật Wiki hiện có):**
   - `wiki_root` gọi `wiki_ingestor` cho file `sources/<domain>/<topic>/<slug>.md`.
   - `wiki_ingestor` đọc file mới cùng `wiki/index.md`:
     - Tạo trang tóm tắt nguồn `wiki/sources/<domain>/<topic>/<slug>.md`.
     - Với các trang `wiki/entities/<domain>/...` và `wiki/concepts/<domain>/...` **đã tồn tại**: gọi `wiki_patch_file` để bổ sung phát hiện mới.
     - Với các thực thể/khái niệm **chưa tồn tại**: gọi `wiki_write_file` tạo trang mới.
   - Tiếp theo, `wiki_root` gọi `wiki_curator` để cập nhật trang `wiki/syntheses/<domain>/<topic>-synthesis.md` và chạy `wiki_lint` đảm bảo 0 liên kết đứt, 0 trang mồ côi.
   - Worker gọi `engine.embed_pending(false)` để cập nhật vector embeddings cho toàn bộ chunk mới.

---

### Trường hợp 2: Tra cứu & Hỏi đáp chuyên sâu trên Wiki (Flow 1)
1. Người dùng hỏi câu hỏi kỹ thuật trên khung Chat.
2. `daily_root` gọi `subagent(agent="wiki_researcher", prompt="...")`.
3. `wiki_researcher` gọi `wiki_query` (kết hợp `lex` BM25 + `vec` Semantic + RRF) và `wiki_graph_neighbors` để lần theo các liên kết `[[WikiLinks]]`, sau đó gọi `wiki_get` đọc chi tiết các đoạn cần thiết.
4. `wiki_researcher` trả kết quả kèm trích dẫn `[[...]]` về cho `daily_root` hiển thị cho người dùng.

---

### Trường hợp 3: Tạo Báo cáo Định kỳ kết hợp Wiki + Tin tức Web mới (Flow 1)
1. Người dùng yêu cầu: *"Đối chiếu kiến thức trong Wiki về chủ đề X với bài báo tại URL Y và xuất file báo cáo `bao-cao-tuan.md`"*.
2. `daily_root` gọi `wiki_researcher`:
   - `wiki_researcher` vừa gọi `wiki_query`/`wiki_get` lấy kiến thức nền trong Wiki, vừa gọi `fetch_html` + `html_extract_article` để lấy nội dung từ URL Y.
3. Sau khi nhận bản tổng hợp từ `wiki_researcher`, `daily_root` gọi `report_stager` để gọi `markdown_create(filename="bao-cao-tuan.md", ...)` và `markdown_read` kiểm chứng.

---

### Trường hợp 4: Tạo Thông báo Nhắc nhở gắn với Tài liệu Wiki (Flow 1)
1. Người dùng yêu cầu: *"Nhắc tôi 9h sáng thứ Hai tuần tới kiểm tra lại các rủi ro trong bài tổng hợp chiến lược DCA"*.
2. `daily_root` gọi `wiki_researcher` (nếu chưa biết đường dẫn chính xác) để tìm trang `[[wiki/syntheses/algo-trading/mql5-dca-synthesis]]` và các điểm rủi ro chính.
3. `daily_root` gọi `reminder_planner` truyền tiêu đề, thời gian mục tiêu, ghi chú tóm tắt rủi ro và link `[[wiki/syntheses/algo-trading/mql5-dca-synthesis]]`.
4. `reminder_planner` kiểm tra `reminder_list(status="pending")` rồi gọi `reminder_create` và trả về xác nhận.

---

### Trường hợp 5: Nạp Trực tiếp Nguồn Tài liệu Lớn từ Bên ngoài (YouTube Playlist / PDF `anydoc` / Web) ở Flow 1 $\rightarrow$ Kích hoạt Tự động Flow 2
1. Người dùng cung cấp đường dẫn PDF hoặc URL YouTube Playlist cho `daily_root` (Flow 1).
2. `daily_root` gọi `source_collector`:
   - Nếu là PDF: gọi `anydoc_inspect_pdf` $\rightarrow$ phân loại `<domain>/<topic>` $\rightarrow$ gọi `anydoc_save_to_sources`.
   - Nếu là Playlist: gọi `yt_list_playlist` $\rightarrow$ phân loại `<domain>/<topic>` $\rightarrow$ gọi `yt_fetch_transcript` cho từng video.
3. Ngay khi các file được lưu vào `sources/`, vòng lặp `build_next_autoflow_prompt` của Flow 2 (`wiki_root`) tự động mở Task `Autoflow` cho từng chặng để gọi `wiki_ingestor` (tuần tự từng file) và `wiki_curator` (`wiki_lint`).

---

### Trường hợp 6: Người dùng Tự thêm/sửa File Markdown trong thư mục `sources/` trên Đĩa
1. Người dùng tạo hoặc copy một file `.md` trực tiếp vào `sources/<domain>/<topic>/` trong IDE.
2. IDE gọi `engine.sync_index()` và `autoflow_notify.notify_one()`.
3. Flow 2 (`wiki_root`) tự động phát hiện file mới qua `lint.uningested_sources` trong `build_next_autoflow_prompt(None)` và chạy đúng quy trình của `wiki_ingestor` $\rightarrow$ `wiki_curator` mà không cần bất kỳ thao tác thủ công nào.

---

## 6. Mẫu Khởi tạo & Ghép nối 2 `Orchestrator` bằng `WorkspaceToolKit`

```rust
use std::sync::Arc;
use tokio::sync::Notify;
use llm_wiki::WikiEngine;
use llm_wiki::prompts::WikiTemplateId;
use orchestrator_agent_rs::{Orchestrator, OrchestratorConfig};
use orchestrator_flow_rs::{
    TaskConfig, TaskManager, WikiAutoFlowBinding, WorkspaceLayout, WorkspaceToolKit,
};

pub async fn init_dual_flow(
    layout: &WorkspaceLayout,
    daily_cfg: OrchestratorConfig,
    wiki_cfg: OrchestratorConfig,
) -> Result<Arc<TaskManager>, Box<dyn std::error::Error>> {
    let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir())?);
    engine.apply_template(WikiTemplateId::Research)?;
    let autoflow_notify = Arc::new(Notify::new());

    let task_init = TaskManager::prepare(
        TaskConfig::from_layout(layout).with_default_target("daily", "daily_root"),
    )
    .await?;

    let toolkit = WorkspaceToolKit::from_layout(layout, engine.clone(), autoflow_notify.clone());

    // FLOW 1 (`daily_orch`): Thu thập ngoại vi + Đọc Wiki + Staging + Báo cáo + Media + Reminder
    let mut daily_builder = Orchestrator::builder(daily_cfg);
    for tool in toolkit.daily_flow_host_tools(&task_init) {
        daily_builder = daily_builder.register_tool(tool);
    }
    let daily_orch = Arc::new(daily_builder.build().await?);

    // FLOW 2 (`wiki_orch`): Chỉ gồm 14 tool `wiki_*` cho đường ống tự động
    let mut wiki_builder = Orchestrator::builder(wiki_cfg);
    for tool in toolkit.wiki_flow_host_tools() {
        wiki_builder = wiki_builder.register_tool(tool);
    }
    let wiki_orch = Arc::new(wiki_builder.build().await?);

    let task_mgr = task_init
        .register_flow("daily", daily_orch)
        .register_flow("wiki", wiki_orch)
        .with_wiki_autoflow(WikiAutoFlowBinding::new(
            engine,
            autoflow_notify,
            "wiki",
            "wiki_root",
        ))
        .start()
        .await?;

    Ok(task_mgr)
}
```
