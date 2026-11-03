use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use orchestrator_agent_rs::{
    GatewayError, HostTool, ModelMessage, ModelRequest, ModelResponse, ModelRole, ModelToolCall,
    ProviderRuntime, RuntimeProvider, StreamSink, ToolContext, ToolDescriptor, ToolEffect,
    ToolError, ToolOutcome,
};
use orchestrator_flow_rs::{
    RecipeDecision, RecipeMetadata, RecipeProgress, RecipeStepKind, RecipeStepOutline,
    RecipeValidationError, TaskRecipe, TaskRecipeRequest,
};

use super::*;

struct RecipeReadTool(Arc<AtomicUsize>);

#[async_trait]
impl HostTool for RecipeReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "recipe_read".into(),
            description: "Read a recipe input once.".into(),
            input_schema: json!({"type":"object", "properties":{}}),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(
        &self,
        _context: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutcome, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome {
            call_id: String::new(),
            content: "read complete".into(),
            metadata: json!({}),
        })
    }
}

struct RetryRecipe;

impl TaskRecipe for RetryRecipe {
    fn metadata(&self) -> RecipeMetadata {
        RecipeMetadata {
            id: "retry_recipe".into(),
            version: 1,
            title: "Retry recipe".into(),
            description: "A tool step followed by a delegated agent step.".into(),
            params_schema: json!({"type":"object", "properties":{}}),
            outline: vec![
                RecipeStepOutline {
                    title: "Read input".into(),
                    kind: RecipeStepKind::Tool,
                    repeat: false,
                    host_tools: vec!["recipe_read".into()],
                    sub_agents: vec![],
                },
                RecipeStepOutline {
                    title: "Analyze input".into(),
                    kind: RecipeStepKind::Agent,
                    repeat: false,
                    host_tools: vec![],
                    sub_agents: vec!["recipe_worker".into()],
                },
            ],
            max_steps: 5,
        }
    }

    fn validate_params(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, RecipeValidationError> {
        Ok(params.clone())
    }

    fn next(&self, _params: &serde_json::Value, transcript: &[ModelMessage]) -> RecipeDecision {
        let tool_results = transcript
            .iter()
            .filter(|message| message.role == ModelRole::Tool)
            .count();
        match tool_results {
            0 => RecipeDecision::ToolCalls(vec![ModelToolCall {
                id: "read_input".into(),
                name: "recipe_read".into(),
                arguments: json!({}),
            }]),
            1 => RecipeDecision::ToolCalls(vec![ModelToolCall {
                id: "analyze_input".into(),
                name: "subagent".into(),
                arguments: json!({
                    "agent": "recipe_worker",
                    "description": "Analyze the recipe input",
                    "prompt": "Analyze the already-read input."
                }),
            }]),
            _ => RecipeDecision::Complete("recipe complete".into()),
        }
    }

    fn progress(
        &self,
        _params: &serde_json::Value,
        _transcript: &[ModelMessage],
    ) -> RecipeProgress {
        RecipeProgress {
            current_step: None,
            completed_steps: vec![],
            summary: None,
            stop_reason: None,
            failed_step: None,
        }
    }
}

struct BatchRecipe;

impl TaskRecipe for BatchRecipe {
    fn metadata(&self) -> RecipeMetadata {
        RecipeMetadata {
            id: "batch_recipe".into(),
            version: 1,
            title: "Batch recipe".into(),
            description: "A recipe with one in-flight tool batch.".into(),
            params_schema: json!({"type":"object", "properties":{}}),
            outline: vec![RecipeStepOutline {
                title: "Process batch".into(),
                kind: RecipeStepKind::Tool,
                repeat: true,
                host_tools: vec!["slow_a".into(), "slow_b".into()],
                sub_agents: vec![],
            }],
            max_steps: 3,
        }
    }

    fn validate_params(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, RecipeValidationError> {
        Ok(params.clone())
    }

    fn next(&self, _params: &serde_json::Value, transcript: &[ModelMessage]) -> RecipeDecision {
        if transcript
            .iter()
            .any(|message| message.role == ModelRole::Tool)
        {
            RecipeDecision::Complete("batch complete".into())
        } else {
            RecipeDecision::ToolCalls(
                vec!["slow_a", "slow_b"]
                    .into_iter()
                    .map(|name| ModelToolCall {
                        id: format!("call_{name}"),
                        name: name.into(),
                        arguments: json!({}),
                    })
                    .collect(),
            )
        }
    }

    fn progress(
        &self,
        _params: &serde_json::Value,
        _transcript: &[ModelMessage],
    ) -> RecipeProgress {
        RecipeProgress {
            current_step: Some(0),
            completed_steps: vec![],
            summary: None,
            stop_reason: None,
            failed_step: None,
        }
    }
}

struct BudgetRecipe;

impl TaskRecipe for BudgetRecipe {
    fn metadata(&self) -> RecipeMetadata {
        RecipeMetadata {
            id: "budget_recipe".into(),
            version: 1,
            title: "Budget recipe".into(),
            description: "A recipe step that consumes the shared active time budget.".into(),
            params_schema: json!({"type":"object", "properties":{}}),
            outline: vec![RecipeStepOutline {
                title: "Collect partial result".into(),
                kind: RecipeStepKind::Tool,
                repeat: false,
                host_tools: vec!["slow_budget_tool".into()],
                sub_agents: vec![],
            }],
            max_steps: 3,
        }
    }

    fn validate_params(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, RecipeValidationError> {
        Ok(params.clone())
    }

    fn next(&self, _params: &serde_json::Value, transcript: &[ModelMessage]) -> RecipeDecision {
        if transcript
            .iter()
            .any(|message| message.role == ModelRole::Tool)
        {
            RecipeDecision::Complete("partial recipe result".into())
        } else {
            RecipeDecision::ToolCalls(vec![ModelToolCall {
                id: "slow_budget_call".into(),
                name: "slow_budget_tool".into(),
                arguments: json!({}),
            }])
        }
    }

    fn progress(
        &self,
        _params: &serde_json::Value,
        _transcript: &[ModelMessage],
    ) -> RecipeProgress {
        RecipeProgress {
            current_step: Some(0),
            completed_steps: vec![],
            summary: Some("one partial result".into()),
            stop_reason: None,
            failed_step: None,
        }
    }
}

struct SlowBudgetTool(Arc<AtomicUsize>);

#[async_trait]
impl HostTool for SlowBudgetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "slow_budget_tool".into(),
            description: "Takes the full Voice Job active-time budget.".into(),
            input_schema: json!({"type":"object", "properties":{}}),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutcome, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(300)).await;
        Ok(ToolOutcome {
            call_id: context.scope().tool_call_id.clone(),
            content: "partial collection completed".into(),
            metadata: json!({"items": 1}),
        })
    }
}

struct SlowTool {
    name: &'static str,
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}

struct DurableRecipe {
    id: &'static str,
    tool: &'static str,
}

impl TaskRecipe for DurableRecipe {
    fn metadata(&self) -> RecipeMetadata {
        RecipeMetadata {
            id: self.id.into(),
            version: 1,
            title: "Durable recipe".into(),
            description: "One durable tool step.".into(),
            params_schema: json!({"type":"object", "properties":{}}),
            outline: vec![RecipeStepOutline {
                title: "Read one value".into(),
                kind: RecipeStepKind::Tool,
                repeat: false,
                host_tools: vec![self.tool.into()],
                sub_agents: vec![],
            }],
            max_steps: 3,
        }
    }

    fn validate_params(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, RecipeValidationError> {
        Ok(params.clone())
    }

    fn next(&self, _params: &serde_json::Value, transcript: &[ModelMessage]) -> RecipeDecision {
        if transcript.iter().any(|message| {
            message.role == ModelRole::Tool
                && message.tool_name.as_deref() == Some(self.tool)
                && !message.text.trim_start().starts_with("error ")
        }) {
            RecipeDecision::Complete("durable recipe complete".into())
        } else {
            RecipeDecision::ToolCalls(vec![ModelToolCall {
                id: "durable_step".into(),
                name: self.tool.into(),
                arguments: json!({}),
            }])
        }
    }

    fn progress(
        &self,
        _params: &serde_json::Value,
        _transcript: &[ModelMessage],
    ) -> RecipeProgress {
        RecipeProgress {
            current_step: Some(0),
            completed_steps: vec![],
            summary: None,
            stop_reason: None,
            failed_step: None,
        }
    }
}

struct RecoveryProbeTool {
    name: &'static str,
    effect: ToolEffect,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl HostTool for RecoveryProbeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.into(),
            description: "A probe for Task restart behavior.".into(),
            input_schema: json!({"type":"object", "properties":{}}),
            effect: self.effect,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutcome, ToolError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            let _ = context;
            std::future::pending::<()>().await;
        }
        Ok(ToolOutcome {
            call_id: String::new(),
            content: "read done".into(),
            metadata: json!({}),
        })
    }
}

#[async_trait]
impl HostTool for SlowTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.into(),
            description: "Wait until the Task is cancelled.".into(),
            input_schema: json!({"type":"object", "properties":{}}),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutcome, ToolError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        context.cancel_token().cancelled().await;
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Err(ToolError {
            code: "CANCELLED".into(),
            message: "cancelled by the Task manager".into(),
        })
    }
}

struct RecipeAndScriptedRuntime {
    recipes: Arc<orchestrator_flow_rs::RecipeProviderRuntime>,
    allow_agent_response: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ProviderRuntime for RecipeAndScriptedRuntime {
    fn providers(&self) -> Vec<RuntimeProvider> {
        vec![
            RuntimeProvider::new("recipe", "test-recipe-runtime:v1"),
            RuntimeProvider::new("test", "test-scripted-runtime:v1"),
        ]
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        cancel: &tokio_util::sync::CancellationToken,
        sink: StreamSink<'_>,
    ) -> Result<ModelResponse, GatewayError> {
        if request.provider_id == "recipe" {
            self.recipes.complete(request, cancel, sink).await
        } else if self.allow_agent_response.load(Ordering::SeqCst) {
            Ok(ModelResponse {
                text: "analysis complete".into(),
                finish_reason: "stop".into(),
                tool_calls: vec![],
                usage: None,
            })
        } else {
            Err(GatewayError::RateLimited {
                retry_after_secs: Some(60),
            })
        }
    }
}

#[tokio::test]
async fn recipe_waits_on_agent_rate_limit_and_resumes_without_repeating_completed_tool_step() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("agent.md");
    std::fs::write(&prompt_path, "Test agent").unwrap();
    let read_count = Arc::new(AtomicUsize::new(0));
    let task_init = TaskManager::prepare(TaskConfig::new(dir.path().join("task_store")))
        .await
        .unwrap()
        .register_recipe(Arc::new(RetryRecipe))
        .unwrap();

    let allow_agent_response = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let root = AgentDefinition::new("recipe_retry_recipe", "recipe", "retry_recipe@1")
        .with_mode(AgentMode::Primary)
        .with_description("Deterministic recipe root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["recipe_read", "subagent"])
        .with_allowed_subagents(["recipe_worker"])
        .with_steps(5);
    let worker = AgentDefinition::new("recipe_worker", "test", "gpt-test")
        .with_mode(AgentMode::Subagent)
        .with_description("Recipe worker")
        .with_prompt_path(&prompt_path);
    let mut builder = Orchestrator::builder(OrchestratorConfig::new(
        orchestrator_agent_rs::LlmConfig { providers: vec![] },
        vec![root, worker],
        dir.path().join("orchestrator_store"),
    ))
    .with_provider_runtime(Arc::new(RecipeAndScriptedRuntime {
        recipes: task_init.recipe_provider_runtime().unwrap(),
        allow_agent_response: allow_agent_response.clone(),
    }));
    builder = builder.register_tool(Arc::new(RecipeReadTool(read_count.clone())));
    let orchestrator = Arc::new(builder.build().await.unwrap());
    let manager = task_init
        .register_flow("default", orchestrator.clone())
        .start()
        .await
        .unwrap();

    let created = manager
        .create_task(CreateTaskRequest {
            recipe: Some(TaskRecipeRequest {
                id: "retry_recipe".into(),
                params: json!({}),
            }),
            ..CreateTaskRequest::new("", "")
        })
        .await
        .unwrap();

    wait_for_task_status(&manager, &created.id, TaskStatus::Waiting).await;
    assert_eq!(read_count.load(Ordering::SeqCst), 1);
    allow_agent_response.store(true, Ordering::SeqCst);
    manager.resume_waiting_task(&created.id).await.unwrap();
    wait_for_task_status(&manager, &created.id, TaskStatus::Completed).await;

    assert_eq!(read_count.load(Ordering::SeqCst), 1);
    let detail = manager.get_task_detail(&created.id).await.unwrap();
    assert_eq!(detail.task.status, TaskStatus::Completed);
    assert_eq!(
        detail.task.output_preview.as_deref(),
        Some("recipe complete")
    );

    manager.shutdown().await.unwrap();
    orchestrator.shutdown().await.unwrap();
}

#[tokio::test]
async fn recipe_uses_the_shared_voice_job_budget_and_keeps_its_partial_result() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("agent.md");
    std::fs::write(&prompt_path, "test").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let task_init = TaskManager::prepare(TaskConfig::new(dir.path().join("task_store")))
        .await
        .unwrap()
        .register_recipe(Arc::new(BudgetRecipe))
        .unwrap();
    let root = AgentDefinition::new("recipe_budget_recipe", "recipe", "budget_recipe@1")
        .with_mode(AgentMode::Primary)
        .with_description("Recipe root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["slow_budget_tool"])
        .with_steps(3);
    let orchestrator = Arc::new(
        Orchestrator::builder(OrchestratorConfig::new(
            orchestrator_agent_rs::LlmConfig { providers: vec![] },
            vec![root],
            dir.path().join("orchestrator_store"),
        ))
        .with_provider_runtime(task_init.recipe_provider_runtime().unwrap())
        .register_tool(Arc::new(SlowBudgetTool(calls.clone())))
        .build()
        .await
        .unwrap(),
    );
    let manager = task_init
        .register_flow("default", orchestrator.clone())
        .start()
        .await
        .unwrap();

    tokio::time::pause();
    let task = manager
        .create_task(
            CreateTaskRequest {
                recipe: Some(TaskRecipeRequest {
                    id: "budget_recipe".into(),
                    params: json!({}),
                }),
                ..CreateTaskRequest::new("", "")
            }
            .with_source(TaskSource::Voice),
        )
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let stopped = loop {
        let current = manager.get_task(&task.id).await.unwrap();
        if current.status.is_terminal() {
            break current;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "recipe budget task did not stop: {current:?}"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    assert_eq!(stopped.status, TaskStatus::Stopped, "{stopped:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let detail = manager.get_task_detail(&task.id).await.unwrap();
    assert_eq!(detail.task.error_code.as_deref(), Some("BUDGET_EXHAUSTED"));
    assert_eq!(
        detail.task.output_preview.as_deref(),
        Some("partial recipe result")
    );
    let summary = detail.execution_summary.unwrap();
    assert_eq!(summary.total_input_tokens, 0);
    assert_eq!(summary.total_output_tokens, 0);
    let invocation = detail
        .root_agent
        .unwrap()
        .invocations
        .into_iter()
        .find(|inv| inv.tool_name == "slow_budget_tool")
        .expect("completed partial result is visible");
    assert_eq!(invocation.status, "completed");

    manager.shutdown().await.unwrap();
    orchestrator.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_recipe_stops_the_active_tool_before_the_next_batch_call() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("agent.md");
    std::fs::write(&prompt_path, "test").unwrap();
    let task_init = TaskManager::prepare(TaskConfig::new(dir.path().join("task_store")))
        .await
        .unwrap()
        .register_recipe(Arc::new(BatchRecipe))
        .unwrap();
    let started = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let root = AgentDefinition::new("recipe_batch_recipe", "recipe", "batch_recipe@1")
        .with_mode(AgentMode::Primary)
        .with_description("Recipe root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["slow_a", "slow_b"])
        .with_steps(3);
    let orchestrator = Arc::new(
        Orchestrator::builder(OrchestratorConfig::new(
            orchestrator_agent_rs::LlmConfig { providers: vec![] },
            vec![root],
            dir.path().join("orchestrator_store"),
        ))
        .with_provider_runtime(task_init.recipe_provider_runtime().unwrap())
        .register_tool(Arc::new(SlowTool {
            name: "slow_a",
            started: started.clone(),
            stopped: stopped.clone(),
        }))
        .register_tool(Arc::new(SlowTool {
            name: "slow_b",
            started: started.clone(),
            stopped: stopped.clone(),
        }))
        .build()
        .await
        .unwrap(),
    );
    let manager = task_init
        .register_flow("default", orchestrator.clone())
        .start()
        .await
        .unwrap();
    let task = manager
        .create_task(CreateTaskRequest {
            recipe: Some(TaskRecipeRequest {
                id: "batch_recipe".into(),
                params: json!({}),
            }),
            ..CreateTaskRequest::new("", "")
        })
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while started.load(Ordering::SeqCst) < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first call in the recipe batch should start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    manager.cancel_task(&task.id).await.unwrap();
    wait_for_task_status(&manager, &task.id, TaskStatus::Cancelled).await;
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);

    manager.shutdown().await.unwrap();
    orchestrator.shutdown().await.unwrap();
}

async fn start_durable_recipe_system(
    task_dir: &std::path::Path,
    orchestrator_dir: &std::path::Path,
    prompt_path: &std::path::Path,
    register_recipe: bool,
    tool: Arc<RecoveryProbeTool>,
) -> (Arc<TaskManager>, Arc<Orchestrator>) {
    let mut init =
        TaskManager::prepare(TaskConfig::new(task_dir).with_default_agent("recipe_durable_recipe"))
            .await
            .unwrap();
    if register_recipe {
        init = init
            .register_recipe(Arc::new(DurableRecipe {
                id: "durable_recipe",
                tool: tool.name,
            }))
            .unwrap();
    }
    let root = AgentDefinition::new("recipe_durable_recipe", "recipe", "durable_recipe@1")
        .with_mode(AgentMode::Primary)
        .with_description("Durable recipe root")
        .with_prompt_path(prompt_path)
        .with_allowed_tools([tool.name])
        .with_steps(3);
    let orchestrator = Arc::new(
        Orchestrator::builder(OrchestratorConfig::new(
            orchestrator_agent_rs::LlmConfig { providers: vec![] },
            vec![root],
            orchestrator_dir,
        ))
        .with_provider_runtime(init.recipe_provider_runtime().unwrap())
        .register_tool(tool)
        .build()
        .await
        .unwrap(),
    );
    let manager = init
        .register_flow("default", orchestrator.clone())
        .start()
        .await
        .unwrap();
    (manager, orchestrator)
}

async fn stop_with_inflight_recipe_tool(
    manager: &TaskManager,
    orchestrator: &Orchestrator,
    task_id: &str,
) {
    manager.shutdown().await.unwrap();
    orchestrator.shutdown().await.unwrap();
    let _ = task_id;
}

async fn wait_for_call_count(calls: &AtomicUsize, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) < expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "tool call did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn read_only_recipe_step_replays_after_restart_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let task_dir = dir.path().join("tasks");
    let orch_dir = dir.path().join("orchestrator");
    let prompt = dir.path().join("agent.md");
    std::fs::write(&prompt, "test").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(RecoveryProbeTool {
        name: "recover_read",
        effect: ToolEffect::ReadOnly,
        calls: calls.clone(),
    });
    let (manager, orchestrator) =
        start_durable_recipe_system(&task_dir, &orch_dir, &prompt, true, tool.clone()).await;
    let task = manager
        .create_task(CreateTaskRequest {
            recipe: Some(TaskRecipeRequest {
                id: "durable_recipe".into(),
                params: json!({}),
            }),
            ..CreateTaskRequest::new("", "")
        })
        .await
        .unwrap();
    wait_for_call_count(&calls, 1).await;
    stop_with_inflight_recipe_tool(&manager, &orchestrator, &task.id).await;

    let (manager, orchestrator) =
        start_durable_recipe_system(&task_dir, &orch_dir, &prompt, true, tool).await;
    wait_for_task_status(&manager, &task.id, TaskStatus::Completed).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        manager
            .get_task(&task.id)
            .await
            .unwrap()
            .output_preview
            .as_deref(),
        Some("durable recipe complete")
    );
    manager.shutdown().await.unwrap();
    orchestrator.shutdown().await.unwrap();
}

#[tokio::test]
async fn mutating_recipe_step_is_not_replayed_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let task_dir = dir.path().join("tasks");
    let orch_dir = dir.path().join("orchestrator");
    let prompt = dir.path().join("agent.md");
    std::fs::write(&prompt, "test").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(RecoveryProbeTool {
        name: "recover_write",
        effect: ToolEffect::Mutating,
        calls: calls.clone(),
    });
    let (manager, orchestrator) =
        start_durable_recipe_system(&task_dir, &orch_dir, &prompt, true, tool.clone()).await;
    let task = manager
        .create_task(CreateTaskRequest {
            recipe: Some(TaskRecipeRequest {
                id: "durable_recipe".into(),
                params: json!({}),
            }),
            ..CreateTaskRequest::new("", "")
        })
        .await
        .unwrap();
    wait_for_call_count(&calls, 1).await;
    stop_with_inflight_recipe_tool(&manager, &orchestrator, &task.id).await;

    let (manager, orchestrator) =
        start_durable_recipe_system(&task_dir, &orch_dir, &prompt, true, tool).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let detail = manager.get_task_detail(&task.id).await.unwrap();
    assert_eq!(detail.task.status, TaskStatus::Interrupted);
    assert!(detail
        .root_agent
        .unwrap()
        .invocations
        .iter()
        .any(|inv| { inv.tool_name == "recover_write" && inv.status == "interrupted" }));
    manager.shutdown().await.unwrap();
    orchestrator.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_pinned_recipe_version_missing_after_restart_fails_with_a_stable_code() {
    let dir = tempfile::tempdir().unwrap();
    let task_dir = dir.path().join("tasks");
    let orch_dir = dir.path().join("orchestrator");
    let prompt = dir.path().join("agent.md");
    std::fs::write(&prompt, "test").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(RecoveryProbeTool {
        name: "recover_read",
        effect: ToolEffect::ReadOnly,
        calls: calls.clone(),
    });
    let (manager, orchestrator) =
        start_durable_recipe_system(&task_dir, &orch_dir, &prompt, true, tool.clone()).await;
    let task = manager
        .create_task(CreateTaskRequest {
            recipe: Some(TaskRecipeRequest {
                id: "durable_recipe".into(),
                params: json!({}),
            }),
            ..CreateTaskRequest::new("", "")
        })
        .await
        .unwrap();
    wait_for_call_count(&calls, 1).await;
    stop_with_inflight_recipe_tool(&manager, &orchestrator, &task.id).await;

    let (manager, orchestrator) =
        start_durable_recipe_system(&task_dir, &orch_dir, &prompt, false, tool).await;
    wait_for_task_status(&manager, &task.id, TaskStatus::Failed).await;
    let task_view = manager.get_task(&task.id).await.unwrap();
    assert_eq!(
        task_view.error_code.as_deref(),
        Some("RECIPE_VERSION_UNAVAILABLE")
    );
    manager.shutdown().await.unwrap();
    orchestrator.shutdown().await.unwrap();
}

async fn wait_for_task_status(manager: &TaskManager, task_id: &str, expected: TaskStatus) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let task = manager.get_task(task_id).await.unwrap();
        if task.status == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Task did not reach {expected:?}: {task:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
