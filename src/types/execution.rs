//! Value types có `Serialize`/`Deserialize` biểu diễn chi tiết thực thi của Root Agent
//! và các Subagent con bên trong một Task (ánh xạ từ `FamilySnapshot` của `Orchestrator`).

use orchestrator_agent_rs::{
    FamilySnapshot, InvocationSnapshot, InvocationStatus, MessageRole, MessageToolCall,
    MessageView, RunSnapshot, RunStatus, SessionSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::artifact::{TaskArtifactKind, TaskArtifactRole, TaskArtifactView};

/// Tổng hợp số liệu thực thi của toàn bộ cây Agent (Root + các Subagent) trong một Task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskExecutionSummary {
    /// Tổng số phiên agent tham gia trong Task (`1` Root + `subagent_count`).
    pub total_agents: usize,
    /// Số lượng phiên Subagent con đã được khởi tạo trong Task.
    pub subagent_count: usize,
    /// Danh sách `agent_id` hiện đang ở trạng thái `running`.
    pub active_agents: Vec<String>,
    /// Tổng số bước suy luận LLM (`steps`) cộng dồn từ Root và mọi Subagent.
    pub total_steps: u32,
    /// Tổng số input tokens cộng dồn từ Root và mọi Subagent.
    pub total_input_tokens: u64,
    /// Tổng số output tokens cộng dồn từ Root và mọi Subagent.
    pub total_output_tokens: u64,
    /// Tổng số tokens (`total_input_tokens + total_output_tokens`).
    pub total_tokens: u64,
    /// Tổng số lệnh gọi tool (`invocations`) của Root và mọi Subagent.
    pub total_tool_calls: usize,
    /// Số lượng lệnh gọi tool đã hoàn tất thành công (`status == "completed"`).
    pub succeeded_tool_calls: usize,
    /// Số lượng lệnh gọi tool thất bại hoặc bị chặn bởi cổng kiểm duyệt (`status == "failed"`).
    pub failed_tool_calls: usize,
    /// Danh sách các mã lỗi (`error_code`) đã phát sinh từ các lệnh gọi tool thất bại (khử trùng lặp).
    pub failed_error_codes: Vec<String>,
    /// Danh sách đường dẫn `Source` (`sources/...`) đã được ghi thành công trong Task.
    pub sources_written: Vec<String>,
    /// Danh sách đường dẫn `Wiki Page` (`wiki/...`) đã được tạo hoặc cập nhật thành công trong Task.
    pub wiki_pages_written: Vec<String>,
    /// Tổng số Task Artifacts đã xuất bản trong Task (gồm cả `primary` và `intermediate`).
    pub total_artifacts: usize,
    /// Số lượng Task Artifacts có vai trò `primary`.
    pub primary_artifacts: usize,
    /// Số lượng Task Artifacts thuộc phân loại `report` (`text/markdown`, `text/plain`, `text/html`).
    pub report_artifacts: usize,
    /// Số lượng Task Artifacts thuộc phân loại `image` (`image/*`).
    pub image_artifacts: usize,
    /// Số lượng Task Artifacts thuộc phân loại `video` (`video/*`).
    pub video_artifacts: usize,
    /// Số lượng Task Artifacts thuộc phân loại `audio` / TTS (`audio/*`).
    pub audio_artifacts: usize,
    /// Danh sách các `artifact_id` đã xuất bản trong Task (khử trùng lặp).
    pub artifact_ids: Vec<String>,
    /// Kết quả `is_healthy` của lần kiểm toán `wiki_lint` gần nhất trong Task (nếu có).
    pub last_lint_healthy: Option<bool>,
    /// `true` nếu danh sách Subagent con vượt trần snapshot của `Orchestrator`.
    pub children_truncated: bool,
}

impl TaskExecutionSummary {
    pub fn from_family_snapshot(snap: &FamilySnapshot) -> Self {
        let subagent_count = snap.children.len();
        let mut summary = Self {
            total_agents: 1 + subagent_count,
            subagent_count,
            children_truncated: snap.children_truncated,
            ..Self::default()
        };

        for session in snap.sessions() {
            summary.total_tool_calls = summary
                .total_tool_calls
                .saturating_add(session.invocations.len());
            for inv in &session.invocations {
                summary.record_invocation(inv);
            }
            if let Some(ref run) = session.latest_run {
                if run.status == RunStatus::Running
                    && !summary.active_agents.contains(&session.agent_id)
                {
                    summary.active_agents.push(session.agent_id.clone());
                }
                summary.total_steps = summary.total_steps.saturating_add(run.steps);
                summary.total_input_tokens =
                    summary.total_input_tokens.saturating_add(run.input_tokens);
                summary.total_output_tokens = summary
                    .total_output_tokens
                    .saturating_add(run.output_tokens);
            }
        }

        summary.total_tokens = summary
            .total_input_tokens
            .saturating_add(summary.total_output_tokens);
        summary
    }

    fn record_invocation(&mut self, inv: &InvocationSnapshot) {
        match inv.status {
            InvocationStatus::Completed => {
                self.succeeded_tool_calls = self.succeeded_tool_calls.saturating_add(1);
                if let Some(meta) = inv.result.as_ref().and_then(|r| r.get("metadata")) {
                    self.record_completed_metadata(meta);
                }
            }
            InvocationStatus::Failed => {
                self.failed_tool_calls = self.failed_tool_calls.saturating_add(1);
                if let Some(ref code) = inv.error_code {
                    push_unique(&mut self.failed_error_codes, code);
                }
            }
            _ => {}
        }
    }

    fn record_completed_metadata(&mut self, meta: &Value) {
        if let Some(vault_rel) = meta.get("vault_rel_path").and_then(Value::as_str) {
            self.record_vault_path(vault_rel);
        }

        if let Some(wiki_rel) = meta.get("wiki_rel_path").and_then(Value::as_str) {
            push_unique(&mut self.wiki_pages_written, wiki_rel);
        }

        let is_mutating = meta
            .get("mutating")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || meta.get("content_hash").is_some();
        let is_deleted = meta
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_mutating && !is_deleted {
            if let Some(rel) = meta.get("rel_path").and_then(Value::as_str) {
                self.record_vault_path(rel);
            }
        }

        if let Some(healthy) = meta.get("is_healthy").and_then(Value::as_bool) {
            self.last_lint_healthy = Some(healthy);
        }

        if let Some(art_id) = meta.get("artifact_id").and_then(Value::as_str) {
            push_unique(&mut self.artifact_ids, art_id);
        }
        if let Some(arr) = meta.get("artifact_ids").and_then(Value::as_array) {
            for item in arr {
                if let Some(id_str) = item.as_str() {
                    push_unique(&mut self.artifact_ids, id_str);
                }
            }
        }
    }

    /// Cập nhật thống kê artifacts từ danh sách `TaskArtifactView` đã commit trong `ArtifactStore`.
    pub fn apply_artifacts(&mut self, artifacts: &[TaskArtifactView]) {
        self.total_artifacts = artifacts.len();
        self.primary_artifacts = 0;
        self.report_artifacts = 0;
        self.image_artifacts = 0;
        self.video_artifacts = 0;
        self.audio_artifacts = 0;
        for art in artifacts {
            push_unique(&mut self.artifact_ids, &art.id);
            if art.role == TaskArtifactRole::Primary {
                self.primary_artifacts = self.primary_artifacts.saturating_add(1);
            }
            match art.kind {
                TaskArtifactKind::Report => {
                    self.report_artifacts = self.report_artifacts.saturating_add(1);
                }
                TaskArtifactKind::Image => {
                    self.image_artifacts = self.image_artifacts.saturating_add(1);
                }
                TaskArtifactKind::Video => {
                    self.video_artifacts = self.video_artifacts.saturating_add(1);
                }
                TaskArtifactKind::Audio => {
                    self.audio_artifacts = self.audio_artifacts.saturating_add(1);
                }
                _ => {}
            }
        }
    }

    fn record_vault_path(&mut self, path: &str) {
        if path.starts_with("sources/") {
            push_unique(&mut self.sources_written, path);
        } else if path.starts_with("wiki/") {
            push_unique(&mut self.wiki_pages_written, path);
        }
    }
}

fn push_unique(list: &mut Vec<String>, item: &str) {
    let trimmed = item.trim();
    if !trimmed.is_empty() && !list.iter().any(|existing| existing == trimmed) {
        list.push(trimmed.to_string());
    }
}

/// Chi tiết phiên thực thi của một Agent (Root hoặc Subagent) trong Task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAgentSessionView {
    pub session_id: String,
    /// `None` nếu là Root Agent của Task; `Some(parent_session_id)` nếu là Subagent con.
    pub parent_session_id: Option<String>,
    pub agent_id: String,
    pub is_root: bool,
    pub archived: bool,
    pub state_revision: i64,
    pub latest_run: Option<TaskAgentRunView>,
    pub invocations: Vec<TaskToolInvocationView>,
    pub recent_messages: Vec<TaskAgentMessageView>,
}

impl TaskAgentSessionView {
    pub fn from_session_snapshot(snap: &SessionSnapshot) -> Self {
        Self {
            session_id: snap.session_id.clone(),
            parent_session_id: snap.parent_session_id.clone(),
            agent_id: snap.agent_id.clone(),
            is_root: snap.parent_session_id.is_none(),
            archived: snap.archived,
            state_revision: snap.state_revision,
            latest_run: snap
                .latest_run
                .as_ref()
                .map(TaskAgentRunView::from_run_snapshot),
            invocations: snap
                .invocations
                .iter()
                .map(TaskToolInvocationView::from_invocation_snapshot)
                .collect(),
            recent_messages: snap
                .recent_messages
                .iter()
                .map(TaskAgentMessageView::from_message_view)
                .collect(),
        }
    }

    /// Trả về nội dung `text` (đã kiểm tra khác rỗng) của message `assistant` đính kèm `tool_call_id`.
    pub fn invocation_assistant_text(&self, tool_call_id: &str) -> Option<&str> {
        self.recent_messages
            .iter()
            .find(|m| {
                m.role == "assistant"
                    && !m.text.trim().is_empty()
                    && m.tool_calls.iter().any(|tc| tc.call_id == tool_call_id)
            })
            .map(|m| m.text.as_str())
    }

    /// Trả về nội dung `text` của message `assistant` kết thúc lượt chạy `completed` mới nhất
    /// (không có `tool_calls` và khớp `run_id` của `latest_run` theo chuẩn `docs/17` §17.6).
    pub fn final_assistant_text(&self) -> Option<&str> {
        let run = self.latest_run.as_ref()?;
        if run.status != "completed" {
            return None;
        }
        self.recent_messages
            .iter()
            .rev()
            .find(|m| {
                m.role == "assistant"
                    && m.tool_calls.is_empty()
                    && !m.text.trim().is_empty()
                    && m.run_id.as_deref() == Some(run.run_id.as_str())
            })
            .map(|m| m.text.as_str())
    }
}

/// Số liệu và trạng thái của lượt chạy (run attempt) mới nhất trên một Agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAgentRunView {
    pub run_id: String,
    pub turn_id: String,
    pub attempt: u32,
    /// `"running"`, `"completed"`, `"failed"`, `"cancelled"`, `"timed_out"`, `"interrupted"`.
    pub status: String,
    pub steps: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

impl TaskAgentRunView {
    pub fn from_run_snapshot(run: &RunSnapshot) -> Self {
        Self {
            run_id: run.run_id.clone(),
            turn_id: run.turn_id.clone(),
            attempt: run.attempt,
            status: run.status.as_str().to_string(),
            steps: run.steps,
            input_tokens: run.input_tokens,
            output_tokens: run.output_tokens,
            total_tokens: run.total_tokens(),
            error_code: run.error.as_ref().map(|e| e.code.clone()),
            error_message: run.error.as_ref().map(|e| e.message.clone()),
        }
    }
}

/// Chi tiết một lần gọi Tool (`InvocationSnapshot`) của Root Agent hoặc Subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskToolInvocationView {
    pub invocation_id: String,
    pub run_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    /// `"requested"`, `"running"`, `"completed"`, `"failed"`, `"cancelled"`, `"interrupted"`.
    pub status: String,
    pub result: Option<Value>,
    /// Nếu `tool_name == "subagent"`, chứa `session_id` của Subagent con được tạo/tái sử dụng.
    pub child_session_id: Option<String>,
    pub error_code: Option<String>,
}

impl TaskToolInvocationView {
    pub fn from_invocation_snapshot(inv: &InvocationSnapshot) -> Self {
        Self {
            invocation_id: inv.invocation_id.clone(),
            run_id: inv.run_id.clone(),
            tool_call_id: inv.tool_call_id.clone(),
            tool_name: inv.tool_name.clone(),
            arguments: inv.arguments.clone(),
            status: inv.status.as_str().to_string(),
            result: inv.result.clone(),
            child_session_id: inv.child_session_id.clone(),
            error_code: inv.error_code.clone(),
        }
    }
}

/// Tin nhắn trong lịch sử phiên của một Agent (`user`, `assistant`, `tool`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAgentMessageView {
    pub id: String,
    pub sequence: u64,
    /// `"user"`, `"assistant"`, hoặc `"tool"`.
    pub role: String,
    pub text: String,
    pub run_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<TaskMessageToolCallView>,
}

impl TaskAgentMessageView {
    pub fn from_message_view(msg: &MessageView) -> Self {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
            _ => "unknown",
        };
        let text = if matches!(msg.role, MessageRole::Assistant) {
            llm_wiki::normalize_media_markdown_links(&msg.text)
        } else {
            msg.text.clone()
        };
        Self {
            id: msg.id.clone(),
            sequence: msg.sequence,
            role: role.to_string(),
            text,
            run_id: msg.run_id.clone(),
            tool_call_id: msg.tool_call_id.clone(),
            tool_calls: msg
                .tool_calls
                .iter()
                .map(TaskMessageToolCallView::from_tool_call)
                .collect(),
        }
    }
}

/// Lệnh gọi tool đính kèm trong một message `assistant`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMessageToolCallView {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

impl TaskMessageToolCallView {
    pub fn from_tool_call(tc: &MessageToolCall) -> Self {
        Self {
            call_id: tc.call_id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        }
    }
}
