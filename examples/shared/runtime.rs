//! Khởi tạo đầy đủ hệ thống Dual-Flow (`daily_orch` + `wiki_orch`) + `WikiEngine` + `TaskManager`
//! trên cùng 1 thư mục `WorkspaceLayout` dùng chung.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use llm_wiki::prompts::WikiTemplateId;
use llm_wiki::{AdaptiveReadBudget, WikiEngine};
use orchestrator_agent_rs::{
    AgentDefinition, AgentMode, LlmBackend, LlmConfig, LlmProviderConfig, Orchestrator,
    OrchestratorConfig, ProviderAuth, SecretString,
};
use orchestrator_flow_rs::{
    SharedPdfState, TaskConfig, TaskManager, TaskRuntimeConfig, WikiAutoFlowBinding,
    WorkspaceLayout, WorkspaceToolKit, YoutubeCollectorEnv,
};
use tokio::sync::Notify;

use crate::shared::env::{load_llm_credentials, resolve_shared_layout, ExampleResult};
use crate::shared::prompts::write_prompts_and_skills;

#[derive(Debug, Clone)]
pub struct HomelabBootOptions {
    pub custom_workspace_dir: Option<PathBuf>,
    pub custom_env_file: Option<PathBuf>,
    pub model_override: Option<String>,
    pub langs: String,
    pub limit: usize,
}

impl Default for HomelabBootOptions {
    fn default() -> Self {
        Self {
            custom_workspace_dir: None,
            custom_env_file: None,
            model_override: None,
            langs: "en,vi".to_string(),
            limit: usize::MAX,
        }
    }
}

#[allow(dead_code)]
pub struct HomelabExampleApp {
    pub layout: WorkspaceLayout,
    pub engine: Arc<WikiEngine>,
    pub orchestrator: Arc<Orchestrator>,
    pub task_mgr: Arc<TaskManager>,
    pub autoflow_notify: Arc<Notify>,
    pub pdf_state: Arc<SharedPdfState>,
    pub yt_env: Arc<YoutubeCollectorEnv>,
    pub model: String,
}

impl HomelabExampleApp {
    #[allow(dead_code)]
    pub async fn boot(
        custom_workspace_dir: Option<PathBuf>,
        custom_env_file: Option<&Path>,
    ) -> ExampleResult<Self> {
        Self::boot_with_options(HomelabBootOptions {
            custom_workspace_dir,
            custom_env_file: custom_env_file.map(PathBuf::from),
            ..Default::default()
        })
        .await
    }

    pub async fn boot_with_options(opts: HomelabBootOptions) -> ExampleResult<Self> {
        let layout = resolve_shared_layout(opts.custom_workspace_dir)?;
        write_prompts_and_skills(&layout)?;
        let creds = load_llm_credentials(opts.custom_env_file.as_deref())?;
        let model = opts.model_override.unwrap_or_else(|| creds.model.clone());
        let work_dir = layout.root_dir().join("work");
        std::fs::create_dir_all(&work_dir)?;

        println!("==================================================================");
        println!(
            "🏠 HOMELAB SHARED WORKSPACE: {}",
            layout.root_dir().display()
        );
        println!(
            "   ├─ Wiki Vault Dir     : {}",
            layout.wiki_vault_dir().display()
        );
        println!(
            "   ├─ Tasks SQLite Dir   : {}",
            layout.tasks_store_dir().display()
        );
        println!(
            "   ├─ Orchestrator Dir   : {}",
            layout.orchestrator_store_dir().display()
        );
        println!(
            "   ├─ Reports Dir        : {}",
            layout.reports_dir().display()
        );
        println!("   └─ Active LLM Model   : {model}");
        println!("==================================================================");

        // 1. Mở WikiEngine dùng chung trên `layout.wiki_vault_dir()`
        let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir())?);
        engine.apply_template(WikiTemplateId::Research)?;
        let autoflow_notify = Arc::new(Notify::new());

        let context_limit_tokens: usize = std::env::var("WIKI_CONTEXT_LIMIT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200_000);
        let max_output_tokens: u32 = std::env::var("WIKI_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16_384);
        let read_budget = AdaptiveReadBudget::for_context_limit(context_limit_tokens);

        let toolkit =
            WorkspaceToolKit::from_layout(&layout, engine.clone(), autoflow_notify.clone())
                .with_read_budget(read_budget)
                .with_youtube_options(opts.langs, opts.limit);
        let pdf_state = toolkit.pdf_state.clone();
        let yt_env = toolkit.yt_env.clone();

        // 2. PHA 1: Prepare `TaskManager` từ `WorkspaceLayout`
        let runtime_cfg = TaskRuntimeConfig {
            scheduler_tick: Duration::from_millis(200),
            autoflow_poll_interval: Duration::from_secs(5),
            event_capacity: 4096,
            ..Default::default()
        };
        let task_cfg = TaskConfig::from_layout(&layout)
            .with_default_agent("daily_root")
            .with_runtime(runtime_cfg);
        let task_init = TaskManager::prepare(task_cfg).await?;

        let llm_cfg = LlmConfig {
            providers: vec![LlmProviderConfig {
                id: "live".to_string(),
                backend: LlmBackend::OpenAiCompatible,
                endpoint: Some(creds.base_url.clone()),
                auth: ProviderAuth::ApiKey(SecretString::new(creds.api_key.clone())),
                request_timeout: Duration::from_secs(240),
            }],
        };

        let prompts = layout.prompts_dir();

        // 3. Xây dựng 1 instance `Orchestrator` hợp nhất duy nhất (`orchestrator.db`):
        //    Phân tách cứng 2 luồng `daily_root` và `wiki_root` thông qua `with_allowed_subagents` + `with_allowed_tools`.
        let agents = vec![
            // === Luồng Tương tác & Tác vụ Hàng ngày (`daily_root`) ===
            AgentDefinition::new("daily_root", "live", &model)
                .with_mode(AgentMode::Primary)
                .with_description(
                    "Interactive & background daily assistant: researches Wiki, creates reports, stages knowledge to sources/, and manages reminders.",
                )
                .with_prompt_path(prompts.join("daily_root.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools(["subagent"])
                .with_allowed_subagents(["wiki_researcher", "report_stager", "reminder_planner"])
                .with_steps(15),
            AgentDefinition::new("wiki_researcher", "live", &model)
                .with_mode(AgentMode::Subagent)
                .with_description("Read-only specialist for querying and reading the LLM Wiki.")
                .with_prompt_path(prompts.join("wiki_researcher.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools([
                    "skill",
                    "wiki_tree",
                    "wiki_query",
                    "wiki_get",
                    "wiki_multi_get",
                    "wiki_metadata",
                    "wiki_status",
                    "wiki_graph_neighbors",
                ])
                .with_allowed_skills(["wiki-query"])
                .with_steps(12),
            AgentDefinition::new("report_stager", "live", &model)
                .with_mode(AgentMode::Subagent)
                .with_description(
                    "Synthesizes discussions into sources/<domain>/<topic>/ via add_to_sources or writes standalone reports via markdown_create.",
                )
                .with_prompt_path(prompts.join("report_stager.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools([
                    "skill",
                    "wiki_tree",
                    "wiki_get",
                    "add_to_sources",
                    "markdown_create",
                    "markdown_read",
                ])
                .with_allowed_skills(["wiki-taxonomy", "daily-synthesis-staging"])
                .with_steps(12),
            AgentDefinition::new("reminder_planner", "live", &model)
                .with_mode(AgentMode::Subagent)
                .with_description("Creates, lists, completes, and deletes user reminders and scheduled tasks.")
                .with_prompt_path(prompts.join("reminder_planner.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools([
                    "skill",
                    "reminder_create",
                    "reminder_list",
                    "reminder_complete",
                    "reminder_delete",
                ])
                .with_allowed_skills(["daily-task-reminder"])
                .with_steps(10),
            // === Luồng Biên dịch & Kiểm toán Wiki (`wiki_root`) ===
            AgentDefinition::new("wiki_root", "live", &model)
                .with_mode(AgentMode::Primary)
                .with_description(
                    "Background Wiki pipeline coordinator: collects external sources (e.g. YouTube playlists), ingests new files from sources/, updates/patches Wiki pages, synthesizes topics, and runs wiki_lint.",
                )
                .with_prompt_path(prompts.join("wiki_root.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools(["subagent"])
                .with_allowed_subagents(["source_collector", "wiki_ingestor", "wiki_curator"])
                .with_steps(40),
            AgentDefinition::new("source_collector", "live", &model)
                .with_mode(AgentMode::Subagent)
                .with_description(
                    "Inspects external sources (e.g. YouTube playlist or PDF via anydoc), extracts detailed transcripts/markdown/metadata, classifies topic hierarchy (<domain>/<topic>), and saves raw Markdown files into sources/<domain>/<topic>/<slug>.md.",
                )
                .with_prompt_path(prompts.join("source_collector.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools([
                    "skill",
                    "anydoc_inspect_pdf",
                    "anydoc_save_to_sources",
                    "yt_list_playlist",
                    "yt_fetch_transcript",
                    "wiki_tree",
                ])
                .with_allowed_skills(["wiki-taxonomy"])
                .with_steps(35),
            AgentDefinition::new("wiki_ingestor", "live", &model)
                .with_mode(AgentMode::Subagent)
                .with_description("Compiles raw sources into wiki/sources/ and creates/patches entity & concept pages.")
                .with_prompt_path(prompts.join("wiki_ingestor.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools([
                    "skill",
                    "wiki_tree",
                    "wiki_get",
                    "wiki_multi_get",
                    "wiki_query",
                    "wiki_metadata",
                    "wiki_ingest_source",
                    "wiki_write_file",
                    "wiki_patch_file",
                ])
                .with_allowed_skills(["wiki-taxonomy", "wiki-ingest"])
                .with_steps(35),
            AgentDefinition::new("wiki_curator", "live", &model)
                .with_mode(AgentMode::Subagent)
                .with_description("Creates topic syntheses and repairs structural issues via wiki_lint.")
                .with_prompt_path(prompts.join("wiki_curator.md"))
                .with_context_limit_tokens(context_limit_tokens)
                .with_max_output_tokens(max_output_tokens)
                .with_allowed_tools([
                    "skill",
                    "wiki_tree",
                    "wiki_get",
                    "wiki_multi_get",
                    "wiki_query",
                    "wiki_file_synthesis",
                    "wiki_write_file",
                    "wiki_patch_file",
                    "wiki_delete_file",
                    "wiki_lint",
                    "wiki_schema",
                ])
                .with_allowed_skills(["wiki-taxonomy", "wiki-query", "wiki-lint"])
                .with_steps(35),
        ];

        let mut orch_cfg =
            OrchestratorConfig::new(llm_cfg, agents, layout.orchestrator_store_dir())
                .with_skills_dir(layout.skills_dir())
                .with_project_instructions(engine.config().schema_path());
        orch_cfg.runtime.snapshot_refresh = Duration::from_secs(1);
        orch_cfg.runtime.context_limit_tokens = context_limit_tokens;
        orch_cfg.runtime.compaction_threshold_percent = 85;
        orch_cfg.runtime.compaction_keep_recent = 6;
        orch_cfg.runtime.tool_output_cap_bytes =
            (read_budget.max_window_chars * 3 / 2).max(24 * 1024);

        let mut builder = Orchestrator::builder(orch_cfg);
        for tool in toolkit.all_host_tools(&task_init) {
            builder = builder.register_tool(tool);
        }
        let orchestrator = Arc::new(builder.build().await?);

        // 4. PHA 2: Gắn `Orchestrator` hợp nhất duy nhất + `WikiAutoFlowBinding` và start `TaskManager`
        let task_mgr = task_init
            .with_orchestrator(orchestrator.clone())
            .with_wiki_autoflow(WikiAutoFlowBinding::for_agent(
                engine.clone(),
                autoflow_notify.clone(),
                "wiki_root",
            ))
            .start()
            .await?;

        Ok(Self {
            layout,
            engine,
            orchestrator,
            task_mgr,
            autoflow_notify,
            pdf_state,
            yt_env,
            model,
        })
    }

    pub async fn shutdown(self) -> ExampleResult<()> {
        self.task_mgr.shutdown().await?;
        self.orchestrator.shutdown().await?;
        Ok(())
    }
}
