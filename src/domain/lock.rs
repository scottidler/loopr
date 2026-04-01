use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use loopr_derive::FlexibleEnum;

use crate::id;

/// Status of an advisory lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum)]
#[serde(rename_all = "lowercase")]
pub enum LockStatus {
    Active,
    Released,
    Expired,
}

impl fmt::Display for LockStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockStatus::Active => write!(f, "Active"),
            LockStatus::Released => write!(f, "Released"),
            LockStatus::Expired => write!(f, "Expired"),
        }
    }
}

/// Advisory lock on a resource. MVP1 uses soft locks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    pub id: String,
    pub resource: String,
    pub holder_id: String,
    pub granted_by: String,
    pub status: LockStatus,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub renewable: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Lock {
    pub fn new(resource: String, holder_id: String, granted_by: String) -> Self {
        log::debug!("Lock::new(resource={}, holder_id={})", resource, holder_id);
        let now = id::now_millis();
        Self {
            id: id::generate_id("lk"),
            resource,
            holder_id,
            granted_by,
            status: LockStatus::Active,
            expires_at: None,
            renewable: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// Release this lock.
    pub fn release(&mut self) {
        self.status = LockStatus::Released;
        self.updated_at = id::now_millis();
    }

    /// Mark this lock as expired.
    pub fn expire(&mut self) {
        self.status = LockStatus::Expired;
        self.updated_at = id::now_millis();
    }

    /// Check if this lock is currently active.
    pub fn is_active(&self) -> bool {
        self.status == LockStatus::Active
    }

    /// Check if this lock has expired based on `expires_at` timestamp.
    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            crate::id::now_millis() >= expires_at
        } else {
            false
        }
    }
}

impl Record for Lock {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "locks"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert("resource".into(), IndexValue::String(self.resource.clone()));
        m.insert("holder_id".into(), IndexValue::String(self.holder_id.clone()));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // --- LockStatus tests ---

    #[test]
    fn test_lock_status_display() {
        assert_eq!(LockStatus::Active.to_string(), "Active");
        assert_eq!(LockStatus::Released.to_string(), "Released");
        assert_eq!(LockStatus::Expired.to_string(), "Expired");
    }

    #[test]
    fn test_lock_status_serde_roundtrip() {
        for status in [LockStatus::Active, LockStatus::Released, LockStatus::Expired] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: LockStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_lock_status_serde_format() {
        assert_eq!(serde_json::to_string(&LockStatus::Active).unwrap(), "\"active\"");
        assert_eq!(serde_json::to_string(&LockStatus::Released).unwrap(), "\"released\"");
        assert_eq!(serde_json::to_string(&LockStatus::Expired).unwrap(), "\"expired\"");
    }

    // --- Lock struct tests ---

    #[test]
    fn test_lock_new() {
        let lock = Lock::new("src/main.rs".to_string(), "wi-123".to_string(), "coord-456".to_string());
        assert_eq!(lock.resource, "src/main.rs");
        assert_eq!(lock.holder_id, "wi-123");
        assert_eq!(lock.granted_by, "coord-456");
        assert_eq!(lock.status, LockStatus::Active);
        assert!(lock.is_active());
        assert!(!lock.id.is_empty());
        assert!(lock.created_at > 0);
        assert_eq!(lock.created_at, lock.updated_at);
    }

    #[test]
    fn test_lock_serde_roundtrip() {
        let lock = Lock::new("src/lib.rs".to_string(), "wi-789".to_string(), "coord-101".to_string());
        let json = serde_json::to_string(&lock).unwrap();
        let deserialized: Lock = serde_json::from_str(&json).unwrap();
        assert_eq!(lock.id, deserialized.id);
        assert_eq!(lock.resource, deserialized.resource);
        assert_eq!(lock.holder_id, deserialized.holder_id);
        assert_eq!(lock.granted_by, deserialized.granted_by);
        assert_eq!(lock.status, deserialized.status);
        assert_eq!(lock.created_at, deserialized.created_at);
        assert_eq!(lock.updated_at, deserialized.updated_at);
    }

    #[test]
    fn test_lock_unique_ids() {
        let l1 = Lock::new("a".to_string(), "b".to_string(), "c".to_string());
        let l2 = Lock::new("a".to_string(), "b".to_string(), "c".to_string());
        assert_ne!(l1.id, l2.id);
    }

    #[test]
    fn test_lock_release() {
        let mut lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        assert!(lock.is_active());
        lock.release();
        assert_eq!(lock.status, LockStatus::Released);
        assert!(!lock.is_active());
    }

    #[test]
    fn test_lock_expire() {
        let mut lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        assert!(lock.is_active());
        lock.expire();
        assert_eq!(lock.status, LockStatus::Expired);
        assert!(!lock.is_active());
    }

    #[test]
    fn test_lock_is_expired_no_expiry() {
        let lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        assert!(!lock.is_expired());
    }

    #[test]
    fn test_lock_is_expired_future() {
        let mut lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        lock.expires_at = Some(crate::id::now_millis() + 60_000);
        assert!(!lock.is_expired());
    }

    #[test]
    fn test_lock_is_expired_past() {
        let mut lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        lock.expires_at = Some(crate::id::now_millis() - 1);
        assert!(lock.is_expired());
    }

    #[test]
    fn test_lock_is_active() {
        let mut lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        assert!(lock.is_active());
        lock.release();
        assert!(!lock.is_active());
    }

    #[test]
    fn test_lock_resource_preserved() {
        let lock = Lock::new(
            "src/domain/mod.rs".to_string(),
            "wi-1".to_string(),
            "coord-1".to_string(),
        );
        assert_eq!(lock.resource, "src/domain/mod.rs");
    }

    #[test]
    fn test_lock_holder_id_preserved() {
        let lock = Lock::new("file.rs".to_string(), "wi-special".to_string(), "coord-1".to_string());
        assert_eq!(lock.holder_id, "wi-special");
    }

    #[test]
    fn test_lock_granted_by_preserved() {
        let lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-special".to_string());
        assert_eq!(lock.granted_by, "coord-special");
    }

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        assert_eq!(Record::id(&lock), lock.id);
    }

    #[test]
    fn test_record_updated_at() {
        let lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        assert_eq!(Record::updated_at(&lock), lock.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(Lock::collection_name(), "locks");
    }

    #[test]
    fn test_record_indexed_fields() {
        let lock = Lock::new("src/main.rs".to_string(), "wi-42".to_string(), "coord-1".to_string());
        let fields = lock.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Active".to_string())));
        assert_eq!(
            fields.get("resource"),
            Some(&IndexValue::String("src/main.rs".to_string()))
        );
        assert_eq!(fields.get("holder_id"), Some(&IndexValue::String("wi-42".to_string())));
        assert_eq!(fields.len(), 3);
    }

    #[test]
    fn test_record_indexed_fields_reflect_status() {
        let mut lock = Lock::new("file.rs".to_string(), "wi-1".to_string(), "coord-1".to_string());
        lock.release();
        let fields = lock.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Released".to_string())));
    }
}
