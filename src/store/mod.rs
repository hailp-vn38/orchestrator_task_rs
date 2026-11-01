//! `TaskStore`: sở hữu tập trung 100% câu lệnh SQLite và transaction invariants của `tasks.db`.
//! Tuyệt đối không có câu lệnh SQL nào nằm ngoài thư mục `src/store/`.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::error::{TaskError, TaskResult};

pub(crate) mod codec;
mod collection_ops;
mod followup_ops;
mod recovery_ops;
mod schedule_ops;
mod schedule_query;
mod task_ops;
mod task_query;

pub(crate) use codec::{new_id, now_ms};
pub(crate) use collection_ops::{CollectionItemRow, ITEM_FAILED, ITEM_NO_TRANSCRIPT, ITEM_SAVED};
pub(crate) use schedule_ops::NewScheduleRow;
pub(crate) use task_ops::{NewTaskRow, ProgressUpdate, TerminalUpdate};

pub(crate) const DB_FILE: &str = "tasks.db";
pub(crate) const OWNER_LOCK_FILE: &str = "tasks.owner.lock";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const POOL_MAX_CONNECTIONS: u32 = 5;
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    pub(crate) async fn open(path: &Path) -> TaskResult<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(POOL_MAX_CONNECTIONS)
            .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|e| TaskError::storage("open tasks.db pool", e))?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| TaskError::storage("migrate tasks.db", e))?;
        Ok(Self { pool })
    }

    pub(crate) async fn close(&self) {
        self.pool.close().await;
    }
}
