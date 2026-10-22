//! `WikiAutoFlowBinding` & Worker quản lý tiến trình tự động biên dịch/kiểm toán Wiki (ADR-0003).
//!
//! Khi nhận tín hiệu `autoflow_notify` (từ `add_to_sources` của Flow 2 hoặc khi nạp file mới vào `sources/`),
//! Worker gọi `WikiEngine::build_next_autoflow_prompt(None)` và tạo một `Task` chính thức
//! (`source = TaskSource::Autoflow`) để người dùng theo dõi trực tiếp trên UI View Task.

use std::sync::Arc;

use llm_wiki::WikiEngine;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::config::DEFAULT_FLOW_ID;
use crate::runtime::SharedTaskCore;
use crate::types::{CreateTaskRequest, TaskScope, TaskSource};

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

            // Chạy tối đa 4 chặng liên tiếp cho mỗi lần kích hoạt:
            // Chặng 1+: `[AUTO-FLOW: NEW SOURCES DETECTED]` -> Chặng cuối: `[AUTO-FLOW: WIKI LINT]`
            let mut stage_count = 0usize;
            while stage_count < 4 {
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
                    Ok(None) => break,
                    Err(e) => {
                        warn!("autoflow build_next_autoflow_prompt error: {e}");
                        break;
                    }
                };

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
                        loop {
                            let already_done = core
                                .store
                                .get_task_unscoped(&task.id)
                                .await
                                .ok()
                                .flatten()
                                .is_none_or(|t| t.status.is_terminal());
                            if already_done {
                                break;
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
