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
mod tests {
    use super::*;

    #[test]
    fn exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tasks.owner.lock");
        let lock_a = OwnerLock::acquire(&p).unwrap();
        let err = OwnerLock::acquire(&p).unwrap_err();
        assert_eq!(err.code(), ErrorCode::StoreAlreadyOwned);
        drop(lock_a);
        assert!(OwnerLock::acquire(&p).is_ok());
    }
}
