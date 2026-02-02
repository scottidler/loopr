//! Storage layer for Loopr - TaskStore-backed persistence with SQLite caching.
//!
//! This module provides the storage abstraction for persisting Loop, Signal,
//! ToolJob, and Event records using the taskstore library.

mod loops;

use std::path::Path;
use std::sync::Mutex;

use taskstore::Store;

use crate::error::{LooprError, Result};

// Re-export taskstore types for use by callers
pub use loops::LoopStore;
pub use taskstore::{Filter, FilterOp, IndexValue, Record};

/// Wrapper around taskstore::Store providing interior mutability and error conversion.
///
/// TaskStore's `Store` requires `&mut self` for write operations, but Loopr's
/// storage is typically behind an `Arc`. This wrapper uses `Mutex` for interior
/// mutability since rusqlite::Connection isn't Sync (it uses RefCell internally).
/// Mutex is appropriate here because SQLite operations are quick and we need
/// exclusive access anyway.
pub struct StorageWrapper {
    inner: Mutex<Store>,
}

impl std::fmt::Debug for StorageWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageWrapper").finish_non_exhaustive()
    }
}

impl StorageWrapper {
    /// Open or create a storage at the given path.
    ///
    /// Creates a `.taskstore` subdirectory at the given path for JSONL files
    /// and SQLite database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = Store::open(path).map_err(|e| LooprError::Storage(e.to_string()))?;
        Ok(Self {
            inner: Mutex::new(store),
        })
    }

    /// Create a new record.
    pub fn create<T: Record>(&self, record: &T) -> Result<()> {
        self.inner
            .lock()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .create(record.clone())
            .map_err(|e| LooprError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Get a record by ID.
    pub fn get<T: Record>(&self, id: &str) -> Result<Option<T>> {
        self.inner
            .lock()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .get(id)
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    /// Update a record.
    pub fn update<T: Record>(&self, record: &T) -> Result<()> {
        self.inner
            .lock()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .update(record.clone())
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    /// Delete a record by ID.
    pub fn delete<T: Record>(&self, id: &str) -> Result<()> {
        self.inner
            .lock()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .delete::<T>(id)
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    /// List records with optional filters.
    pub fn list<T: Record>(&self, filters: &[Filter]) -> Result<Vec<T>> {
        self.inner
            .lock()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .list(filters)
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    /// List all records of a type (no filters).
    pub fn list_all<T: Record>(&self) -> Result<Vec<T>> {
        self.list(&[])
    }

    /// Rebuild indexes for a record type after sync.
    ///
    /// Call this for each record type after opening the store to ensure
    /// indexes are up to date.
    pub fn rebuild_indexes<T: Record>(&self) -> Result<usize> {
        self.inner
            .lock()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .rebuild_indexes::<T>()
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    /// Install git hooks for automatic sync on git operations.
    pub fn install_git_hooks(&self) -> Result<()> {
        self.inner
            .lock()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .install_git_hooks()
            .map_err(|e| LooprError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestRecord {
        id: String,
        name: String,
        status: String,
        updated_at: i64,
    }

    impl Record for TestRecord {
        fn id(&self) -> &str {
            &self.id
        }

        fn updated_at(&self) -> i64 {
            self.updated_at
        }

        fn collection_name() -> &'static str {
            "test_records"
        }

        fn indexed_fields(&self) -> HashMap<String, IndexValue> {
            let mut fields = HashMap::new();
            fields.insert("status".to_string(), IndexValue::String(self.status.clone()));
            fields
        }
    }

    fn create_test_storage() -> (StorageWrapper, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = StorageWrapper::open(temp_dir.path()).unwrap();
        (storage, temp_dir)
    }

    #[test]
    fn test_create_and_get() {
        let (storage, _temp) = create_test_storage();
        let record = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "active".to_string(),
            updated_at: 1000,
        };

        storage.create(&record).unwrap();
        let retrieved: Option<TestRecord> = storage.get("1").unwrap();

        assert_eq!(retrieved, Some(record));
    }

    #[test]
    fn test_get_not_found() {
        let (storage, _temp) = create_test_storage();
        let retrieved: Option<TestRecord> = storage.get("nonexistent").unwrap();
        assert_eq!(retrieved, None);
    }

    #[test]
    fn test_update() {
        let (storage, _temp) = create_test_storage();
        let record = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "active".to_string(),
            updated_at: 1000,
        };

        storage.create(&record).unwrap();

        let updated = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "completed".to_string(),
            updated_at: 2000,
        };

        storage.update(&updated).unwrap();
        let retrieved: Option<TestRecord> = storage.get("1").unwrap();

        assert_eq!(retrieved, Some(updated));
    }

    #[test]
    fn test_delete() {
        let (storage, _temp) = create_test_storage();
        let record = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "active".to_string(),
            updated_at: 1000,
        };

        storage.create(&record).unwrap();
        storage.delete::<TestRecord>("1").unwrap();

        let retrieved: Option<TestRecord> = storage.get("1").unwrap();
        assert_eq!(retrieved, None);
    }

    #[test]
    fn test_list_all() {
        let (storage, _temp) = create_test_storage();

        storage
            .create(&TestRecord {
                id: "1".to_string(),
                name: "one".to_string(),
                status: "active".to_string(),
                updated_at: 1000,
            })
            .unwrap();

        storage
            .create(&TestRecord {
                id: "2".to_string(),
                name: "two".to_string(),
                status: "active".to_string(),
                updated_at: 2000,
            })
            .unwrap();

        let all: Vec<TestRecord> = storage.list_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_list_with_filters() {
        let (storage, _temp) = create_test_storage();

        storage
            .create(&TestRecord {
                id: "1".to_string(),
                name: "alice".to_string(),
                status: "active".to_string(),
                updated_at: 1000,
            })
            .unwrap();

        storage
            .create(&TestRecord {
                id: "2".to_string(),
                name: "bob".to_string(),
                status: "inactive".to_string(),
                updated_at: 2000,
            })
            .unwrap();

        storage
            .create(&TestRecord {
                id: "3".to_string(),
                name: "charlie".to_string(),
                status: "active".to_string(),
                updated_at: 3000,
            })
            .unwrap();

        let filters = vec![Filter {
            field: "status".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String("active".to_string()),
        }];

        let active: Vec<TestRecord> = storage.list(&filters).unwrap();

        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|r| r.status == "active"));
    }

    #[test]
    fn test_empty_collection() {
        let (storage, _temp) = create_test_storage();
        let all: Vec<TestRecord> = storage.list_all().unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn test_rebuild_indexes() {
        let (storage, _temp) = create_test_storage();

        storage
            .create(&TestRecord {
                id: "1".to_string(),
                name: "test".to_string(),
                status: "active".to_string(),
                updated_at: 1000,
            })
            .unwrap();

        let count = storage.rebuild_indexes::<TestRecord>().unwrap();
        assert_eq!(count, 1);
    }
}
