//! Deterministic task recipes executed through the normal Orchestrator tool pipeline.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use orchestrator_agent_rs::{
    GatewayError, ModelMessage, ModelRequest, ModelResponse, ModelRole, ModelToolCall, ModelUsage,
    ProviderRuntime, RuntimeProvider, StreamSink,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeStepKind {
    Tool,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeStepOutline {
    pub title: String,
    pub kind: RecipeStepKind,
    #[serde(default)]
    pub repeat: bool,
    /// HostTools explicitly used by this step. Server startup validates these against the
    /// complete registered tool catalog before building the Orchestrator.
    #[serde(default)]
    pub host_tools: Vec<String>,
    /// Sub-Agent definitions explicitly used by this step.
    #[serde(default)]
    pub sub_agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeMetadata {
    pub id: String,
    pub version: u32,
    pub title: String,
    pub description: String,
    pub params_schema: Value,
    pub outline: Vec<RecipeStepOutline>,
    pub max_steps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeProgress {
    pub current_step: Option<usize>,
    pub completed_steps: Vec<usize>,
    pub summary: Option<String>,
    pub stop_reason: Option<String>,
    pub failed_step: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeValidationError {
    pub validation_path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeDecision {
    ToolCalls(Vec<ModelToolCall>),
    Complete(String),
    Failed {
        stop_reason: String,
        failed_step: usize,
    },
}

/// A registered deterministic recipe. Implementations must be pure: the transcript is the
/// durable cursor, and equal params/transcripts must produce equal decisions and progress.
pub trait TaskRecipe: Send + Sync {
    fn metadata(&self) -> RecipeMetadata;
    fn validate_params(&self, params: &Value) -> Result<Value, RecipeValidationError>;
    fn next(&self, params: &Value, transcript: &[ModelMessage]) -> RecipeDecision;
    fn progress(&self, params: &Value, transcript: &[ModelMessage]) -> RecipeProgress;
}

/// ProviderRuntime adapter for registered recipes. Root agents use `provider_id = "recipe"`
/// and pin a recipe with `model_id = "<id>@<version>"`.
pub struct RecipeProviderRuntime {
    recipes: BTreeMap<(String, u32), Arc<dyn TaskRecipe>>,
}

impl RecipeProviderRuntime {
    pub fn new(recipes: Vec<Arc<dyn TaskRecipe>>) -> Result<Self, RecipeValidationError> {
        let mut registry = BTreeMap::new();
        for recipe in recipes {
            let metadata = recipe.metadata();
            if metadata.id.trim().is_empty() || metadata.version == 0 {
                return Err(RecipeValidationError {
                    validation_path: "id".into(),
                    message: "recipe id must be non-empty and version must be positive".into(),
                });
            }
            validate_recipe_schema(&metadata.params_schema)?;
            let key = (metadata.id.clone(), metadata.version);
            if registry.insert(key, recipe).is_some() {
                return Err(RecipeValidationError {
                    validation_path: "id".into(),
                    message: format!(
                        "duplicate recipe id/version '{}@{}'",
                        metadata.id, metadata.version
                    ),
                });
            }
        }
        Ok(Self { recipes: registry })
    }

    pub fn recipes(&self) -> Vec<RecipeMetadata> {
        self.recipes
            .values()
            .map(|recipe| recipe.metadata())
            .collect()
    }

    pub fn recipe(&self, id: &str, version: u32) -> Option<&Arc<dyn TaskRecipe>> {
        self.recipes.get(&(id.to_owned(), version))
    }

    fn resolve_model_id(&self, model_id: &str) -> Result<&Arc<dyn TaskRecipe>, GatewayError> {
        let (id, raw_version) = model_id
            .rsplit_once('@')
            .ok_or(GatewayError::InvalidRequest)?;
        let version = raw_version
            .parse::<u32>()
            .map_err(|_| GatewayError::InvalidRequest)?;
        let recipe = self
            .recipe(id, version)
            .ok_or(GatewayError::HostPermanent {
                code: "RECIPE_VERSION_UNAVAILABLE",
                message: "the pinned task recipe version is unavailable",
            })?;
        let _ = id;
        Ok(recipe)
    }
}

/// Validate the references declared by recipe outlines against the host's real catalogs.
/// This is called by the server after assembling all tools and agent definitions, before build.
pub fn validate_recipe_registry(
    recipes: &[Arc<dyn TaskRecipe>],
    registered_tools: &std::collections::HashSet<String>,
    registered_agents: &std::collections::HashSet<String>,
) -> Result<(), RecipeValidationError> {
    for recipe in recipes {
        let metadata = recipe.metadata();
        for (step_index, step) in metadata.outline.iter().enumerate() {
            let path = format!("outline[{step_index}]");
            match step.kind {
                RecipeStepKind::Tool => {
                    if step.host_tools.is_empty() || !step.sub_agents.is_empty() {
                        return Err(validation(
                            &path,
                            "tool steps must declare HostTools and cannot declare Sub-Agents",
                        ));
                    }
                    for tool in &step.host_tools {
                        if !registered_tools.contains(tool) {
                            return Err(validation(
                                &format!("{path}.host_tools.{tool}"),
                                "recipe outline references an unregistered HostTool",
                            ));
                        }
                    }
                }
                RecipeStepKind::Agent => {
                    if step.sub_agents.is_empty() || !step.host_tools.is_empty() {
                        return Err(validation(
                            &path,
                            "agent steps must declare Sub-Agents and cannot declare HostTools",
                        ));
                    }
                    for agent in &step.sub_agents {
                        if !registered_agents.contains(agent) {
                            return Err(validation(
                                &format!("{path}.sub_agents.{agent}"),
                                "recipe outline references an unregistered Sub-Agent",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[async_trait]
impl ProviderRuntime for RecipeProviderRuntime {
    fn providers(&self) -> Vec<RuntimeProvider> {
        vec![RuntimeProvider::new("recipe", "task-recipe-runtime:v1")]
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        _sink: StreamSink<'_>,
    ) -> Result<ModelResponse, GatewayError> {
        if cancel.is_cancelled() {
            return Err(GatewayError::Cancelled);
        }
        let recipe = self.resolve_model_id(&request.model_id)?;
        let params = request
            .messages
            .iter()
            .find(|m| m.role == ModelRole::User)
            .and_then(|m| serde_json::from_str::<Value>(&m.text).ok())
            .ok_or(GatewayError::InvalidRequest)?;
        let decision = recipe.next(&params, &request.messages);
        let (text, tool_calls) = match decision {
            RecipeDecision::ToolCalls(calls) => (String::new(), calls),
            RecipeDecision::Complete(summary) => (summary, Vec::new()),
            RecipeDecision::Failed {
                stop_reason,
                failed_step,
            } => {
                tracing::error!(%stop_reason, failed_step, "recipe step failed");
                return Err(GatewayError::HostPermanent {
                    code: "RECIPE_STEP_FAILED",
                    message: "a deterministic task recipe step failed",
                });
            }
        };
        Ok(ModelResponse {
            text,
            finish_reason: if tool_calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
            .into(),
            tool_calls,
            usage: Some(ModelUsage::new(0, 0)),
        })
    }
}

/// Validate the deliberately small JSON Schema subset supported by task recipes.
pub fn validate_recipe_params(
    schema: &Value,
    params: &Value,
) -> Result<Value, RecipeValidationError> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Err(validation(
            "params_schema",
            "recipe params_schema must define properties",
        ));
    };
    let object = params
        .as_object()
        .ok_or_else(|| validation("params", "parameters must be an object"))?;
    if let Some((key, _)) = object
        .iter()
        .find(|(key, _)| !properties.contains_key(*key))
    {
        return Err(validation(&format!("params.{key}"), "unknown parameter"));
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                if let Some(default) = properties.get(key).and_then(|p| p.get("default")) {
                    let _ = default;
                } else {
                    return Err(validation(
                        &format!("params.{key}"),
                        "required parameter is missing",
                    ));
                }
            }
        }
    }
    let mut normalized = object.clone();
    for (key, property) in properties {
        if !normalized.contains_key(key) {
            if let Some(default) = property.get("default") {
                normalized.insert(key.clone(), default.clone());
            }
            continue;
        }
        let value = &normalized[key];
        let kind = property
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let valid = match kind {
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some_and(|n| {
                property
                    .get("minimum")
                    .and_then(Value::as_i64)
                    .is_none_or(|min| n >= min)
                    && property
                        .get("maximum")
                        .and_then(Value::as_i64)
                        .is_none_or(|max| n <= max)
            }),
            "boolean" => value.is_boolean(),
            "array" => {
                property
                    .get("items")
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    == Some("string")
                    && value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
            }
            _ => false,
        };
        if !valid {
            return Err(validation(
                &format!("params.{key}"),
                "value does not match the recipe parameter schema",
            ));
        }
        if let Some(options) = property.get("enum").and_then(Value::as_array) {
            if !options.contains(value) {
                return Err(validation(
                    &format!("params.{key}"),
                    "value is not an allowed option",
                ));
            }
        }
        if property.get("format").and_then(Value::as_str) == Some("uri") {
            let text = value.as_str().unwrap_or_default();
            if !(text.starts_with("http://") || text.starts_with("https://")) {
                return Err(validation(
                    &format!("params.{key}"),
                    "value must be an http(s) URI",
                ));
            }
        }
    }
    Ok(Value::Object(normalized))
}

/// Validate the supported JSON Schema subset at registration time, before any Task can be made.
pub fn validate_recipe_schema(schema: &Value) -> Result<(), RecipeValidationError> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(validation(
            "params_schema.type",
            "recipe params schema must be an object",
        ));
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Err(validation(
            "params_schema.properties",
            "recipe params schema must define properties",
        ));
    };
    if schema.get("additionalProperties").and_then(Value::as_bool) == Some(true) {
        return Err(validation(
            "params_schema.additionalProperties",
            "additional properties must be disabled",
        ));
    }
    if let Some(required) = schema.get("required") {
        let Some(required) = required.as_array() else {
            return Err(validation(
                "params_schema.required",
                "required must be an array of parameter names",
            ));
        };
        for name in required.iter().filter_map(Value::as_str) {
            if !properties.contains_key(name) {
                return Err(validation(
                    "params_schema.required",
                    "required names must reference declared parameters",
                ));
            }
        }
    }
    for (name, property) in properties {
        let path = format!("params_schema.properties.{name}");
        let kind = property
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "string" => {
                if property
                    .get("format")
                    .is_some_and(|format| format.as_str() != Some("uri"))
                {
                    return Err(validation(
                        &format!("{path}.format"),
                        "only string format 'uri' is supported",
                    ));
                }
                if property
                    .get("x-options-source")
                    .is_some_and(|source| !source.is_string())
                {
                    return Err(validation(
                        &format!("{path}.x-options-source"),
                        "x-options-source must be a string",
                    ));
                }
            }
            "integer" => {
                let min = property.get("minimum").and_then(Value::as_i64);
                let max = property.get("maximum").and_then(Value::as_i64);
                if property.get("minimum").is_some_and(|_| min.is_none())
                    || property.get("maximum").is_some_and(|_| max.is_none())
                {
                    return Err(validation(
                        &path,
                        "integer minimum and maximum must be integers",
                    ));
                }
                if min
                    .zip(max)
                    .is_some_and(|(minimum, maximum)| minimum > maximum)
                {
                    return Err(validation(&path, "integer minimum must not exceed maximum"));
                }
            }
            "boolean" => {}
            "array"
                if property
                    .get("items")
                    .and_then(|items| items.get("type"))
                    .and_then(Value::as_str)
                    == Some("string") => {}
            _ => {
                return Err(validation(
                    &format!("{path}.type"),
                    "unsupported recipe parameter type",
                ))
            }
        }
        if let Some(options) = property.get("enum") {
            if !options.as_array().is_some_and(|values| !values.is_empty()) {
                return Err(validation(
                    &format!("{path}.enum"),
                    "enum must be a non-empty array",
                ));
            }
        }
    }
    Ok(())
}

fn validation(path: &str, message: &str) -> RecipeValidationError {
    RecipeValidationError {
        validation_path: path.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct OneCallRecipe;
    impl TaskRecipe for OneCallRecipe {
        fn metadata(&self) -> RecipeMetadata {
            RecipeMetadata {
                id: "playlist_sources".into(),
                version: 1,
                title: "Playlist".into(),
                description: "Collect playlist".into(),
                params_schema: json!({"type":"object","properties":{"url":{"type":"string","format":"uri"}},"required":["url"]}),
                outline: vec![RecipeStepOutline {
                    title: "List videos".into(),
                    kind: RecipeStepKind::Tool,
                    repeat: false,
                    host_tools: vec!["yt_list_playlist".into()],
                    sub_agents: vec![],
                }],
                max_steps: 4,
            }
        }
        fn validate_params(&self, params: &Value) -> Result<Value, RecipeValidationError> {
            validate_recipe_params(&self.metadata().params_schema, params)
        }
        fn next(&self, _params: &Value, transcript: &[ModelMessage]) -> RecipeDecision {
            if transcript.iter().any(|m| m.role == ModelRole::Tool) {
                RecipeDecision::Complete("done".into())
            } else {
                RecipeDecision::ToolCalls(vec![ModelToolCall {
                    id: "step-1".into(),
                    name: "yt_list_playlist".into(),
                    arguments: json!({"url":"https://example.com/list"}),
                }])
            }
        }
        fn progress(&self, _params: &Value, transcript: &[ModelMessage]) -> RecipeProgress {
            RecipeProgress {
                current_step: (!transcript.iter().any(|m| m.role == ModelRole::Tool)).then_some(0),
                completed_steps: if transcript.iter().any(|m| m.role == ModelRole::Tool) {
                    vec![0]
                } else {
                    vec![]
                },
                summary: None,
                stop_reason: None,
                failed_step: None,
            }
        }
    }

    #[tokio::test]
    async fn recipe_runtime_turns_a_pure_decision_into_zero_usage_tool_calls() {
        let runtime = RecipeProviderRuntime::new(vec![Arc::new(OneCallRecipe)]).unwrap();
        let request = ModelRequest {
            provider_id: "recipe".into(),
            model_id: "playlist_sources@1".into(),
            system: String::new(),
            messages: vec![ModelMessage::text(
                ModelRole::User,
                r#"{"url":"https://example.com/list"}"#,
            )],
            tools: vec![],
            params: Default::default(),
        };
        let cancel = CancellationToken::new();
        let sink = |_event| {};
        let response = runtime.complete(&request, &cancel, &sink).await.unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "yt_list_playlist");
        assert_eq!(response.usage, Some(ModelUsage::new(0, 0)));
    }

    #[tokio::test]
    async fn recipe_runtime_fails_permanently_when_the_pinned_version_is_missing() {
        let runtime = RecipeProviderRuntime::new(vec![Arc::new(OneCallRecipe)]).unwrap();
        let request = ModelRequest {
            provider_id: "recipe".into(),
            model_id: "playlist_sources@2".into(),
            system: String::new(),
            messages: vec![ModelMessage::text(
                ModelRole::User,
                r#"{"url":"https://example.com/list"}"#,
            )],
            tools: vec![],
            params: Default::default(),
        };
        let cancel = CancellationToken::new();
        let sink = |_event| {};
        let error = runtime
            .complete(&request, &cancel, &sink)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "RECIPE_VERSION_UNAVAILABLE");
    }

    #[test]
    fn recipe_parameter_validation_applies_uri_and_defaults() {
        let recipe = OneCallRecipe;
        let err = recipe
            .validate_params(&json!({"url":"file:///tmp/list"}))
            .unwrap_err();
        assert_eq!(err.validation_path, "params.url");
    }

    #[test]
    fn recipe_schema_rejects_types_outside_the_supported_subset_at_registration() {
        let err = validate_recipe_schema(&json!({
            "type": "object",
            "properties": {"payload": {"type": "object"}}
        }))
        .unwrap_err();
        assert_eq!(err.validation_path, "params_schema.properties.payload.type");
    }

    #[test]
    fn registry_rejects_outline_references_missing_host_tools_and_subagents() {
        let recipe: Arc<dyn TaskRecipe> = Arc::new(OneCallRecipe);
        let error = validate_recipe_registry(
            &[Arc::clone(&recipe)],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .unwrap_err();
        assert_eq!(
            error.validation_path,
            "outline[0].host_tools.yt_list_playlist"
        );

        struct AgentRecipe;
        impl TaskRecipe for AgentRecipe {
            fn metadata(&self) -> RecipeMetadata {
                let mut metadata = OneCallRecipe.metadata();
                metadata.outline = vec![RecipeStepOutline {
                    title: "Classify".into(),
                    kind: RecipeStepKind::Agent,
                    repeat: false,
                    host_tools: vec![],
                    sub_agents: vec!["video_classifier".into()],
                }];
                metadata
            }
            fn validate_params(&self, params: &Value) -> Result<Value, RecipeValidationError> {
                OneCallRecipe.validate_params(params)
            }
            fn next(&self, params: &Value, transcript: &[ModelMessage]) -> RecipeDecision {
                OneCallRecipe.next(params, transcript)
            }
            fn progress(&self, params: &Value, transcript: &[ModelMessage]) -> RecipeProgress {
                OneCallRecipe.progress(params, transcript)
            }
        }
        let error = validate_recipe_registry(
            &[Arc::new(AgentRecipe)],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .unwrap_err();
        assert_eq!(
            error.validation_path,
            "outline[0].sub_agents.video_classifier"
        );
    }
}
