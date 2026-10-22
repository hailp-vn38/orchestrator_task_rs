//! `WikiAutoFlowBinding` & Worker quản lý tiến trình tự động biên dịch/kiểm toán Wiki (ADR-0003).
//!
//! Khi nhận tín hiệu `autoflow_notify` (từ `add_to_sources` của Flow 2 hoặc khi nạp file mới vào `sources/`),
//! Worker gọi `WikiEngine::build_next_autoflow_prompt(None)` và tạo một `Task` chính thức
//! (`source = TaskSource::Autoflow`) để người dùng theo dõi trực tiếp trên UI View Task.
//!
//! ### Cơ chế Bảo vệ Học từ Javis OS Self-Learning:
//! 1. **Vault Change Signature (`AutoflowVaultSignature`)**: Tính chữ ký trạng thái xác định của Vault
//!    (`sources_count`, `wiki_count`, `uningested_sources`, `unsynthesized_topics`, `broken_links`,
//!    `orphan_wiki_pages`, `missing_frontmatter_pages`). Nếu chữ ký không đổi kể từ lần chạy Auto-Flow
//!    gần nhất, bỏ qua không sinh Task LLM trùng lặp.
//! 2. **Anti-Stall Guard**: Trong chu kỳ đa chặng (`stage_count < MAX_AUTOFLOW_STAGES_PER_CYCLE`), nếu một chặng Auto-Flow kết thúc
//!    nhưng không làm thay đổi chữ ký trạng thái của Vault (hoặc Task bị `Failed`/`Cancelled`), dừng ngay
//!    chu kỳ thay vì lặp lại cùng một prompt gây tốn token.
//! 3. **Anti-Amplification**: Tín hiệu `notify` tích tụ trong lúc Auto-Flow đang chạy sẽ được đối chiếu
//!    với `last_attempted_signature` để không tự kích hoạt vòng lặp kiểm toán vô tận khi Vault không đổi.

use std::sync::Arc;

use llm_wiki::WikiEngine;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::config::DEFAULT_FLOW_ID;
use crate::runtime::SharedTaskCore;
use crate::types::{CreateTaskRequest, TaskScope, TaskSource, TaskStatus};

/// Số chặng tối đa được phép chạy liên tiếp trong một chu kỳ Auto-Flow khi mỗi chặng
/// đều tạo ra tiến độ thực chất (`post_sig != pre_sig`).
const MAX_AUTOFLOW_STAGES_PER_CYCLE: usize = 16;

/// Chữ ký trạng thái xác định (deterministic fingerprint) của Wiki Vault tại một thời điểm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoflowVaultSignature {
    pub sources_count: usize,
    pub wiki_count: usize,
    pub total_chunks: usize,
    pub total_graph_links: usize,
    pub uningested_sources: Vec<String>,
    pub unsynthesized_topics: Vec<String>,
    pub broken_links: Vec<(String, String, usize)>,
    pub orphan_wiki_pages: Vec<String>,
    pub missing_frontmatter_pages: Vec<String>,
    pub is_healthy: bool,
}

impl AutoflowVaultSignature {
    /// Thu thập chữ ký trạng thái hiện tại từ `WikiEngine`.
    pub(crate) fn capture(engine: &WikiEngine) -> Option<Self> {
        let status = engine.status().ok()?;
        let lint = engine.lint_wiki().ok()?;
        let mut unsynthesized_topics = engine.unsynthesized_topics().unwrap_or_default();
        unsynthesized_topics.sort();

        let mut uningested_sources: Vec<String> = lint
            .uningested_sources
            .iter()
            .map(|s| s.source_rel_path.clone())
            .collect();
        uningested_sources.sort();

        let mut broken_links: Vec<(String, String, usize)> = lint
            .broken_links
            .iter()
            .map(|b| {
                (
                    b.source_rel_path.clone(),
                    b.raw_target.clone(),
                    b.line_number,
                )
            })
            .collect();
        broken_links.sort();

        let mut orphan_wiki_pages = lint.orphan_wiki_pages.clone();
        orphan_wiki_pages.sort();

        let mut missing_frontmatter_pages = lint.missing_frontmatter_pages.clone();
        missing_frontmatter_pages.sort();

        Some(Self {
            sources_count: status.sources_count,
            wiki_count: status.wiki_count,
            total_chunks: status.total_chunks,
            total_graph_links: status.total_graph_links,
            uningested_sources,
            unsynthesized_topics,
            broken_links,
            orphan_wiki_pages,
            missing_frontmatter_pages,
            is_healthy: lint.is_healthy,
        })
    }
}

/// Cấu hình gắn kết `WikiEngine` và tín hiệu `autoflow_notify` vào `TaskManager`.
#[derive(Clone)]
pub struct WikiAutoFlowBinding {
    pub engine: Arc<WikiEngine>,
    pub notify: Arc<Notify>,
    pub flow_id: String,
    pub agent_id: String,
    pub scope: Option<TaskScope>,
}

impl WikiAutoFlowBinding {
    pub fn new(
        engine: Arc<WikiEngine>,
        notify: Arc<Notify>,
        flow_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            engine,
            notify,
            flow_id: flow_id.into(),
            agent_id: agent_id.into(),
            scope: None,
        }
    }

    /// Tạo cấu hình Auto-Flow cho kiến trúc 1 `Orchestrator` hợp nhất (`flow_id = "default"`).
    pub fn for_agent(
        engine: Arc<WikiEngine>,
        notify: Arc<Notify>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self::new(engine, notify, DEFAULT_FLOW_ID, agent_id)
    }

    pub fn with_scope(mut self, scope: TaskScope) -> Self {
        self.scope = Some(scope);
        self
    }
}

pub(crate) fn spawn_autoflow_loop(
    core: Arc<SharedTaskCore>,
    binding: WikiAutoFlowBinding,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let poll_interval = core.config.runtime.autoflow_poll_interval;
        let scope = binding
            .scope
            .clone()
            .unwrap_or_else(|| core.config.default_scope.clone());
        let target_key = format!("{}/{}", binding.flow_id, binding.agent_id);
        let mut last_attempted_signature: Option<AutoflowVaultSignature> = None;

        loop {
            if core.shutdown.is_cancelled() {
                break;
            }

            let notified = tokio::select! {
                biased;
                () = core.shutdown.cancelled() => break,
                () = binding.notify.notified() => true,
                () = tokio::time::sleep(poll_interval) => false,
            };

            // Nếu chỉ là nhịp poll định kỳ (không phải tín hiệu `autoflow_notify` trực tiếp từ `add_to_sources`),
            // chỉ khởi chạy chu kỳ Auto-Flow khi thực sự phát hiện file mới chưa ingest trong `sources/`.
            if !notified {
                let has_new_sources = binding
                    .engine
                    .lint_wiki()
                    .map(|l| !l.uningested_sources.is_empty())
                    .unwrap_or(false);
                if !has_new_sources {
                    continue;
                }
            }

            // Chạy liên tiếp các chặng chừng nào mỗi chặng đều làm thay đổi trạng thái Vault:
            // Chặng 1+: `[AUTO-FLOW: NEW SOURCES DETECTED]` -> Chặng cuối: `[AUTO-FLOW: WIKI LINT]`
            let mut stage_count = 0usize;
            while stage_count < MAX_AUTOFLOW_STAGES_PER_CYCLE {
                if core.shutdown.is_cancelled() {
                    break;
                }

                let active_done = core.autoflow_done_wake.notified();
                tokio::pin!(active_done);
                active_done.as_mut().enable();

                // Nếu đang có Task đang chạy trên agent/flow của Auto-Flow (ví dụ `source_collector`
                // đang tải playlist hoặc một Auto-Flow Task khác đang chạy), đợi nó kết thúc trước
                match core
                    .store
                    .has_active_autoflow_task(&binding.flow_id, &binding.agent_id)
                    .await
                {
                    Ok(true) => {
                        tokio::select! {
                            biased;
                            () = core.shutdown.cancelled() => break,
                            () = &mut active_done => continue,
                            () = tokio::time::sleep(poll_interval) => continue,
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!("autoflow could not check active tasks: {e}");
                        break;
                    }
                }

                let next_prompt = match binding.engine.build_next_autoflow_prompt(None) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        last_attempted_signature = AutoflowVaultSignature::capture(&binding.engine);
                        break;
                    }
                    Err(e) => {
                        warn!("autoflow build_next_autoflow_prompt error: {e}");
                        break;
                    }
                };

                // Change Signature Guard: Nếu chữ ký trạng thái của Vault hoàn toàn không đổi
                // so với lần vừa chạy Auto-Flow gần nhất, bỏ qua để tránh vòng lặp tự khuếch đại
                // hoặc gọi LLM kiểm toán trùng lặp trên cùng một trạng thái dữ liệu.
                let pre_sig = AutoflowVaultSignature::capture(&binding.engine);
                if let (Some(ref current), Some(ref last)) = (&pre_sig, &last_attempted_signature) {
                    if current == last {
                        debug!(
                            "autoflow skipped: vault signature unchanged since last autoflow attempt"
                        );
                        break;
                    }
                }
                last_attempted_signature = pre_sig.clone();

                let title = if next_prompt.starts_with("[AUTO-FLOW: NEW SOURCES DETECTED]") {
                    "Auto-Flow: Biên dịch nguồn mới vào Wiki".to_string()
                } else {
                    "Auto-Flow: Tổng hợp Synthesis & Kiểm toán Wiki Lint".to_string()
                };

                let done_notified = core.autoflow_done_wake.notified();
                tokio::pin!(done_notified);
                done_notified.as_mut().enable();

                let req = CreateTaskRequest {
                    title,
                    prompt: next_prompt,
                    target_agent: Some(target_key.clone()),
                    source: TaskSource::Autoflow,
                    schedule_id: None,
                    existing_session_id: None,
                    related_wiki_links: Vec::new(),
                };

                match core.create_and_start_task(&scope, req).await {
                    Ok(task) => {
                        stage_count += 1;
                        debug!(task_id = %task.id, "spawned managed autoflow task");
                        // Đợi Watcher báo Task Auto-Flow này thực sự đạt trạng thái terminal
                        let mut terminal_status = None;
                        loop {
                            let current_status = core
                                .store
                                .get_task_unscoped(&task.id)
                                .await
                                .ok()
                                .flatten()
                                .map(|t| t.status);
                            match current_status {
                                Some(st) if st.is_terminal() => {
                                    terminal_status = Some(st);
                                    break;
                                }
                                None => break,
                                _ => {}
                            }
                            tokio::select! {
                                biased;
                                () = core.shutdown.cancelled() => break,
                                () = &mut done_notified => {
                                    done_notified.set(core.autoflow_done_wake.notified());
                                    done_notified.as_mut().enable();
                                }
                                () = tokio::time::sleep(poll_interval) => {}
                            }
                        }
                        let _ = binding.engine.embed_pending(false);
                        let _ = binding.engine.rebuild_or_sync_index_md();

                        // Nếu Task bị Failed hoặc Cancelled, không tự động chạy tiếp chặng kế tiếp
                        if matches!(
                            terminal_status,
                            Some(TaskStatus::Failed | TaskStatus::Cancelled)
                        ) {
                            debug!(
                                task_id = %task.id,
                                status = ?terminal_status,
                                "autoflow stage did not complete cleanly; stopping cycle"
                            );
                            break;
                        }

                        // Anti-Stall Guard: Cập nhật lại chữ ký sau khi rebuild index;
                        // nếu chặng vừa rồi không làm thay đổi trạng thái Vault, ghi nhận rõ vào
                        // `progress_summary` và dừng ngay chu kỳ.
                        let post_sig = AutoflowVaultSignature::capture(&binding.engine);
                        if pre_sig.is_some() && post_sig == pre_sig {
                            let _ = core
                                .store
                                .update_task_summary(
                                    &task.id,
                                    "Auto-Flow stopped: no Vault changes produced in this stage",
                                )
                                .await;
                            debug!(
                                task_id = %task.id,
                                "autoflow stage finished with no vault changes; stopping cycle"
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("failed to spawn autoflow task: {e}");
                        break;
                    }
                }
            }
        }
    })
}
