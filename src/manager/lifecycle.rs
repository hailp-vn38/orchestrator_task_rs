//! Giao thức đóng (`shutdown`) của `TaskManager`: hủy hợp tác các background worker,
//! chờ join trong `shutdown_grace`, đóng `tasks.db` pool rồi mới nhả `OwnerLock`.

use std::mem;

use crate::error::TaskResult;
use crate::manager::TaskManager;
use crate::runtime::lock;

impl TaskManager {
    pub async fn shutdown(&self) -> TaskResult<()> {
        self.core.shutdown.cancel();

        let bg_handles = mem::take(&mut *lock(&self.bg_tasks));
        let watcher_handles: Vec<_> = lock(&self.core.active_watchers)
            .drain()
            .map(|(_, h)| h)
            .collect();

        let grace = self.core.config.runtime.shutdown_grace;
        let join_all = async move {
            for h in bg_handles {
                let _ = h.await;
            }
            for h in watcher_handles {
                let _ = h.await;
            }
        };
        let _ = tokio::time::timeout(grace, join_all).await;

        self.core.store.close().await;
        drop(lock(&self.owner_lock).take());
        Ok(())
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        self.core.shutdown.cancel();
    }
}
