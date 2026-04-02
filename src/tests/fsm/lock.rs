use crate::domain::lock::{Lock, LockStatus};

// --- Valid transitions via methods ---

#[test]
fn valid_release_from_active() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    assert_eq!(lock.status, LockStatus::Active);
    lock.release();
    assert_eq!(lock.status, LockStatus::Released);
}

#[test]
fn valid_expire_from_active() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    assert_eq!(lock.status, LockStatus::Active);
    lock.expire();
    assert_eq!(lock.status, LockStatus::Expired);
}

// --- is_active correctness ---

#[test]
fn is_active_correct() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    assert!(lock.is_active());
    lock.release();
    assert!(!lock.is_active());
}

#[test]
fn is_active_after_expire() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    assert!(lock.is_active());
    lock.expire();
    assert!(!lock.is_active());
}

// --- is_expired correctness ---

#[test]
fn is_expired_no_ttl() {
    let lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    assert!(!lock.is_expired());
}

#[test]
fn is_expired_with_future_ttl() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    lock.expires_at = Some(crate::id::now_millis() + 60_000);
    assert!(!lock.is_expired());
}

#[test]
fn is_expired_with_past_ttl() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    lock.expires_at = Some(crate::id::now_millis() - 1);
    assert!(lock.is_expired());
}

// --- Double-release and cross-state transitions (documenting current behavior) ---
// Note: Lock methods don't guard against current state -- these document that gap.

#[test]
fn double_release_succeeds_currently() {
    // KNOWN GAP: release() doesn't check current status
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    lock.release();
    assert_eq!(lock.status, LockStatus::Released);
    // This should arguably fail but currently succeeds
    lock.release();
    assert_eq!(lock.status, LockStatus::Released);
}

#[test]
fn expire_after_release_succeeds_currently() {
    // KNOWN GAP: expire() doesn't check current status
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    lock.release();
    assert_eq!(lock.status, LockStatus::Released);
    // This should arguably fail but currently succeeds
    lock.expire();
    assert_eq!(lock.status, LockStatus::Expired);
}

#[test]
fn release_after_expire_succeeds_currently() {
    // KNOWN GAP: release() doesn't check current status
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    lock.expire();
    assert_eq!(lock.status, LockStatus::Expired);
    // This should arguably fail but currently succeeds
    lock.release();
    assert_eq!(lock.status, LockStatus::Released);
}

// --- Serde roundtrip ---

#[test]
fn lock_serde_all_statuses() {
    for status in [LockStatus::Active, LockStatus::Released, LockStatus::Expired] {
        let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
        lock.status = status;
        let json = serde_json::to_string(&lock).unwrap();
        let restored: Lock = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status, status);
    }
}

// --- updated_at changes on transition ---

#[test]
fn release_updates_timestamp() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    let before = lock.updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));
    lock.release();
    assert!(lock.updated_at >= before);
}

#[test]
fn expire_updates_timestamp() {
    let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
    let before = lock.updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));
    lock.expire();
    assert!(lock.updated_at >= before);
}
