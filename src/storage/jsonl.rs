//! JSONL-based storage implementation with in-memory caching.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Serialize, de::DeserializeOwned};

use super::traits::{Filter, HasId, Storage};
use crate::error::{LooprError, Result};

/// JSONL-based storage with in-memory caching.
pub struct JsonlStorage {
    base_path: PathBuf,
    cache: RwLock<HashMap<String, Vec<serde_json::Value>>>,
}

impl JsonlStorage {
    /// Create a new JsonlStorage at the given path.
    pub fn new(base_path: impl AsRef<Path>) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        fs::create_dir_all(&base_path)?;
        Ok(Self {
            base_path,
            cache: RwLock::new(HashMap::new()),
        })
    }

    /// Get the file path for a collection.
    fn collection_path(&self, collection: &str) -> PathBuf {
        self.base_path.join(format!("{}.jsonl", collection))
    }

    /// Load a collection into cache if not already loaded.
    fn ensure_loaded(&self, collection: &str) -> Result<()> {
        {
            let cache = self.cache.read().map_err(|e| LooprError::Storage(e.to_string()))?;
            if cache.contains_key(collection) {
                return Ok(());
            }
        }

        let mut cache = self.cache.write().map_err(|e| LooprError::Storage(e.to_string()))?;
        if cache.contains_key(collection) {
            return Ok(());
        }

        let path = self.collection_path(collection);
        let records = if path.exists() {
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut records = Vec::new();
            for line in reader.lines() {
                let line = line?;
                if !line.trim().is_empty() {
                    let record: serde_json::Value = serde_json::from_str(&line)?;
                    records.push(record);
                }
            }
            records
        } else {
            Vec::new()
        };

        cache.insert(collection.to_string(), records);
        Ok(())
    }

    /// Append a record to the JSONL file.
    fn append_to_file(&self, collection: &str, record: &serde_json::Value) -> Result<()> {
        let path = self.collection_path(collection);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{}", serde_json::to_string(record)?)?;
        Ok(())
    }

    /// Rewrite the entire collection file from cache.
    fn rewrite_file(&self, collection: &str) -> Result<()> {
        let cache = self.cache.read().map_err(|e| LooprError::Storage(e.to_string()))?;
        let records = cache
            .get(collection)
            .ok_or_else(|| LooprError::Storage(format!("Collection not loaded: {}", collection)))?;

        let path = self.collection_path(collection);
        let mut file = File::create(&path)?;
        for record in records {
            writeln!(file, "{}", serde_json::to_string(record)?)?;
        }
        Ok(())
    }
}

impl Storage for JsonlStorage {
    fn create<T: Serialize + DeserializeOwned + HasId>(&self, collection: &str, record: &T) -> Result<()> {
        self.ensure_loaded(collection)?;

        let value = serde_json::to_value(record)?;

        // Append to file first (source of truth)
        self.append_to_file(collection, &value)?;

        // Then update cache
        let mut cache = self.cache.write().map_err(|e| LooprError::Storage(e.to_string()))?;
        cache.get_mut(collection).unwrap().push(value);

        Ok(())
    }

    fn get<T: DeserializeOwned>(&self, collection: &str, id: &str) -> Result<Option<T>> {
        self.ensure_loaded(collection)?;

        let cache = self.cache.read().map_err(|e| LooprError::Storage(e.to_string()))?;
        let records = cache
            .get(collection)
            .ok_or_else(|| LooprError::Storage(format!("Collection not loaded: {}", collection)))?;

        for record in records {
            if record.get("id").and_then(|v| v.as_str()) == Some(id) {
                let parsed: T = serde_json::from_value(record.clone())?;
                return Ok(Some(parsed));
            }
        }

        Ok(None)
    }

    fn update<T: Serialize + DeserializeOwned + HasId>(&self, collection: &str, id: &str, record: &T) -> Result<()> {
        self.ensure_loaded(collection)?;

        let value = serde_json::to_value(record)?;

        {
            let mut cache = self.cache.write().map_err(|e| LooprError::Storage(e.to_string()))?;
            let records = cache
                .get_mut(collection)
                .ok_or_else(|| LooprError::Storage(format!("Collection not loaded: {}", collection)))?;

            let mut found = false;
            for r in records.iter_mut() {
                if r.get("id").and_then(|v| v.as_str()) == Some(id) {
                    *r = value.clone();
                    found = true;
                    break;
                }
            }

            if !found {
                return Err(LooprError::LoopNotFound(id.to_string()));
            }
        }

        // Rewrite file with updated cache
        self.rewrite_file(collection)?;

        Ok(())
    }

    fn delete(&self, collection: &str, id: &str) -> Result<()> {
        self.ensure_loaded(collection)?;

        {
            let mut cache = self.cache.write().map_err(|e| LooprError::Storage(e.to_string()))?;
            let records = cache
                .get_mut(collection)
                .ok_or_else(|| LooprError::Storage(format!("Collection not loaded: {}", collection)))?;

            let original_len = records.len();
            records.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(id));

            if records.len() == original_len {
                return Err(LooprError::LoopNotFound(id.to_string()));
            }
        }

        // Rewrite file with updated cache
        self.rewrite_file(collection)?;

        Ok(())
    }

    fn query<T: DeserializeOwned>(&self, collection: &str, filters: &[Filter]) -> Result<Vec<T>> {
        self.ensure_loaded(collection)?;

        let cache = self.cache.read().map_err(|e| LooprError::Storage(e.to_string()))?;
        let records = cache
            .get(collection)
            .ok_or_else(|| LooprError::Storage(format!("Collection not loaded: {}", collection)))?;

        let mut results = Vec::new();
        for record in records {
            let matches = filters.iter().all(|f| f.matches(record));
            if matches {
                let parsed: T = serde_json::from_value(record.clone())?;
                results.push(parsed);
            }
        }

        Ok(results)
    }

    fn list<T: DeserializeOwned>(&self, collection: &str) -> Result<Vec<T>> {
        self.query(collection, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestRecord {
        id: String,
        name: String,
        status: String,
    }

    impl HasId for TestRecord {
        fn id(&self) -> &str {
            &self.id
        }
    }

    fn create_test_storage() -> (JsonlStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = JsonlStorage::new(temp_dir.path()).unwrap();
        (storage, temp_dir)
    }

    #[test]
    fn test_create_and_get() {
        let (storage, _temp) = create_test_storage();
        let record = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "active".to_string(),
        };

        storage.create("test", &record).unwrap();
        let retrieved: Option<TestRecord> = storage.get("test", "1").unwrap();

        assert_eq!(retrieved, Some(record));
    }

    #[test]
    fn test_get_not_found() {
        let (storage, _temp) = create_test_storage();
        let retrieved: Option<TestRecord> = storage.get("test", "nonexistent").unwrap();
        assert_eq!(retrieved, None);
    }

    #[test]
    fn test_update() {
        let (storage, _temp) = create_test_storage();
        let record = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "active".to_string(),
        };

        storage.create("test", &record).unwrap();

        let updated = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "completed".to_string(),
        };

        storage.update("test", "1", &updated).unwrap();
        let retrieved: Option<TestRecord> = storage.get("test", "1").unwrap();

        assert_eq!(retrieved, Some(updated));
    }

    #[test]
    fn test_update_not_found() {
        let (storage, _temp) = create_test_storage();
        let record = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "active".to_string(),
        };

        let result = storage.update("test", "1", &record);
        assert!(result.is_err());
    }

    #[test]
    fn test_delete() {
        let (storage, _temp) = create_test_storage();
        let record = TestRecord {
            id: "1".to_string(),
            name: "test".to_string(),
            status: "active".to_string(),
        };

        storage.create("test", &record).unwrap();
        storage.delete("test", "1").unwrap();

        let retrieved: Option<TestRecord> = storage.get("test", "1").unwrap();
        assert_eq!(retrieved, None);
    }

    #[test]
    fn test_delete_not_found() {
        let (storage, _temp) = create_test_storage();
        let result = storage.delete("test", "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_query_with_filters() {
        let (storage, _temp) = create_test_storage();

        storage
            .create(
                "test",
                &TestRecord {
                    id: "1".to_string(),
                    name: "alice".to_string(),
                    status: "active".to_string(),
                },
            )
            .unwrap();

        storage
            .create(
                "test",
                &TestRecord {
                    id: "2".to_string(),
                    name: "bob".to_string(),
                    status: "inactive".to_string(),
                },
            )
            .unwrap();

        storage
            .create(
                "test",
                &TestRecord {
                    id: "3".to_string(),
                    name: "charlie".to_string(),
                    status: "active".to_string(),
                },
            )
            .unwrap();

        let active: Vec<TestRecord> = storage.query("test", &[Filter::eq("status", "active")]).unwrap();

        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|r| r.status == "active"));
    }

    #[test]
    fn test_list() {
        let (storage, _temp) = create_test_storage();

        storage
            .create(
                "test",
                &TestRecord {
                    id: "1".to_string(),
                    name: "one".to_string(),
                    status: "active".to_string(),
                },
            )
            .unwrap();

        storage
            .create(
                "test",
                &TestRecord {
                    id: "2".to_string(),
                    name: "two".to_string(),
                    status: "active".to_string(),
                },
            )
            .unwrap();

        let all: Vec<TestRecord> = storage.list("test").unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_persistence_across_instances() {
        let temp_dir = TempDir::new().unwrap();

        // Create and write with first instance
        {
            let storage = JsonlStorage::new(temp_dir.path()).unwrap();
            storage
                .create(
                    "test",
                    &TestRecord {
                        id: "1".to_string(),
                        name: "test".to_string(),
                        status: "active".to_string(),
                    },
                )
                .unwrap();
        }

        // Read with second instance
        {
            let storage = JsonlStorage::new(temp_dir.path()).unwrap();
            let retrieved: Option<TestRecord> = storage.get("test", "1").unwrap();
            assert!(retrieved.is_some());
            assert_eq!(retrieved.unwrap().name, "test");
        }
    }

    #[test]
    fn test_empty_collection() {
        let (storage, _temp) = create_test_storage();
        let all: Vec<TestRecord> = storage.list("empty").unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn test_multiple_collections() {
        let (storage, _temp) = create_test_storage();

        storage
            .create(
                "collection_a",
                &TestRecord {
                    id: "1".to_string(),
                    name: "in_a".to_string(),
                    status: "active".to_string(),
                },
            )
            .unwrap();

        storage
            .create(
                "collection_b",
                &TestRecord {
                    id: "1".to_string(),
                    name: "in_b".to_string(),
                    status: "active".to_string(),
                },
            )
            .unwrap();

        let a: Option<TestRecord> = storage.get("collection_a", "1").unwrap();
        let b: Option<TestRecord> = storage.get("collection_b", "1").unwrap();

        assert_eq!(a.unwrap().name, "in_a");
        assert_eq!(b.unwrap().name, "in_b");
    }
}
