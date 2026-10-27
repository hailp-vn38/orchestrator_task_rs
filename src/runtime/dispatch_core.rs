//! Logic điều phối lõi trên `Arc<SharedTaskCore>`, dùng chung cho `TaskManager`, `Scheduler`,
//! `AutoFlow` và các `HostTool`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use llm_wiki::CollectionScope;
use orchestrator_agent_rs::{ErrorCode as OrchErrorCode, PageRequest, RunStatus};

use crate::error::{TaskError, TaskResult};
use crate::runtime::{compute_next_cron_ms, lock, parse_due_at_ms, watcher, SharedTaskCore};
use crate::store::{new_id, now_ms, NewScheduleRow, NewTaskRow, TerminalUpdate};
use crate::types::{
    CreateScheduleRequest, CreateTaskRequest, FollowupStatus, FollowupView, ScheduleActionKind,
    ScheduleKind, ScheduleStatus, ScheduleView, TaskAgentMessageView, TaskArtifactCatalogEntry,
    TaskArtifactCatalogPage, TaskArtifactCatalogSummary, TaskArtifactDownload, TaskArtifactFilter,
    TaskArtifactKind, TaskArtifactRole, TaskArtifactView, TaskDetailView, TaskEvent, TaskScope,
    TaskSource, TaskStatus, TaskView, WikiDeleteOutcome,
};

impl SharedTaskCore {
    /// Tạo Task mới trên Flow tương ứng, gọi `Orchestrator::submit`, lưu vào `tasks.db`
    /// và khởi chạy Watcher theo dõi tiến độ.
    pub(crate) async fn create_and_start_task(
        self: &Arc<Self>,
        scope: &TaskScope,
        req: CreateTaskRequest,
    ) -> TaskResult<TaskView> {
        self.ensure_open()?;
        scope.validate()?;

        let title = req.title.trim();
        let prompt = req.prompt.trim();
        if title.is_empty() {
            return Err(TaskError::invalid_input("task title must not be empty"));
        }
        if prompt.is_empty() {
            return Err(TaskError::invalid_input("task prompt must not be empty"));
        }

        let (target, orch) = self.flows()?.resolve_target(req.target_agent.as_deref())?;
        let caller = scope.to_caller_context();

        let session_id = match req.existing_session_id {
            Some(ref sid) if !sid.trim().is_empty() => sid.trim().to_string(),
            _ => orch.create_session(&caller, &target.agent_id).await?,
        };

        let task_id = new_id();
        match orch.submit(&caller, &session_id, prompt.to_string()).await {
            Ok(accepted) => {
                let task = self
                    .store
                    .insert_task(NewTaskRow {
                        id: task_id.clone(),
                        scope: scope.clone(),
                        title: title.to_string(),
                        prompt: prompt.to_string(),
                        flow_id: target.flow_id,
                        agent_id: target.agent_id,
                        source: req.source,
                        schedule_id: req.schedule_id,
                        session_id,
                        latest_turn_id: Some(accepted.turn_id),
                        status: TaskStatus::Running,
                        progress_summary: Some("Starting agent turn...".to_string()),
                        related_wiki_links: req.related_wiki_links,
                    })
                    .await?;
                self.event_bus
                    .emit(TaskEvent::TaskCreated { task: task.clone() });
                watcher::ensure_task_watcher(Arc::clone(self), task.clone());
                Ok(task)
            }
            Err(err) if err.code() == OrchErrorCode::SessionBusy => {
                // Nếu tái sử dụng session đang bận, tạo task ở trạng thái 'queued' kèm followup chờ
                let task = self
                    .store
                    .insert_task(NewTaskRow {
                        id: task_id.clone(),
                        scope: scope.clone(),
                        title: title.to_string(),
                        prompt: prompt.to_string(),
                        flow_id: target.flow_id,
                        agent_id: target.agent_id,
                        source: req.source,
                        schedule_id: req.schedule_id,
                        session_id,
                        latest_turn_id: None,
                        status: TaskStatus::Queued,
                        progress_summary: Some(
                            "Queued (waiting for active session)...".to_string(),
                        ),
                        related_wiki_links: req.related_wiki_links,
                    })
                    .await?;
                self.store
                    .enqueue_followup(&task.id, prompt, req.source)
                    .await?;
                self.event_bus
                    .emit(TaskEvent::TaskCreated { task: task.clone() });
                watcher::ensure_task_watcher(Arc::clone(self), task.clone());
                Ok(task)
            }
            Err(err) => Err(TaskError::from(err)),
        }
    }

    pub(crate) async fn resolve_task_scope(&self, task_id: &str) -> TaskResult<TaskScope> {
        self.ensure_open()?;
        let task = self
            .store
            .get_task_unscoped(task_id)
            .await?
            .ok_or_else(|| TaskError::not_found(format!("task '{task_id}' not found")))?;
        Ok(TaskScope::new(task.owner_id, task.workspace_id))
    }

    /// Gửi lệnh bổ sung (Followup) vào một Task đã có:
    /// - Nếu session đang rảnh -> `submit` ngay lập tức và mở lại Watcher.
    /// - Nếu session đang `Running` (`SESSION_BUSY`) -> lưu vào bảng `task_followups` để Watcher tự `submit` khi turn xong.
    pub(crate) async fn submit_or_enqueue_followup(
        self: &Arc<Self>,
        scope: &TaskScope,
        task_id: &str,
        prompt: String,
        source: TaskSource,
    ) -> TaskResult<FollowupView> {
        self.ensure_open()?;
        let clean_prompt = prompt.trim();
        if clean_prompt.is_empty() {
            return Err(TaskError::invalid_input(
                "followup prompt must not be empty",
            ));
        }

        let task = self.store.get_task(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let orch = self.flows()?.get_flow(&task.flow_id)?.clone();
        let caller = task_scope.to_caller_context();

        let followup = self
            .store
            .enqueue_followup(&task.id, clean_prompt, source)
            .await?;

        if task.status != TaskStatus::Running {
            if let Ok(accepted) = orch
                .submit(&caller, &task.session_id, clean_prompt.to_string())
                .await
            {
                self.store
                    .mark_followup_submitted(&followup.id, &accepted.turn_id)
                    .await?;
                if let Some(updated_task) = self
                    .store
                    .mark_task_turn_running(
                        &task.id,
                        &accepted.turn_id,
                        "Running follow-up turn...",
                    )
                    .await?
                {
                    watcher::ensure_task_watcher(Arc::clone(self), updated_task);
                }
                return Ok(FollowupView {
                    status: FollowupStatus::Submitted,
                    submitted_turn_id: Some(accepted.turn_id),
                    submitted_at: Some(now_ms()),
                    ..followup
                });
            }
        }

        self.event_bus.emit(TaskEvent::FollowupQueued {
            task_id: task.id.clone(),
            followup_id: followup.id.clone(),
            owner_id: task.owner_id.clone(),
            workspace_id: task.workspace_id.clone(),
        });
        watcher::ensure_task_watcher(Arc::clone(self), task);
        Ok(followup)
    }

    /// Hủy một Task đang chạy: hủy các followup đang chờ và gọi `Orchestrator::cancel`.
    pub(crate) async fn cancel_task_internal(
        self: &Arc<Self>,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<TaskView> {
        self.ensure_open()?;
        let task = self.store.get_task(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        self.store.cancel_pending_followups(&task.id).await?;

        if !task.status.is_terminal() {
            let orch = self.flows()?.get_flow(&task.flow_id)?.clone();
            let caller = task_scope.to_caller_context();
            let _ = orch.cancel(&caller, &task.session_id).await;
            if let Some(updated) = self
                .store
                .finish_task(
                    &task.id,
                    TerminalUpdate {
                        status: TaskStatus::Cancelled,
                        output_preview: None,
                        error_code: Some("CANCELLED"),
                        error_message: Some("Cancelled by caller"),
                    },
                )
                .await?
            {
                self.event_bus.emit(TaskEvent::TaskTerminal {
                    task: updated.clone(),
                });
                return Ok(updated);
            }
        }
        Ok(task)
    }

    /// Xóa vĩnh viễn một Task:
    /// 1. Hủy Task và các Followup đang chờ nếu Task chưa ở trạng thái kết thúc.
    /// 2. Dừng Watcher đang theo dõi Task (nếu có).
    /// 3. Xóa bản ghi Task trong `tasks.db` (tự động `ON DELETE CASCADE` với `task_followups`).
    /// 4. Nếu không còn Task nào khác dùng chung `session_id`, gọi `Orchestrator::delete_session`
    ///    để xóa luôn Root + Child sessions, messages, runs, invocations và artifacts trong `orchestrator.db`.
    /// 5. Phát sự kiện `TaskEvent::TaskDeleted` cho UI View Task.
    pub(crate) async fn delete_task_internal(
        self: &Arc<Self>,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<()> {
        self.ensure_open()?;
        let task = self.cancel_task_internal(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);

        if let Some(handle) = lock(&self.active_watchers).remove(&task.id) {
            handle.abort();
        }

        self.store.delete_task(&task_scope, &task.id).await?;

        let remaining = self
            .store
            .count_tasks_for_session(&task.flow_id, &task.session_id)
            .await
            .unwrap_or(1);
        if remaining == 0 {
            if let Ok(orch) = self.flows()?.get_flow(&task.flow_id) {
                let caller = task_scope.to_caller_context();
                let _ = orch.delete_session(&caller, &task.session_id).await;
            }
        }

        self.event_bus.emit(TaskEvent::TaskDeleted {
            task_id: task.id,
            flow_id: task.flow_id,
            session_id: task.session_id,
            owner_id: task.owner_id,
            workspace_id: task.workspace_id,
            timestamp_ms: now_ms(),
        });
        Ok(())
    }

    /// Xóa một trang trong `wiki/` (kèm dọn thư mục cha rỗng, xóa khỏi `.wiki/index.sqlite` và đồng bộ `wiki/index.md`).
    pub(crate) fn delete_wiki_file_internal(
        &self,
        rel_path: &str,
    ) -> TaskResult<WikiDeleteOutcome> {
        self.ensure_open()?;
        let trimmed = rel_path.trim();
        if trimmed.is_empty() {
            return Err(TaskError::invalid_input("wiki file path must not be empty"));
        }
        let engine = self.resolve_wiki_engine()?;
        let deleted = engine
            .delete_file(CollectionScope::Wiki, trimmed)
            .map_err(|e| TaskError::invalid_input(format!("delete wiki file '{trimmed}': {e}")))?;
        if deleted {
            let _ = engine.rebuild_or_sync_index_md();
        }
        let normalized = normalize_collection_rel_path("wiki", trimmed);
        Ok(WikiDeleteOutcome {
            rel_path: normalized,
            deleted,
            mirrored_wiki_rel_path: None,
            mirrored_wiki_deleted: false,
        })
    }

    /// Xóa một tài liệu nguồn trong `sources/` (và tùy chọn xóa luôn trang tóm tắt đối ứng trong `wiki/sources/...`).
    pub(crate) fn delete_source_file_internal(
        &self,
        rel_path: &str,
        delete_wiki_mirror: bool,
    ) -> TaskResult<WikiDeleteOutcome> {
        self.ensure_open()?;
        let trimmed = rel_path.trim();
        if trimmed.is_empty() {
            return Err(TaskError::invalid_input(
                "source file path must not be empty",
            ));
        }
        let engine = self.resolve_wiki_engine()?;
        let deleted = engine
            .delete_file(CollectionScope::Sources, trimmed)
            .map_err(|e| {
                TaskError::invalid_input(format!("delete source file '{trimmed}': {e}"))
            })?;

        let normalized = normalize_collection_rel_path("sources", trimmed);
        let scoped = normalized.strip_prefix("sources/").unwrap_or(&normalized);

        let (mirrored_wiki_rel_path, mirrored_wiki_deleted) = if delete_wiki_mirror {
            let mirror_scoped = format!("sources/{scoped}");
            let mirror_full = format!("wiki/{mirror_scoped}");
            let m_del = engine
                .delete_file(CollectionScope::Wiki, &mirror_scoped)
                .unwrap_or(false);
            if m_del {
                let _ = engine.rebuild_or_sync_index_md();
            }
            (Some(mirror_full), m_del)
        } else {
            (None, false)
        };

        Ok(WikiDeleteOutcome {
            rel_path: normalized,
            deleted,
            mirrored_wiki_rel_path,
            mirrored_wiki_deleted,
        })
    }

    /// Tạo Schedule hoặc Reminder mới trong `task_schedules` và đánh thức `Scheduler`.
    pub(crate) async fn create_schedule_internal(
        self: &Arc<Self>,
        scope: &TaskScope,
        req: CreateScheduleRequest,
    ) -> TaskResult<ScheduleView> {
        self.ensure_open()?;
        scope.validate()?;

        let title = req.title.trim();
        if title.is_empty() {
            return Err(TaskError::invalid_input(
                "schedule/reminder title must not be empty",
            ));
        }

        let tz_name = req
            .timezone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.default_timezone)
            .to_string();

        let (flow_id, agent_id) = match req.action_kind {
            ScheduleActionKind::SpawnTask => {
                if let Some(flows) = self.flows.get() {
                    let (target, _) = flows.resolve_target(req.target_agent.as_deref())?;
                    (Some(target.flow_id), Some(target.agent_id))
                } else {
                    let target = req
                        .target_agent
                        .as_deref()
                        .and_then(|s| s.split_once('/'))
                        .map(|(f, a)| (Some(f.to_string()), Some(a.to_string())))
                        .or_else(|| {
                            self.config
                                .default_target
                                .as_ref()
                                .map(|t| (Some(t.flow_id.clone()), Some(t.agent_id.clone())))
                        })
                        .unwrap_or((
                            Some("daily".to_string()),
                            req.target_agent
                                .clone()
                                .or_else(|| Some("daily_root".to_string())),
                        ));
                    target
                }
            }
            ScheduleActionKind::NotifyOnly => (None, None),
        };

        let now = now_ms();
        let (schedule_kind, due_at, cron_expr, next_run_at, status) = if let Some(cron_raw) =
            req.cron_expr.as_deref().filter(|s| !s.trim().is_empty())
        {
            let (clean_cron, next_ms) = compute_next_cron_ms(cron_raw, &tz_name, now)?;
            (
                ScheduleKind::Cron,
                None,
                Some(clean_cron),
                Some(next_ms),
                ScheduleStatus::Active,
            )
        } else if let Some(delay_secs) = req.delay_seconds {
            let due = now.saturating_add(i64::try_from(delay_secs).unwrap_or(0) * 1000);
            (
                ScheduleKind::Once,
                Some(due),
                None,
                Some(due),
                ScheduleStatus::Pending,
            )
        } else if let Some(due_iso) = req.due_at_iso.as_deref().filter(|s| !s.trim().is_empty()) {
            let due = parse_due_at_ms(due_iso, &tz_name)?;
            (
                ScheduleKind::Once,
                Some(due),
                None,
                Some(due),
                ScheduleStatus::Pending,
            )
        } else {
            return Err(TaskError::invalid_input(
                "schedule requires one of: due_at_iso, delay_seconds, or cron_expr",
            ));
        };

        let created = self
            .store
            .insert_schedule(NewScheduleRow {
                id: new_id(),
                scope: scope.clone(),
                title: title.to_string(),
                notes: req
                    .notes
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                action_kind: req.action_kind,
                flow_id,
                agent_id,
                session_mode: req.session_mode,
                priority: req.priority,
                related_wiki_links: req.related_wiki_links,
                schedule_kind,
                due_at,
                cron_expr,
                timezone: tz_name,
                status,
                next_run_at,
            })
            .await?;

        self.scheduler_wake.notify_one();
        Ok(created)
    }

    /// Đồng bộ trạng thái của một Task với `Orchestrator::result` (dùng khi turn kết thúc hoặc lúc startup recovery).
    pub(crate) async fn settle_or_advance_task(
        self: &Arc<Self>,
        task: &TaskView,
    ) -> TaskResult<bool> {
        let orch = self.flows()?.get_flow(&task.flow_id)?.clone();
        let scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let caller = scope.to_caller_context();

        if let Some(ref turn_id) = task.latest_turn_id {
            let res = orch.result(&caller, &task.session_id, turn_id).await?;
            if res.status == RunStatus::Running {
                return Ok(false);
            }

            // Nếu turn vừa hoàn tất và có followup đang chờ trong hàng đợi -> tự động submit followup kế tiếp!
            if res.status == RunStatus::Completed {
                if let Some(next_fu) = self.store.next_pending_followup(&task.id).await? {
                    if let Ok(accepted) = orch
                        .submit(&caller, &task.session_id, next_fu.prompt.clone())
                        .await
                    {
                        self.store
                            .mark_followup_submitted(&next_fu.id, &accepted.turn_id)
                            .await?;
                        self.store
                            .mark_task_turn_running(
                                &task.id,
                                &accepted.turn_id,
                                "Running queued follow-up...",
                            )
                            .await?;
                        return Ok(false);
                    }
                }
            }

            let status = TaskStatus::from_run_status(res.status);
            let preview = res
                .output
                .as_deref()
                .map(|s| clip_preview(s, self.config.runtime.output_preview_chars));
            let err_code = res.error.as_ref().map(|e| e.code.as_str());
            let err_msg = res.error.as_ref().map(|e| e.message.as_str());

            if let Some(finished) = self
                .store
                .finish_task(
                    &task.id,
                    TerminalUpdate {
                        status,
                        output_preview: preview.as_deref(),
                        error_code: err_code,
                        error_message: err_msg,
                    },
                )
                .await?
            {
                self.event_bus.emit(TaskEvent::TaskTerminal {
                    task: finished.clone(),
                });
                self.autoflow_done_wake.notify_waiters();
            }
            return Ok(true);
        }

        // Trường hợp Task đang 'queued' (chưa có turn_id) và có followup chờ
        if let Some(next_fu) = self.store.next_pending_followup(&task.id).await? {
            if let Ok(accepted) = orch
                .submit(&caller, &task.session_id, next_fu.prompt.clone())
                .await
            {
                self.store
                    .mark_followup_submitted(&next_fu.id, &accepted.turn_id)
                    .await?;
                self.store
                    .mark_task_turn_running(&task.id, &accepted.turn_id, "Starting queued turn...")
                    .await?;
                return Ok(false);
            }
        }

        Ok(false)
    }

    /// Lấy chi tiết đầy đủ của một Task kèm `FamilySnapshot`, danh sách `artifacts` và các view thực thi của Root + Subagent.
    pub(crate) async fn get_task_detail_internal(
        &self,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<TaskDetailView> {
        self.ensure_open()?;
        let task = self.store.get_task(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let followups = self.store.list_followups(&task.id).await?;
        let (family_snapshot, artifacts) = if let Ok(orch) = self.flows()?.get_flow(&task.flow_id) {
            let caller = task_scope.to_caller_context();
            let snap = orch.snapshot(&caller, &task.session_id).await.ok();
            let arts = orch
                .list_family_artifacts(&caller, &task.session_id)
                .await
                .unwrap_or_default()
                .iter()
                .map(|fa| TaskArtifactView::from_family_artifact(&task.id, fa))
                .collect();
            (snap, arts)
        } else {
            (None, Vec::new())
        };

        Ok(TaskDetailView::from_parts_with_artifacts(
            task,
            followups,
            family_snapshot,
            artifacts,
        ))
    }

    /// Liệt kê toàn bộ các Artifact đã commit thuộc về một Task (gồm cả Root Agent và các Subagent con).
    pub(crate) async fn list_task_artifacts_internal(
        &self,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<Vec<TaskArtifactView>> {
        self.ensure_open()?;
        let task = self.store.get_task(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let orch = self.flows()?.get_flow(&task.flow_id)?;
        let caller = task_scope.to_caller_context();
        let family_artifacts = orch
            .list_family_artifacts(&caller, &task.session_id)
            .await?;
        Ok(family_artifacts
            .iter()
            .map(|fa| TaskArtifactView::from_family_artifact(&task.id, fa))
            .collect())
    }

    /// Truy vấn danh mục Artifact toàn cục (`Artifact Catalog`) từ tất cả `Orchestrator` flows,
    /// kết nối với `TaskView` sở hữu trong `tasks.db`, hỗ trợ lọc, tìm kiếm, phân trang và tính `summary`.
    pub(crate) async fn list_artifacts_catalog_internal(
        &self,
        scope: Option<&TaskScope>,
        filter: &TaskArtifactFilter,
    ) -> TaskResult<TaskArtifactCatalogPage> {
        self.ensure_open()?;
        if let Some(s) = scope {
            s.validate()?;
        }

        let tasks = self.store.list_tasks_unbounded(scope).await?;
        if tasks.is_empty() {
            return Ok(TaskArtifactCatalogPage::default());
        }

        let mut tasks_by_session: HashMap<&str, Vec<&TaskView>> = HashMap::new();
        for task in &tasks {
            tasks_by_session
                .entry(task.session_id.as_str())
                .or_default()
                .push(task);
        }

        let mut seen_orch_ptrs: HashSet<usize> = HashSet::new();
        let mut seen_artifact_ids: HashSet<String> = HashSet::new();
        let mut entries: Vec<TaskArtifactCatalogEntry> = Vec::new();

        for (flow_id, orch) in self.flows()?.iter_flows() {
            let ptr = Arc::as_ptr(orch) as usize;
            if !seen_orch_ptrs.insert(ptr) {
                continue;
            }
            let family_artifacts = orch.list_all_family_artifacts().await?;
            for fa in family_artifacts {
                if seen_artifact_ids.contains(&fa.artifact.id) {
                    continue;
                }
                let Some(candidates) = tasks_by_session.get(fa.root_session_id.as_str()) else {
                    continue;
                };
                let art_created_ms = if fa.created_at < 1_000_000_000_000 {
                    fa.created_at.saturating_mul(1000)
                } else {
                    fa.created_at
                };
                let matched_task = candidates
                    .iter()
                    .copied()
                    .find(|t| {
                        t.flow_id == flow_id
                            && t.latest_turn_id.as_deref() == Some(fa.turn_id.as_str())
                    })
                    .or_else(|| {
                        candidates
                            .iter()
                            .copied()
                            .find(|t| t.latest_turn_id.as_deref() == Some(fa.turn_id.as_str()))
                    })
                    .or_else(|| {
                        candidates
                            .iter()
                            .copied()
                            .find(|t| t.flow_id == flow_id && t.created_at <= art_created_ms)
                    })
                    .or_else(|| candidates.iter().copied().find(|t| t.flow_id == flow_id))
                    .or_else(|| candidates.first().copied());

                let Some(task) = matched_task else {
                    continue;
                };

                seen_artifact_ids.insert(fa.artifact.id.clone());
                let artifact = TaskArtifactView::from_family_artifact(&task.id, &fa);
                entries.push(TaskArtifactCatalogEntry {
                    artifact,
                    task_title: task.title.clone(),
                    task_prompt: task.prompt.clone(),
                    task_status: task.status,
                    flow_id: task.flow_id.clone(),
                    root_agent_id: task.agent_id.clone(),
                    owner_id: task.owner_id.clone(),
                    workspace_id: task.workspace_id.clone(),
                });
            }
        }

        entries.sort_by(|a, b| {
            b.artifact
                .created_at
                .cmp(&a.artifact.created_at)
                .then_with(|| b.artifact.id.cmp(&a.artifact.id))
        });

        let task_id_filter = filter
            .task_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let flow_id_filter = filter
            .flow_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let agent_id_filter = filter
            .producer_agent_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let query_lower = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);

        let base_matched: Vec<TaskArtifactCatalogEntry> = entries
            .into_iter()
            .filter(|entry| {
                if let Some(tid) = task_id_filter {
                    if entry.artifact.task_id != tid {
                        return false;
                    }
                }
                if let Some(fid) = flow_id_filter {
                    if entry.flow_id != fid {
                        return false;
                    }
                }
                if let Some(aid) = agent_id_filter {
                    if entry.artifact.producer_agent_id != aid {
                        return false;
                    }
                }
                if let Some(ref q) = query_lower {
                    let matches_q = entry.artifact.name.to_lowercase().contains(q)
                        || entry.artifact.id.to_lowercase().contains(q)
                        || entry.artifact.mime_type.to_lowercase().contains(q)
                        || entry.artifact.producer_agent_id.to_lowercase().contains(q)
                        || entry
                            .artifact
                            .producer_tool_name
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(q)
                        || entry.task_title.to_lowercase().contains(q)
                        || entry.task_prompt.to_lowercase().contains(q)
                        || entry.artifact.task_id.to_lowercase().contains(q);
                    if !matches_q {
                        return false;
                    }
                }
                true
            })
            .collect();

        let mut summary = TaskArtifactCatalogSummary::default();
        for entry in &base_matched {
            summary.record(&entry.artifact);
        }

        let filtered: Vec<TaskArtifactCatalogEntry> = base_matched
            .into_iter()
            .filter(|entry| {
                if let Some(kind) = filter.kind {
                    if entry.artifact.kind != kind {
                        return false;
                    }
                }
                if let Some(role) = filter.role {
                    if entry.artifact.role != role {
                        return false;
                    }
                }
                true
            })
            .collect();

        let total = u64::try_from(filtered.len()).unwrap_or(0);
        let max_limit = self.config.runtime.page_size.max(500);
        let limit = filter
            .limit
            .unwrap_or(self.config.runtime.page_size)
            .clamp(1, max_limit) as usize;
        let offset = filter.offset.unwrap_or(0) as usize;

        let items = filtered.into_iter().skip(offset).take(limit).collect();

        Ok(TaskArtifactCatalogPage {
            items,
            total,
            summary,
        })
    }

    /// Mở một Artifact thuộc về một Task để đọc/stream kèm xác minh toàn vẹn SHA-256.
    pub(crate) async fn open_task_artifact_internal(
        &self,
        scope: &TaskScope,
        task_id: &str,
        artifact_id: &str,
    ) -> TaskResult<TaskArtifactDownload> {
        self.ensure_open()?;
        let trimmed_art_id = artifact_id.trim();
        if trimmed_art_id.is_empty() {
            return Err(TaskError::invalid_input("artifact_id must not be empty"));
        }
        let task = self.store.get_task(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let orch = self.flows()?.get_flow(&task.flow_id)?;
        let caller = task_scope.to_caller_context();

        let family_artifacts = orch
            .list_family_artifacts(&caller, &task.session_id)
            .await?;
        let download = orch
            .open_family_artifact(&caller, &task.session_id, trimmed_art_id)
            .await?;

        let metadata = family_artifacts
            .iter()
            .find(|fa| fa.artifact.id == download.artifact.id)
            .map(|fa| TaskArtifactView::from_family_artifact(&task.id, fa))
            .unwrap_or_else(|| TaskArtifactView {
                id: download.artifact.id.clone(),
                task_id: task.id.clone(),
                root_session_id: task.session_id.clone(),
                producer_session_id: task.session_id.clone(),
                producer_agent_id: task.agent_id.clone(),
                turn_id: task.latest_turn_id.clone().unwrap_or_default(),
                producer_invocation_id: None,
                producer_tool_name: None,
                name: download.artifact.name.clone(),
                mime_type: download.artifact.mime_type.clone(),
                kind: TaskArtifactKind::from_mime_and_name(
                    &download.artifact.mime_type,
                    &download.artifact.name,
                ),
                role: TaskArtifactRole::from_agent_role(download.artifact.role),
                size_bytes: download.artifact.size_bytes,
                sha256: download.artifact.sha256.clone(),
                created_at: task.updated_at,
            });

        Ok(TaskArtifactDownload {
            metadata,
            reader: download.into_reader(),
        })
    }

    /// Lấy lịch sử tin nhắn (`user`, `assistant`, `tool`) của Root Agent hoặc một Subagent cụ thể trong Task.
    pub(crate) async fn get_task_agent_history_internal(
        &self,
        scope: &TaskScope,
        task_id: &str,
        session_or_agent_id: Option<&str>,
        limit: Option<u32>,
    ) -> TaskResult<Vec<TaskAgentMessageView>> {
        self.ensure_open()?;
        let task = self.store.get_task(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let orch = self.flows()?.get_flow(&task.flow_id)?;
        let caller = task_scope.to_caller_context();

        let target_session_id = match session_or_agent_id.map(str::trim).filter(|s| !s.is_empty()) {
            None => task.session_id.clone(),
            Some(key) if key == task.session_id || key == task.agent_id => task.session_id.clone(),
            Some(key) => {
                let snap = orch.snapshot(&caller, &task.session_id).await?;
                if let Some(child) = snap.children.iter().find(|c| c.session_id == key) {
                    child.session_id.clone()
                } else if let Some(child) = snap.children.iter().rev().find(|c| c.agent_id == key) {
                    child.session_id.clone()
                } else {
                    return Err(TaskError::not_found(format!(
                        "agent session '{key}' not found in task '{task_id}'"
                    )));
                }
            }
        };

        let page_limit = limit
            .unwrap_or(self.config.runtime.page_size)
            .clamp(1, self.config.runtime.page_size);
        let page = orch
            .history(&caller, &target_session_id, PageRequest::first(page_limit))
            .await?;

        Ok(page
            .items
            .iter()
            .map(TaskAgentMessageView::from_message_view)
            .collect())
    }
}

fn normalize_collection_rel_path(collection: &str, raw: &str) -> String {
    let clean = raw.trim().trim_start_matches('/').replace('\\', "/");
    let prefix = format!("{collection}/");
    let scoped = clean.strip_prefix(&prefix).unwrap_or(&clean);
    let with_ext = if scoped.ends_with(".md") {
        scoped.to_string()
    } else {
        format!("{scoped}.md")
    };
    format!("{collection}/{with_ext}")
}

fn clip_preview(text: &str, max_chars: usize) -> String {
    let cleaned = orchestrator_agent_rs::strip_thinking_blocks(text);
    let normalized = llm_wiki::normalize_media_markdown_links(cleaned.trim());
    let trimmed = normalized.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push('…');
    out
}
