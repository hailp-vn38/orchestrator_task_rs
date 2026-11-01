//! Integration tests tại public interface của `TaskManager` (theo `codebase-design`:
//! "The interface is the test surface").

use std::sync::Arc;
use std::time::Duration;

use llm_wiki::prompts::WikiTemplateId;
use llm_wiki::{WikiEngine, WriteTopicSourceRequest};
use orchestrator_agent_rs::test_support::ScriptedGateway;
use orchestrator_agent_rs::{
    AgentDefinition, AgentMode, LlmBackend, LlmConfig, LlmProviderConfig, Orchestrator,
    OrchestratorConfig, ProviderAuth, SecretString,
};
use orchestrator_flow_rs::{
    build_sample_technical_pdf, CreateScheduleRequest, CreateTaskRequest, Priority,
    ScheduleActionKind, ScheduleFilter, TaskConfig, TaskEvent, TaskFeedItem, TaskFilter,
    TaskManager, TaskRuntimeConfig, TaskScope, TaskSource, TaskStatus, WikiAutoFlowBinding,
    WorkspaceLayout, WorkspaceToolKit, COLLECTOR_TOOL_NAMES,
};
use serde_json::json;
use tokio::sync::Notify;

pub(crate) fn test_llm_config() -> LlmConfig {
    LlmConfig {
        providers: vec![LlmProviderConfig {
            id: "test".to_string(),
            backend: LlmBackend::OpenAiCompatible,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            auth: ProviderAuth::ApiKey(SecretString::new("test-key")),
            limits: orchestrator_agent_rs::RequestLimits::uniform(Duration::from_secs(10)),
        }],
    }
}

#[path = "integration/asr_transcribe.rs"]
mod asr_transcribe;
#[path = "integration/autoflow_and_toolkit.rs"]
mod autoflow_and_toolkit;
#[path = "integration/collection_batch.rs"]
mod collection_batch;
#[path = "integration/media_artifacts.rs"]
mod media_artifacts;
#[path = "integration/scope_and_watcher.rs"]
mod scope_and_watcher;
#[path = "integration/sources_guardrails.rs"]
mod sources_guardrails;
#[path = "integration/task_lifecycle.rs"]
mod task_lifecycle;
