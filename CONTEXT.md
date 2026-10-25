# Orchestrator Task Context

Miền quản lý Task, Followup, Schedule, Reminder và Auto-Flow trên các Flow của hệ thống trợ lý cá nhân và tri thức Wiki. Các thuật ngữ được tổng hợp từ [đặc tả hợp nhất](docs/Unified%20Task%20And%20Dualflow%20Spec.md), [đặc tả Dual-Flow](docs/Dual%20Flow%20Architecture%20Spec.md) và [từ điển hiện có](GLOSSARY.md); các đánh đổi kiến trúc được ghi riêng trong [ADRs](docs/adr/).

## Language

### Công việc và lịch hẹn

**Task**:
Một đơn vị công việc chạy nền được theo dõi bằng tiêu đề, nguồn kích hoạt, tiến độ và trạng thái, gắn với một root session trong một Flow cụ thể.
_Avoid_: Job, Background Process, Thread, Worker

**Target Agent**:
Một định nghĩa agent có khả năng đảm nhận vai trò root trong một Flow, được chọn làm đích thực thi Task. Mô tả của agent giúp người gọi chọn chuyên môn phù hợp.
_Avoid_: Worker Agent, Bot, Handler

**Followup**:
Chỉ dẫn bổ sung gửi vào một Task hiện có, tiếp tục trên session của Task đó. Chỉ dẫn được thực thi khi session rảnh hoặc xếp hàng theo thứ tự đến khi session đang bận.
_Avoid_: Message, Queued Input, Continuation, Reply

**Schedule**:
Quy tắc thời gian chạy một lần hoặc lặp lại, dùng để phát lời nhắc hoặc khởi tạo Task khi đến hạn.
_Avoid_: Cronjob, Timer, Alarm

**Reminder**:
Một Schedule ở chế độ chỉ thông báo cho người dùng, có mức ưu tiên và có thể kèm các liên kết tài liệu Wiki. Reminder không tự khởi tạo Task để agent thực thi.
_Avoid_: Alert, Todo, Notification

**Scope**:
Cặp định danh chủ sở hữu và không gian làm việc (`owner_id`, `workspace_id`) xác định phạm vi cô lập dữ liệu Task và Schedule.
_Avoid_: Tenant, Account, Namespace

**Task View**:
Giao diện theo dõi các Task từ nhiều Flow, bao gồm công việc do người dùng yêu cầu và công việc được Schedule hoặc Auto-Flow kích hoạt.
_Avoid_: Job Dashboard

### Các luồng trợ lý

**Flow**:
Một Orchestrator độc lập, có tập agent và quyền sử dụng công cụ riêng. Task luôn thuộc một Flow cụ thể.
_Avoid_: Pipeline, Engine, Instance, Cluster

**Dual-Flow**:
Mô hình tách công việc trợ lý hằng ngày và công việc biên soạn tri thức thành hai Flow, cùng sử dụng một kho tri thức Wiki nhưng có quyền ghi khác nhau.

**Daily Flow**:
Flow `daily` (`daily_orch`, Flow 2) phục vụ thảo luận, tra cứu, báo cáo và nhắc nhở hằng ngày. Flow này chỉ đọc Wiki và chuyển nội dung cần bổ sung tri thức qua cổng tiếp nhận nguồn `add_to_sources`.

**Wiki Flow**:
Flow `wiki` (`wiki_orch`, Flow 1) chuyên thu thập nguồn, biên soạn, cập nhật và kiểm toán tri thức Wiki. Đây là Flow có quyền ghi các trang Wiki trong mô hình Dual-Flow.

**Voice Agent**:
Tác nhân nhận yêu cầu bằng giọng nói và sử dụng các công cụ quản lý Task cùng danh mục Target Agent để chuyển công việc tới Flow phù hợp.

### Nguồn tài liệu và tri thức

**Source**:
Tài liệu đầu vào trong vùng `sources/`, có thể đến từ bộ thu thập bên ngoài, bản tổng hợp cuộc thảo luận hoặc file do người dùng thêm trực tiếp. Source là đầu vào cho quá trình biên soạn Wiki.
_Avoid_: Wiki Page

**Wiki Page**:
Trang tri thức trong vùng `wiki/`, được Wiki Flow biên soạn hoặc cập nhật từ các Source; có thể là trang tóm tắt nguồn, thực thể, khái niệm hoặc tổng hợp.
_Avoid_: Source

**WikiLink**:
Liên kết tài liệu theo ký hiệu `[[...]]`, dùng để nối các trang tri thức và gắn tài liệu liên quan vào Task hoặc Reminder.

**Standalone Report**:
Báo cáo Markdown được xuất riêng để phục vụ người dùng. Việc xuất báo cáo không đồng nghĩa với việc đưa nội dung vào Source hoặc biên soạn thành Wiki Page.

**Task Artifact**:
Sản phẩm đầu ra bất biến đa định dạng (`Report`, `Image`, `Audio`/TTS, `Document`, `Data`, `Other`) được xuất bản bởi Root Agent hoặc Subagent thông qua các công cụ (`markdown_create`, `image_create`, `tts_synthesize`, `artifact_create`), lưu trữ trong `ArtifactStore` gắn với session family của Task và được xác minh toàn vẹn bằng SHA-256 khi đọc.

**Auto-Flow**:
Quá trình phát hiện Source chưa được nạp vào Wiki hoặc vấn đề cấu trúc tri thức, rồi khởi tạo Task trên Wiki Flow để xử lý. Mỗi lượt Auto-Flow được quản lý như một Task có thể theo dõi và hủy, theo [ADR-0003](docs/adr/0003-managed-autoflow-as-first-class-tasks.md).
_Avoid_: Sync, Background Loop, Daemon
