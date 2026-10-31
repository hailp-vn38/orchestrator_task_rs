//! Truy vấn chi tiết Task, lịch sử tin nhắn Agent, danh mục `Artifact Catalog`
//! và thao tác quản lý file `wiki/` / `sources/` trên `SharedTaskCore`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use llm_wiki::CollectionScope;
use orchestrator_agent_rs::PageRequest;

use crate::error::{TaskError, TaskResult};
use crate::runtime::SharedTaskCore;
use crate::types::{
    TaskAgentMessageView, TaskArtifactCatalogEntry, TaskArtifactCatalogPage,
    TaskArtifactCatalogSummary, TaskArtifactDownload, TaskArtifactFilter, TaskArtifactKind,
    TaskArtifactRole, TaskArtifactView, TaskDetailView, TaskScope, TaskView, WikiDeleteOutcome,
};

impl SharedTaskCore {
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

        let max_resume_attempts = self
            .flows()?
            .get_flow(&task.flow_id)
            .ok()
            .map(|orch| orch.max_resume_attempts());
        let mut detail =
            TaskDetailView::from_parts_with_artifacts(task, followups, family_snapshot, artifacts);
        detail.max_resume_attempts = max_resume_attempts;
        Ok(detail)
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
