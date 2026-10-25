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
