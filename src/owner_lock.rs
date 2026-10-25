//! Exclusive process owner lock cho `tasks.db` (ADR-0001).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::error::{ErrorCode, TaskError, TaskResult};

#[derive(Debug)]
pub(crate) struct OwnerLock {
    _file: File,
}

impl OwnerLock {
    pub(crate) fn acquire(path: &Path) -> TaskResult<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| TaskError::storage("open tasks owner lock", e))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(TaskError::new(
                ErrorCode::StoreAlreadyOwned,
                "task store is already owned by another TaskManager instance",
            )),
            Err(TryLockError::Error(e)) => Err(TaskError::storage("lock tasks owner file", e)),
        }
    }
}

#[cfg(test)]
#[path = "owner_lock_tests.rs"]
mod tests;
