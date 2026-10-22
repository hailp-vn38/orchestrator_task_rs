//! Các dịch vụ runtime nội bộ ẩn sau `TaskManager` (Deep Module implementation).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule as CronSchedule;
use llm_wiki::WikiEngine;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::TaskConfig;
use crate::error::{ErrorCode, TaskError, TaskResult};
use crate::store::{now_ms, TaskStore};

pub(crate) mod autoflow;
pub(crate) mod dispatch_core;
pub(crate) mod event_bus;
pub(crate) mod flow_registry;
pub(crate) mod scheduler;
pub(crate) mod watcher;

pub use autoflow::WikiAutoFlowBinding;
pub(crate) use event_bus::TaskEventBus;
pub(crate) use flow_registry::FlowRegistry;

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Lõi chia sẻ giữa `TaskManager`, các `HostTool` đã đăng ký ở Pha 1 (`prepare`),
/// và các worker nền ở Pha 2 (`start`).
pub(crate) struct SharedTaskCore {
    pub(crate) config: TaskConfig,
    pub(crate) store: TaskStore,
    pub(crate) event_bus: TaskEventBus,
    pub(crate) scheduler_wake: Arc<Notify>,
    pub(crate) autoflow_done_wake: Arc<Notify>,
    pub(crate) shutdown: CancellationToken,
    /// Được gán một lần duy nhất khi gọi `TaskManagerInit::start()`.
    pub(crate) flows: OnceLock<FlowRegistry>,
    /// `WikiEngine` dùng chung cho Auto-Flow và các thao tác quản lý tài liệu Wiki/Source.
    pub(crate) wiki_engine: OnceLock<Arc<WikiEngine>>,
    /// Các watcher đang theo dõi các Task `running` (key = `task_id`).
    pub(crate) active_watchers: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl SharedTaskCore {
    pub(crate) fn new(config: TaskConfig, store: TaskStore, shutdown: CancellationToken) -> Self {
        let event_bus = TaskEventBus::new(config.runtime.event_capacity, shutdown.clone());
        Self {
            config,
            store,
            event_bus,
            scheduler_wake: Arc::new(Notify::new()),
            autoflow_done_wake: Arc::new(Notify::new()),
            shutdown,
            flows: OnceLock::new(),
            wiki_engine: OnceLock::new(),
            active_watchers: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn ensure_open(&self) -> TaskResult<()> {
        if self.shutdown.is_cancelled() {
            return Err(TaskError::new(
                ErrorCode::ShuttingDown,
                "TaskManager is shutting down",
            ));
        }
        Ok(())
    }

    pub(crate) fn flows(&self) -> TaskResult<&FlowRegistry> {
        self.flows.get().ok_or_else(|| {
            TaskError::new(
                ErrorCode::NotReady,
                "TaskManager has not been started yet (call TaskManagerInit::start first)",
            )
        })
    }

    pub(crate) fn resolve_wiki_engine(&self) -> TaskResult<Arc<WikiEngine>> {
        if let Some(eng) = self.wiki_engine.get() {
            return Ok(Arc::clone(eng));
        }
        if let Some(ref vault_dir) = self.config.wiki_vault_dir {
            let opened = Arc::new(
                WikiEngine::open(vault_dir)
                    .map_err(|e| TaskError::storage("open WikiEngine", e))?,
            );
            let _ = self.wiki_engine.set(Arc::clone(&opened));
            return Ok(opened);
        }
        Err(TaskError::config(
            "WikiEngine is not configured (set wiki_vault_dir in TaskConfig or call with_wiki_engine / with_wiki_autoflow)",
        ))
    }
}

/// Phân tích chuỗi ISO-8601 / RFC3339 hoặc số nguyên epoch ms thành timestamp milliseconds.
pub(crate) fn parse_due_at_ms(raw: &str, default_tz_name: &str) -> TaskResult<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TaskError::invalid_input("due_at must not be empty"));
    }
    if let Ok(epoch_ms) = trimmed.parse::<i64>() {
        return Ok(epoch_ms);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.timestamp_millis());
    }
    let tz: Tz = Tz::from_str(default_tz_name).unwrap_or(chrono_tz::Asia::Ho_Chi_Minh);
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, fmt) {
            if let Some(local_dt) = tz.from_local_datetime(&naive).earliest() {
                return Ok(local_dt.timestamp_millis());
            }
        }
    }
    Err(TaskError::invalid_input(format!(
        "invalid due_at format '{trimmed}': expected ISO-8601 (e.g. '2026-10-22T09:00:00+07:00')"
    )))
}

/// Chuẩn hóa biểu thức cron (hỗ trợ cả chuẩn 5 trường Unix lẫn 6/7 trường của crate `cron`)
/// và tính thời điểm chạy kế tiếp sau `after_ms`.
pub(crate) fn compute_next_cron_ms(
    cron_expr: &str,
    tz_name: &str,
    after_ms: i64,
) -> TaskResult<(String, i64)> {
    let trimmed = cron_expr.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let normalized = match parts.len() {
        5 => format!("0 {trimmed}"),
        6 | 7 => trimmed.to_string(),
        _ => {
            return Err(TaskError::invalid_input(format!(
                "invalid cron expression '{trimmed}': expected 5 or 6 fields"
            )))
        }
    };
    let schedule = CronSchedule::from_str(&normalized).map_err(|e| {
        TaskError::invalid_input(format!("invalid cron expression '{trimmed}': {e}"))
    })?;
    let tz: Tz = Tz::from_str(tz_name.trim())
        .map_err(|_| TaskError::invalid_input(format!("invalid timezone '{tz_name}'")))?;
    let base_utc = Utc
        .timestamp_millis_opt(after_ms.max(now_ms()))
        .single()
        .unwrap_or_else(Utc::now);
    let base_tz = base_utc.with_timezone(&tz);
    let next = schedule.after(&base_tz).next().ok_or_else(|| {
        TaskError::invalid_input(format!(
            "cron expression '{trimmed}' has no upcoming execution time"
        ))
    })?;
    Ok((trimmed.to_string(), next.timestamp_millis()))
}
