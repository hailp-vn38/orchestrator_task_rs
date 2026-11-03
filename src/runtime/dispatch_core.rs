//! Logic điều phối lõi trên `Arc<SharedTaskCore>`, dùng chung cho `TaskManager`, `Scheduler`,
//! `AutoFlow` và các `HostTool`.

use std::sync::Arc;

use orchestrator_agent_rs::{
    ErrorCode as OrchErrorCode, OperatorContext, RunBudget, RunStatus, SubmitOptions, ToolOutcome,
    VerifiedToolResolution, CODE_BUDGET_EXHAUSTED, CODE_STEP_LIMIT_REACHED,
};

use crate::error::{TaskError, TaskResult};
use crate::runtime::{lock, watcher, SharedTaskCore};
use crate::store::{new_id, now_ms, NewTaskRow, TerminalUpdate};
use crate::types::{
    CreateTaskRequest, FollowupStatus, FollowupView, ResolveStuckTool, StuckToolDecision,
    TaskEvent, TaskScope, TaskSource, TaskStatus, TaskView,
};

pub(crate) fn budget_for_source(source: TaskSource) -> RunBudget {
    let now = (now_ms() / 1000).max(0);
    let (active_limit_secs, total_secs, reserve_secs) = match source {
        TaskSource::Voice => (5 * 60, 5 * 60, 30),
        TaskSource::Api | TaskSource::Chat => (2 * 3600, 24 * 3600, 10 * 60),
        TaskSource::Schedule | TaskSource::Autoflow => (2 * 3600, 12 * 3600, 10 * 60),
    };
    RunBudget {
        started_at: now,
        total_deadline_at: now.saturating_add(total_secs),
        active_limit_secs,
        reserve_secs,
        max_tokens: None,
        waited_secs: 0,
    }
}

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
        let has_input_artifacts = !req.input_artifacts.is_empty();
        let submit_opts = SubmitOptions {
            budget: Some(budget_for_source(req.source)),
        };
        let submitted = if has_input_artifacts {
            let prompt_owned = prompt.to_string();
            orch.submit_with_artifacts_and_options(
                &caller,
                &session_id,
                req.input_artifacts,
                submit_opts,
                move |refs| format_prompt_with_artifacts(&prompt_owned, refs),
            )
            .await
            .map(|(accepted, _)| accepted)
        } else {
            orch.submit_with_options(&caller, &session_id, prompt.to_string(), submit_opts)
                .await
        };

        match submitted {
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
            Err(err) if err.code() == OrchErrorCode::SessionBusy && has_input_artifacts => Err(
                TaskError::new(crate::error::ErrorCode::SessionBusy, err.to_string()),
            ),
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
        followup_agent: Option<&str>,
        first_context: Option<String>,
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

        let (target_session_id, is_new_followup_session) = if let Some(agent_id) = followup_agent {
            if let Some(session_id) = task.followup_session_id.clone() {
                (session_id, false)
            } else {
                let created = orch.create_session(&caller, agent_id).await?;
                let persisted = self
                    .store
                    .set_followup_session_id(&task.id, &created)
                    .await?;
                (persisted.clone(), persisted == created)
            }
        } else {
            (task.active_session_id.clone(), false)
        };
        let stored_prompt = if is_new_followup_session {
            first_context.as_deref().unwrap_or(clean_prompt).to_string()
        } else {
            clean_prompt.to_string()
        };

        let followup = self
            .store
            .enqueue_followup(&task.id, &stored_prompt, source)
            .await?;

        if task.status != TaskStatus::Running && task.status != TaskStatus::Waiting {
            let submit_opts = SubmitOptions {
                budget: Some(budget_for_source(source)),
            };
            if let Ok(accepted) = orch
                .submit_with_options(&caller, &target_session_id, stored_prompt, submit_opts)
                .await
            {
                self.store
                    .mark_followup_submitted(&followup.id, &accepted.turn_id)
                    .await?;
                if let Some(updated_task) = self
                    .store
                    .mark_task_turn_running(
                        &task.id,
                        &target_session_id,
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
            let _ = orch.cancel(&caller, &task.active_session_id).await;
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
        if let Some(session_id) = task.followup_session_id.as_deref() {
            if let Ok(orch) = self.flows()?.get_flow(&task.flow_id) {
                let caller = task_scope.to_caller_context();
                let _ = orch.delete_session(&caller, session_id).await;
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

    /// Tiếp tục ngay một Task đang ở trạng thái chờ (`waiting_retry` hoặc `waiting_user`).
    pub(crate) async fn resume_waiting_task_internal(
        self: &Arc<Self>,
        scope: &TaskScope,
        task_id: &str,
    ) -> TaskResult<TaskView> {
        self.ensure_open()?;
        let task = self.store.get_task(scope, task_id).await?;
        let task_scope = TaskScope::new(&task.owner_id, &task.workspace_id);
        let orch = self.flows()?.get_flow(&task.flow_id)?.clone();
        let caller = task_scope.to_caller_context();

        match orch.resume_waiting(&caller, &task.active_session_id).await {
            Ok(()) => {}
            Err(err) if err.code() == OrchErrorCode::InvalidInput => {
                return Err(TaskError::invalid_state(err.message()));
            }
            Err(err) => return Err(TaskError::from(err)),
        }

        let updated = self
            .store
            .clear_task_waiting(&task.id, "Resuming waiting turn...")
            .await?
            .unwrap_or(task);
        watcher::ensure_task_watcher(Arc::clone(self), updated.clone());
        Ok(updated)
    }

    /// Ghi kết luận của người vận hành cho tool không rõ kết quả rồi cho Task chạy tiếp. Không bao
    /// giờ chạy lại tool; `session_id` là session con khi tool đó thuộc subagent.
    pub(crate) async fn resolve_stuck_tool_internal(
        self: &Arc<Self>,
        request: ResolveStuckTool,
    ) -> TaskResult<TaskView> {
        self.ensure_open()?;
        let scope = self.resolve_task_scope(&request.task_id).await?;
        let task = self.store.get_task(&scope, &request.task_id).await?;
        let orch = self.flows()?.get_flow(&task.flow_id)?;
        let operator =
            OperatorContext::new(request.operator_id, &scope.owner_id, &scope.workspace_id)
                .with_resolve_capability();
        let session_id = request
            .session_id
            .as_deref()
            .unwrap_or(task.active_session_id.as_str());
        let decision = match request.decision {
            StuckToolDecision::NoEffect { evidence } => {
                VerifiedToolResolution::ConfirmedNoEffect { evidence }
            }
            StuckToolDecision::Completed {
                tool_call_id,
                result_text,
                evidence,
            } => VerifiedToolResolution::ConfirmedCompleted {
                outcome: ToolOutcome {
                    call_id: tool_call_id,
                    content: result_text,
                    metadata: serde_json::Value::Null,
                },
                evidence,
            },
        };
        match orch
            .resolve_interrupted_invocation(&operator, session_id, &request.invocation_id, decision)
            .await
        {
            Ok(()) => {}
            Err(err) if err.code() == OrchErrorCode::InvalidInput => {
                return Err(TaskError::invalid_state(err.message()));
            }
            Err(err) => return Err(TaskError::from(err)),
        }
        let updated = self
            .store
            .reopen_task(&task.id, "Resuming after tool outcome was resolved...")
            .await?
            .unwrap_or(task);
        watcher::ensure_task_watcher(Arc::clone(self), updated.clone());
        Ok(updated)
    }

    /// Ghi trạng thái chờ lên Task và phát progress. Task đã kết thúc giữ nguyên, không phát gì.
    pub(crate) async fn project_task_waiting(
        &self,
        task: &TaskView,
        wait_reason: Option<&str>,
        next_attempt_at: Option<i64>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> TaskResult<()> {
        let summary = match (wait_reason, next_attempt_at) {
            (Some(reason), Some(at)) => format!("Waiting ({reason}), retry at {at}"),
            (Some(reason), None) => format!("Waiting for user action ({reason})"),
            _ => "Waiting...".to_string(),
        };
        let updated = self
            .store
            .mark_task_waiting(
                &task.id,
                wait_reason,
                next_attempt_at,
                &summary,
                error_code,
                error_message,
            )
            .await?;
        if updated.is_some_and(|latest| latest.status == TaskStatus::Waiting) {
            self.event_bus.emit(TaskEvent::TaskProgress {
                task_id: task.id.clone(),
                owner_id: task.owner_id.clone(),
                workspace_id: task.workspace_id.clone(),
                status: TaskStatus::Waiting,
                active_subagent: None,
                active_tool: None,
                progress_summary: Some(summary),
                timestamp_ms: now_ms(),
            });
        }
        Ok(())
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
            let res = orch
                .result(&caller, &task.active_session_id, turn_id)
                .await?;
            if res.status == RunStatus::Running {
                // Root vẫn `running` khi con cháu chờ: Task phải giữ lý do, lịch và nút "Chạy tiếp".
                if let Some(child) = orch
                    .waiting_descendant(&caller, &task.active_session_id)
                    .await?
                {
                    let unchanged = task.status == TaskStatus::Waiting
                        && task.wait_reason == child.retry_reason
                        && task.next_attempt_at == child.next_attempt_at;
                    if !unchanged {
                        self.project_task_waiting(
                            task,
                            child.retry_reason.as_deref(),
                            child.next_attempt_at,
                            child.error_code.as_deref(),
                            child.error_message.as_deref(),
                        )
                        .await?;
                    }
                    return Ok(false);
                }
                if task.status == TaskStatus::Waiting
                    && self
                        .store
                        .clear_task_waiting(&task.id, "Running resumed turn...")
                        .await?
                        .is_some()
                {
                    self.event_bus.emit(TaskEvent::TaskProgress {
                        task_id: task.id.clone(),
                        owner_id: task.owner_id.clone(),
                        workspace_id: task.workspace_id.clone(),
                        status: TaskStatus::Running,
                        active_subagent: None,
                        active_tool: None,
                        progress_summary: Some("Running resumed turn...".into()),
                        timestamp_ms: now_ms(),
                    });
                }
                return Ok(false);
            }
            if !res.status.is_terminal() {
                let wait_reason = res
                    .retry_reason
                    .as_deref()
                    .or_else(|| res.error.as_ref().map(|e| e.code.as_str()));
                let err_code = res.error.as_ref().map(|e| e.code.as_str());
                let err_msg = res.error.as_ref().map(|e| e.message.as_str());
                self.project_task_waiting(
                    task,
                    wait_reason,
                    res.next_attempt_at,
                    err_code,
                    err_msg,
                )
                .await?;
                return Ok(false);
            }

            let stop_code = res
                .error
                .as_ref()
                .filter(|_| res.status != RunStatus::Cancelled)
                .map(|e| e.code.as_str());
            let is_budget_exhausted = stop_code == Some(CODE_BUDGET_EXHAUSTED);
            let is_step_limited = stop_code == Some(CODE_STEP_LIMIT_REACHED);

            // Nếu turn vừa hoàn tất bình thường và có followup đang chờ trong hàng đợi -> tự động submit followup kế tiếp!
            if res.status == RunStatus::Completed && !is_budget_exhausted {
                if let Some(next_fu) = self.store.next_pending_followup(&task.id).await? {
                    let submit_opts = SubmitOptions {
                        budget: Some(budget_for_source(next_fu.source)),
                    };
                    if let Ok(accepted) = orch
                        .submit_with_options(
                            &caller,
                            task.followup_session_id
                                .as_deref()
                                .unwrap_or(&task.active_session_id),
                            next_fu.prompt.clone(),
                            submit_opts,
                        )
                        .await
                    {
                        self.store
                            .mark_followup_submitted(&next_fu.id, &accepted.turn_id)
                            .await?;
                        self.store
                            .mark_task_turn_running(
                                &task.id,
                                task.followup_session_id
                                    .as_deref()
                                    .unwrap_or(&task.active_session_id),
                                &accepted.turn_id,
                                "Running queued follow-up...",
                            )
                            .await?;
                        return Ok(false);
                    }
                }
            }

            let status = if is_budget_exhausted || is_step_limited {
                TaskStatus::Stopped
            } else {
                TaskStatus::from_run_status(res.status)
            };
            let preview = res
                .output
                .as_deref()
                .or(res.partial_output.as_deref())
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
            let submit_opts = SubmitOptions {
                budget: Some(budget_for_source(next_fu.source)),
            };
            if let Ok(accepted) = orch
                .submit_with_options(
                    &caller,
                    task.followup_session_id
                        .as_deref()
                        .unwrap_or(&task.active_session_id),
                    next_fu.prompt.clone(),
                    submit_opts,
                )
                .await
            {
                self.store
                    .mark_followup_submitted(&next_fu.id, &accepted.turn_id)
                    .await?;
                self.store
                    .mark_task_turn_running(
                        &task.id,
                        task.followup_session_id
                            .as_deref()
                            .unwrap_or(&task.active_session_id),
                        &accepted.turn_id,
                        "Starting queued turn...",
                    )
                    .await?;
                return Ok(false);
            }
        }

        Ok(false)
    }
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

fn format_prompt_with_artifacts(
    prompt: &str,
    refs: &[orchestrator_agent_rs::ArtifactRef],
) -> String {
    if refs.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::with_capacity(prompt.len() + 128 * refs.len());
    out.push_str(prompt);
    out.push_str("\n\nAttached input artifacts:");
    for r in refs {
        out.push_str(&format!(
            "\n- {} ({}, {} bytes, mime={})",
            r.id, r.name, r.size_bytes, r.mime_type
        ));
    }
    out
}
