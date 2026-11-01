//! Thu thập playlist nhiều video và Auto-Flow nhiều nguồn: không trùng, không ghi đè, làm tiếp được
//! sau restart, và một nguồn lỗi không chặn hàng đợi. `yt-dlp` là script giả.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use orchestrator_flow_rs::{ExistingReason, SaveSourceOutcome, YoutubeCollectorEnv};

use super::*;

const PLAYLIST_URL: &str = "https://www.youtube.com/playlist?list=PL1";

/// `PL1` gồm vid-a, vid-b, vid-nosub, vid-reup. `vid-nosub` không có phụ đề; `vid-reup` có nội dung
/// giống `vid-a`; `vid-hang` treo tới khi bị kill.
const FAKE_YTDLP: &str = r#"#!/bin/sh
for last; do :; done
case "$*" in
  *--flat-playlist*)
    printf 'vid-a\tIntroduction\nvid-b\tSecond Lesson\nvid-nosub\tNo Subtitles\nvid-reup\tIntroduction Again\n'
    exit 0 ;;
esac
id="${last##*v=}"
[ "$id" = "vid-hang" ] && exec sleep 60
[ "$id" = "vid-nosub" ] && exit 0
out=""
prev=""
for a in "$@"; do
  [ "$prev" = "-o" ] && out="$a"
  prev="$a"
done
dir="$(dirname "$out")"
body="$id"
[ "$id" = "vid-reup" ] && body="vid-a"
printf 'WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nTranscript body of %s\n' "$body" > "$dir/$id.en.vtt"
"#;

fn fake_ytdlp(dir: &Path) -> PathBuf {
    let path = dir.join("fake-yt-dlp");
    std::fs::write(&path, FAKE_YTDLP).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Vault {
    dir: tempfile::TempDir,
    engine: Arc<WikiEngine>,
    ytdlp: PathBuf,
}

impl Vault {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(WikiEngine::open(dir.path().join("vault")).unwrap());
        let ytdlp = fake_ytdlp(dir.path());
        Self { dir, engine, ytdlp }
    }

    /// Một `env` mới mô phỏng process vừa khởi động lại: mất danh sách video và cache trong RAM.
    fn fresh_env(&self) -> YoutubeCollectorEnv {
        YoutubeCollectorEnv::new(
            self.engine.clone(),
            Arc::new(Notify::new()),
            self.dir.path().join("work"),
        )
        .with_ytdlp(&self.ytdlp, Duration::from_secs(10))
    }

    fn source_files(&self) -> Vec<String> {
        let mut files = Vec::new();
        let mut stack = vec![self.engine.config().sources_dir()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "md") {
                    files.push(path.file_name().unwrap().to_string_lossy().to_string());
                }
            }
        }
        files.sort();
        files
    }
}

async fn save(env: &YoutubeCollectorEnv, id: &str, title: &str, force: bool) -> SaveSourceOutcome {
    env.save_video_to_sources(id, "trading/smc", Some(title), None, vec![], force)
        .await
        .unwrap()
}

#[tokio::test]
async fn saving_a_video_again_writes_nothing_even_after_a_restart() {
    let vault = Vault::new();
    let SaveSourceOutcome::Written(first) = save(&vault.fresh_env(), "vid-a", "Intro", false).await
    else {
        panic!("first save must write");
    };

    // Tiêu đề khác và không có danh sách trong RAM: tên file sẽ khác nếu không nhận ra `video_id`.
    match save(&vault.fresh_env(), "vid-a", "Renamed title", false).await {
        SaveSourceOutcome::Existing { found, reason } => {
            assert_eq!(reason, ExistingReason::SameIdentity);
            assert_eq!(found.scoped_path, first.info.scoped_path);
        }
        other => panic!("expected Existing, got {other:?}"),
    }
    assert_eq!(vault.source_files().len(), 1);
}

#[tokio::test]
async fn same_file_stem_for_a_different_video_never_overwrites() {
    let vault = Vault::new();
    let env = vault.fresh_env();
    for id in ["vid-a", "vid-b"] {
        let saved = env
            .save_video_to_sources(
                id,
                "trading/smc",
                Some("Introduction"),
                Some("introduction"),
                vec![],
                false,
            )
            .await
            .unwrap();
        assert!(matches!(saved, SaveSourceOutcome::Written(_)));
    }
    assert_eq!(
        vault.source_files(),
        vec!["introduction-vid-b.md", "introduction.md"]
    );
    let first = std::fs::read_to_string(
        vault
            .engine
            .config()
            .sources_dir()
            .join("trading/smc/introduction.md"),
    )
    .unwrap();
    assert!(first.contains("Transcript body of vid-a"), "{first}");
}

#[tokio::test]
async fn a_reupload_with_identical_content_is_reported_as_duplicate() {
    let vault = Vault::new();
    let env = vault.fresh_env();
    let SaveSourceOutcome::Written(original) = save(&env, "vid-a", "Original", false).await else {
        panic!("original must be written");
    };
    match save(&env, "vid-reup", "Reupload", false).await {
        SaveSourceOutcome::Existing { found, reason } => {
            assert_eq!(reason, ExistingReason::SameContent);
            assert_eq!(found.scoped_path, original.info.scoped_path);
        }
        other => panic!("expected duplicate, got {other:?}"),
    }
    assert_eq!(vault.source_files().len(), 1);
}

#[tokio::test]
async fn force_rewrites_the_same_path() {
    let vault = Vault::new();
    let env = vault.fresh_env();
    let SaveSourceOutcome::Written(first) = save(&env, "vid-a", "Intro", false).await else {
        panic!("first save must write");
    };
    let SaveSourceOutcome::Written(again) = save(&env, "vid-a", "New title", true).await else {
        panic!("force must write");
    };
    assert_eq!(again.info.scoped_path, first.info.scoped_path);
    assert_eq!(vault.source_files().len(), 1);
}

#[tokio::test]
async fn failures_are_explicit_and_a_hung_ytdlp_is_killed() {
    let vault = Vault::new();
    let env = YoutubeCollectorEnv::new(
        vault.engine.clone(),
        Arc::new(Notify::new()),
        vault.dir.path().join("work"),
    )
    .with_ytdlp(&vault.ytdlp, Duration::from_millis(300));

    let no_subs = env.get_or_fetch_transcript("vid-nosub").await.unwrap_err();
    assert_eq!(no_subs.code, "NO_TRANSCRIPT");

    let started = std::time::Instant::now();
    let hung = env.get_or_fetch_transcript("vid-hang").await.unwrap_err();
    assert_eq!(hung.code, "YTDLP_TIMEOUT");
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ───────────────────────── Qua Orchestrator: sổ theo dõi và Auto-Flow ─────────────────────────

struct Stack {
    _dir: tempfile::TempDir,
    engine: Arc<WikiEngine>,
    notify: Arc<Notify>,
    gw: Arc<ScriptedGateway>,
    mgr: Arc<TaskManager>,
}

async fn stack(runtime: TaskRuntimeConfig, daily_tools: &[&str]) -> Stack {
    stack_with_tool(runtime, daily_tools, None, true).await
}

async fn stack_with_tool(
    runtime: TaskRuntimeConfig,
    daily_tools: &[&str],
    extra_tool: Option<(Arc<dyn orchestrator_agent_rs::HostTool>, Duration)>,
    autoflow: bool,
) -> Stack {
    let dir = tempfile::tempdir().unwrap();
    let layout = WorkspaceLayout::new(dir.path().join("workspace"));
    layout.ensure_dirs().unwrap();
    let prompt_path = layout.prompts_dir().join("agent.md");
    std::fs::write(&prompt_path, "You are a test agent.").unwrap();
    let engine = Arc::new(WikiEngine::open(layout.wiki_vault_dir()).unwrap());
    let notify = Arc::new(Notify::new());
    let init = TaskManager::prepare(
        TaskConfig::from_layout(&layout)
            .with_default_agent("daily_root")
            .with_runtime(runtime),
    )
    .await
    .unwrap();
    let mut toolkit = WorkspaceToolKit::from_layout(&layout, engine.clone(), notify.clone());
    toolkit.yt_env = Arc::new(
        YoutubeCollectorEnv::new(engine.clone(), notify.clone(), dir.path().join("yt"))
            .with_ytdlp(fake_ytdlp(dir.path()), Duration::from_secs(10)),
    );

    let gw = ScriptedGateway::new();
    let daily = AgentDefinition::new("daily_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Daily root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(daily_tools.iter().copied())
        .with_steps(8);
    let wiki = AgentDefinition::new("wiki_root", "test", "gpt-test")
        .with_mode(AgentMode::Primary)
        .with_description("Wiki root")
        .with_prompt_path(&prompt_path)
        .with_allowed_tools(["wiki_ingest_source"])
        .with_steps(8);
    let mut orch_cfg = OrchestratorConfig::new(
        test_llm_config(),
        vec![daily, wiki],
        layout.orchestrator_store_dir(),
    );
    if let Some((_, tool_timeout)) = &extra_tool {
        orch_cfg.runtime.tool_timeout = *tool_timeout;
    }
    let mut builder = Orchestrator::builder(orch_cfg).with_gateway(gw.clone());
    for tool in toolkit.all_host_tools(&init) {
        builder = builder.register_tool(tool);
    }
    if let Some((tool, _)) = extra_tool {
        builder = builder.register_tool(tool);
    }
    let orch = Arc::new(builder.build().await.unwrap());
    let mut init = init.with_orchestrator(orch);
    if autoflow {
        init = init.with_wiki_autoflow(WikiAutoFlowBinding::for_agent(
            engine.clone(),
            notify.clone(),
            "wiki_root",
        ));
    }
    let mgr = init.start().await.unwrap();
    Stack {
        _dir: dir,
        engine,
        notify,
        gw,
        mgr,
    }
}

fn fast_runtime() -> TaskRuntimeConfig {
    TaskRuntimeConfig {
        scheduler_tick: Duration::from_millis(40),
        autoflow_poll_interval: Duration::from_millis(50),
        ..Default::default()
    }
}

async fn wait_terminal(mgr: &TaskManager, task_id: &str) -> orchestrator_flow_rs::TaskView {
    for _ in 0..200 {
        let task = mgr.get_task(task_id).await.unwrap();
        if task.status.is_terminal() {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("task {task_id} did not finish");
}

async fn autoflow_tasks(mgr: &TaskManager) -> Vec<orchestrator_flow_rs::TaskView> {
    let filter = TaskFilter {
        agent_id: Some("wiki_root".to_string()),
        limit: Some(50),
        ..Default::default()
    };
    mgr.list_all_tasks(&filter).await.unwrap().items
}

fn write_source(engine: &WikiEngine, stem: &str) {
    engine
        .write_topic_source(WriteTopicSourceRequest {
            topic_folder: "trading/smc".to_string(),
            file_stem: Some(stem.to_string()),
            title: stem.to_string(),
            raw_body: format!("Body of {stem}."),
            body_heading: None,
            tags: vec![],
            extra_frontmatter: vec![],
            header_bullets: vec![],
            preview_chars: None,
        })
        .unwrap();
}

#[tokio::test]
async fn a_retried_job_learns_from_the_ledger_which_videos_remain() {
    // Không gắn Auto-Flow: `add_to_sources` sẽ đánh thức nó và tranh kịch bản của gateway.
    let s = stack_with_tool(
        fast_runtime(),
        &["yt_list_playlist", "add_to_sources", "collection_status"],
        None,
        false,
    )
    .await;

    // Lần chạy đầu: liệt kê, lưu vid-a, vid-nosub không có phụ đề, rồi dừng giữa chừng.
    s.gw.push_tool_calls(
        "",
        vec![("l1", "yt_list_playlist", json!({ "url": PLAYLIST_URL }))],
    );
    s.gw.push_tool_calls(
        "",
        vec![
            (
                "a1",
                "add_to_sources",
                json!({ "topic_folder": "trading/smc", "video_id": "vid-a" }),
            ),
            (
                "a2",
                "add_to_sources",
                json!({ "topic_folder": "trading/smc", "video_id": "vid-nosub" }),
            ),
            (
                "a2b",
                "add_to_sources",
                json!({ "topic_folder": "trading/smc", "video_id": "vid-reup" }),
            ),
        ],
    );
    s.gw.push_text("stopped early");
    let first = s
        .mgr
        .create_task(CreateTaskRequest::new("Collect", "collect playlist"))
        .await
        .unwrap();
    wait_terminal(&s.mgr, &first.id).await;

    // Job mới không có context cũ: chỉ đọc sổ theo dõi và thử lưu lại vid-a.
    s.gw.push_tool_calls(
        "",
        vec![(
            "s1",
            "collection_status",
            json!({ "playlist_url": PLAYLIST_URL }),
        )],
    );
    s.gw.push_tool_calls(
        "",
        vec![(
            "a3",
            "add_to_sources",
            json!({ "topic_folder": "trading/smc", "video_id": "vid-a" }),
        )],
    );
    s.gw.push_text("done");
    let retry = s
        .mgr
        .create_task(CreateTaskRequest::new("Collect", "collect playlist again"))
        .await
        .unwrap();
    wait_terminal(&s.mgr, &retry.id).await;

    let request_texts: Vec<String> =
        s.gw.agent_requests()
            .iter()
            .flat_map(|r| r.messages.iter().map(|m| m.text.clone()))
            .collect();
    let status = request_texts
        .iter()
        .find(|t| t.contains("NEXT:"))
        .expect("collection_status output reaches the model");
    assert!(
        status.contains("TOTAL: 4 | IN_SOURCES: 1 | DUPLICATE: 1 | REMAINING: 1 | NO_TRANSCRIPT: 1 | GAVE_UP: 0"),
        "{status}"
    );
    assert!(status.contains("video_id=vid-b"), "{status}");
    assert!(!status.contains("video_id=vid-a "), "{status}");
    assert!(
        request_texts
            .iter()
            .any(|t| t.contains("SOURCE_ALREADY_EXISTS")),
        "saving vid-a again must be a no-op the agent can see"
    );
    assert_eq!(s.engine.status().unwrap().sources_count, 1);
    s.mgr.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_task_stopped_at_the_step_limit_is_not_reported_as_completed() {
    let s = stack(fast_runtime(), &["collection_status"]).await;
    for i in 0..8 {
        s.gw.push_tool_calls(
            "",
            vec![(
                format!("c{i}").leak() as &str,
                "collection_status",
                json!({ "playlist_url": PLAYLIST_URL }),
            )],
        );
    }
    s.gw.push_text("progress: did some; remaining: the rest");
    let task = s
        .mgr
        .create_task(CreateTaskRequest::new("Collect", "collect"))
        .await
        .unwrap();
    let done = wait_terminal(&s.mgr, &task.id).await;
    assert_eq!(done.status, TaskStatus::Stopped);
    assert_eq!(done.error_code.as_deref(), Some("STEP_LIMIT_REACHED"));
    s.mgr.shutdown().await.unwrap();
}

#[tokio::test]
async fn autoflow_waits_for_a_running_collection_before_starting() {
    let s = stack(fast_runtime(), &[]).await;
    s.gw.push_delayed(
        Duration::from_millis(800),
        orchestrator_agent_rs::test_support::ModelResponse {
            text: "collected".to_string(),
            finish_reason: "end_turn".to_string(),
            tool_calls: Vec::new(),
            usage: None,
        },
    );
    let collecting = s
        .mgr
        .create_task(CreateTaskRequest::new("Collect", "collect"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    write_source(&s.engine, "first");
    s.notify.notify_one();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        autoflow_tasks(&s.mgr).await.is_empty(),
        "no ingest while collection is still writing sources"
    );

    wait_terminal(&s.mgr, &collecting.id).await;
    s.gw.push_text("ingest attempted");
    for _ in 0..80 {
        if !autoflow_tasks(&s.mgr).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(autoflow_tasks(&s.mgr).await.len(), 1);
    s.mgr.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_source_that_keeps_failing_is_quarantined_and_the_queue_moves_on() {
    let s = stack(
        TaskRuntimeConfig {
            autoflow_max_source_attempts: 2,
            autoflow_batch_size: 1,
            ..fast_runtime()
        },
        &[],
    )
    .await;
    // `poison` đứng trước theo đường dẫn nên luôn được chọn trước; hai chặng đầu không ingest được.
    write_source(&s.engine, "a-poison");
    write_source(&s.engine, "b-good");
    s.gw.push_text("could not ingest a-poison");
    s.gw.push_text("could not ingest a-poison again");
    s.gw.push_tool_calls(
        "",
        vec![(
            "ok",
            "wiki_ingest_source",
            json!({
                "source_rel_path": "trading/smc/b-good.md",
                "title": "Good",
                "one_line_summary": "Good summary.",
                "wiki_markdown_body": "Good body.",
                "tags": []
            }),
        )],
    );
    s.gw.push_text("ingested b-good");
    s.notify.notify_one();

    let poison = "sources/trading/smc/a-poison.md".to_string();
    for _ in 0..200 {
        let good_done = s
            .engine
            .lint_wiki()
            .unwrap()
            .uningested_sources
            .iter()
            .all(|u| u.source_rel_path != "sources/trading/smc/b-good.md");
        if good_done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        s.mgr.quarantined_sources().await.unwrap(),
        vec![poison.clone()]
    );
    let lint = s.engine.lint_wiki().unwrap();
    assert_eq!(lint.uningested_sources.len(), 1, "only the poison remains");
    let prompts: Vec<String> = autoflow_tasks(&s.mgr)
        .await
        .iter()
        .map(|t| t.prompt.clone())
        .collect();
    assert_eq!(
        prompts.iter().filter(|p| p.contains("a-poison.md")).count(),
        2,
        "tried exactly as many times as allowed"
    );

    assert!(s.mgr.release_quarantined_source(&poison).await.unwrap());
    assert!(s.mgr.quarantined_sources().await.unwrap().is_empty());

    // Giải phóng phải thực sự đưa nguồn trở lại hàng đợi dù chữ ký Vault không đổi.
    s.gw.push_text("third try");
    let mut retried = 0;
    for _ in 0..200 {
        retried = autoflow_tasks(&s.mgr)
            .await
            .iter()
            .filter(|t| t.prompt.contains("a-poison.md"))
            .count();
        if retried == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(retried, 3, "a released source is tried again");
    s.mgr.shutdown().await.unwrap();
}

struct HangingWrite;

#[async_trait::async_trait]
impl orchestrator_agent_rs::HostTool for HangingWrite {
    fn descriptor(&self) -> orchestrator_agent_rs::ToolDescriptor {
        orchestrator_agent_rs::ToolDescriptor {
            name: "hanging_write".to_string(),
            description: "A write that never reports back".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            effect: orchestrator_agent_rs::ToolEffect::Mutating,
        }
    }

    async fn execute(
        &self,
        _ctx: orchestrator_agent_rs::ToolContext,
        _args: serde_json::Value,
    ) -> Result<orchestrator_agent_rs::ToolOutcome, orchestrator_agent_rs::ToolError> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        unreachable!("the tool timeout fires first")
    }
}

#[tokio::test]
async fn an_operator_can_unstick_a_task_whose_write_has_an_unknown_outcome() {
    let s = stack_with_tool(
        fast_runtime(),
        &["hanging_write"],
        Some((Arc::new(HangingWrite), Duration::from_millis(100))),
        true,
    )
    .await;
    s.gw.push_tool_calls("", vec![("w1", "hanging_write", json!({}))]);
    let task = s
        .mgr
        .create_task(CreateTaskRequest::new("Write", "write something"))
        .await
        .unwrap();
    let stuck = wait_terminal(&s.mgr, &task.id).await;
    assert_eq!(stuck.status, TaskStatus::Interrupted);

    let detail = s.mgr.get_task_detail(&task.id).await.unwrap();
    let invocation = detail
        .root_agent
        .expect("root session")
        .invocations
        .into_iter()
        .find(|i| i.tool_name == "hanging_write")
        .expect("the stuck invocation is visible");
    assert_eq!(invocation.status, "interrupted");

    s.gw.push_text("continued after the operator's verdict");
    let reopened = s
        .mgr
        .resolve_stuck_tool(orchestrator_flow_rs::ResolveStuckTool {
            task_id: task.id.clone(),
            session_id: None,
            invocation_id: invocation.invocation_id,
            operator_id: "admin".to_string(),
            decision: orchestrator_flow_rs::StuckToolDecision::NoEffect {
                evidence: "checked the target: nothing was written".to_string(),
            },
        })
        .await
        .unwrap();
    assert_eq!(reopened.status, TaskStatus::Running);

    let done = wait_terminal(&s.mgr, &task.id).await;
    assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
    assert_eq!(
        done.output_preview.as_deref(),
        Some("continued after the operator's verdict")
    );
    s.mgr.shutdown().await.unwrap();
}

async fn wait_until_autoflow_settles(s: &Stack, expected_tasks: usize) {
    for _ in 0..200 {
        let tasks = autoflow_tasks(&s.mgr).await;
        if tasks.len() >= expected_tasks && tasks.iter().all(|t| t.status.is_terminal()) {
            tokio::time::sleep(Duration::from_millis(150)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("autoflow did not reach {expected_tasks} finished tasks");
}

#[tokio::test]
async fn a_provider_outage_quarantines_nothing() {
    let s = stack(
        TaskRuntimeConfig {
            autoflow_max_source_attempts: 1,
            ..fast_runtime()
        },
        &[],
    )
    .await;
    write_source(&s.engine, "a");
    write_source(&s.engine, "b");
    s.gw.push_error(orchestrator_agent_rs::test_support::GatewayError::NotConfigured);
    s.notify.notify_one();
    wait_until_autoflow_settles(&s, 1).await;

    assert!(s.mgr.quarantined_sources().await.unwrap().is_empty());
    assert_eq!(
        autoflow_tasks(&s.mgr).await.len(),
        1,
        "no hot retry loop while the provider is broken"
    );
    s.mgr.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_stage_blames_only_the_source_it_was_working_on() {
    let s = stack(
        TaskRuntimeConfig {
            autoflow_max_source_attempts: 1,
            autoflow_batch_size: 2,
            ..fast_runtime()
        },
        &[],
    )
    .await;
    write_source(&s.engine, "a");
    write_source(&s.engine, "b");
    s.gw.push_errors(
        4,
        orchestrator_agent_rs::test_support::GatewayError::ContextOverflow,
    );
    s.gw.push_text("b could not be ingested either");
    s.notify.notify_one();
    wait_until_autoflow_settles(&s, 2).await;

    let quarantined = s.mgr.quarantined_sources().await.unwrap();
    assert!(
        quarantined.contains(&"sources/trading/smc/a.md".to_string()),
        "{quarantined:?}"
    );
    let first_task = autoflow_tasks(&s.mgr)
        .await
        .into_iter()
        .find(|t| t.status == TaskStatus::Failed)
        .expect("the overflow fails the first stage");
    assert_eq!(first_task.error_code.as_deref(), Some("CONTEXT_LIMIT"));
    s.mgr.shutdown().await.unwrap();
}

#[tokio::test]
async fn rewriting_a_quarantined_source_releases_it() {
    let s = stack(
        TaskRuntimeConfig {
            autoflow_max_source_attempts: 1,
            ..fast_runtime()
        },
        &[],
    )
    .await;
    write_source(&s.engine, "poison");
    s.gw.push_text("could not ingest");
    s.notify.notify_one();
    wait_until_autoflow_settles(&s, 1).await;
    assert_eq!(s.mgr.quarantined_sources().await.unwrap().len(), 1);

    tokio::time::sleep(Duration::from_millis(30)).await;
    write_source(&s.engine, "poison");
    assert!(s.mgr.quarantined_sources().await.unwrap().is_empty());
    s.mgr.shutdown().await.unwrap();
}

#[tokio::test]
async fn force_on_markdown_keeps_the_path_and_never_overwrites_other_content() {
    let s = stack_with_tool(fast_runtime(), &["add_to_sources"], None, false).await;
    let body = "Nội dung đủ dài cho một bản tổng hợp, không chứa gì đặc biệt.";
    let save = |id: &'static str, title: &str, stem: &str, body: &str, force: bool| {
        (
            id,
            "add_to_sources",
            json!({
                "topic_folder": "trading/smc",
                "title": title,
                "file_stem": stem,
                "markdown_body": body,
                "provenance": "user",
                "force": force,
            }),
        )
    };
    s.gw.push_tool_calls(
        "",
        vec![
            save("m1", "Tên một", "note", body, false),
            // Cùng nội dung, tên khác, force: ghi lại đúng file cũ.
            save("m2", "Tên hai", "note-renamed", body, true),
            // Nội dung khác, cùng tên file, force: không được đè file cũ.
            save(
                "m3",
                "Tên ba",
                "note",
                "Một nội dung hoàn toàn khác với nội dung trước đó.",
                true,
            ),
        ],
    );
    s.gw.push_text("done");
    let task = s
        .mgr
        .create_task(CreateTaskRequest::new("Save", "save notes"))
        .await
        .unwrap();
    wait_terminal(&s.mgr, &task.id).await;

    assert_eq!(s.engine.status().unwrap().sources_count, 2);
    let original =
        std::fs::read_to_string(s.engine.config().sources_dir().join("trading/smc/note.md"))
            .unwrap();
    assert!(original.contains("đủ dài"), "{original}");
    s.mgr.shutdown().await.unwrap();
}
