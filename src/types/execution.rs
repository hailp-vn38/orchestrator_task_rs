//! Value types có `Serialize`/`Deserialize` biểu diễn chi tiết thực thi của Root Agent
//! và các Subagent con bên trong một Task (ánh xạ từ `FamilySnapshot` của `Orchestrator`).

use orchestrator_agent_rs::{
    FamilySnapshot, InvocationSnapshot, MessageRole, MessageToolCall, MessageView, RunSnapshot,
    RunStatus, SessionSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tổng hợp số liệu thực thi của toàn bộ cây Agent (Root + các Subagent) trong một Task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// `true` nếu danh sách Subagent con vượt trần snapshot của `Orchestrator`.
    pub children_truncated: bool,
}

impl TaskExecutionSummary {
    pub fn from_family_snapshot(snap: &FamilySnapshot) -> Self {
        let mut active_agents = Vec::new();
        let mut total_steps = 0u32;
        let mut total_input_tokens = 0u64;
        let mut total_output_tokens = 0u64;
        let mut total_tool_calls = 0usize;

        for session in snap.sessions() {
            total_tool_calls = total_tool_calls.saturating_add(session.invocations.len());
            if let Some(ref run) = session.latest_run {
                if run.status == RunStatus::Running && !active_agents.contains(&session.agent_id) {
                    active_agents.push(session.agent_id.clone());
                }
                total_steps = total_steps.saturating_add(run.steps);
                total_input_tokens = total_input_tokens.saturating_add(run.input_tokens);
                total_output_tokens = total_output_tokens.saturating_add(run.output_tokens);
            }
        }

        let subagent_count = snap.children.len();
        Self {
            total_agents: 1 + subagent_count,
            subagent_count,
            active_agents,
            total_steps,
            total_input_tokens,
            total_output_tokens,
            total_tokens: total_input_tokens.saturating_add(total_output_tokens),
            total_tool_calls,
            children_truncated: snap.children_truncated,
        }
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
        Self {
            id: msg.id.clone(),
            sequence: msg.sequence,
            role: role.to_string(),
            text: msg.text.clone(),
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
