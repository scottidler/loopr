//! Loop-specific storage helpers.

use super::traits::{Filter, Storage};
use crate::domain::loop_record::{Loop, LoopStatus};
use crate::error::Result;

/// Collection name for loops.
pub const LOOPS_COLLECTION: &str = "loops";

/// Helper for loop-specific queries.
pub struct LoopStore<'a, S: Storage> {
    storage: &'a S,
}

impl<'a, S: Storage> LoopStore<'a, S> {
    /// Create a new LoopStore wrapping the given storage.
    pub fn new(storage: &'a S) -> Self {
        Self { storage }
    }

    /// Find all loops with a specific status.
    pub fn find_by_status(&self, status: LoopStatus) -> Result<Vec<Loop>> {
        let status_str = serde_json::to_value(status)?;
        self.storage
            .query(LOOPS_COLLECTION, &[Filter::eq("status", status_str)])
    }

    /// Find all child loops of a parent.
    pub fn find_by_parent(&self, parent_id: &str) -> Result<Vec<Loop>> {
        self.storage
            .query(LOOPS_COLLECTION, &[Filter::eq("parent_id", parent_id)])
    }

    /// Find all pending loops.
    pub fn find_pending(&self) -> Result<Vec<Loop>> {
        self.find_by_status(LoopStatus::Pending)
    }

    /// Find all running loops.
    pub fn find_running(&self) -> Result<Vec<Loop>> {
        self.find_by_status(LoopStatus::Running)
    }

    /// Find all complete loops.
    pub fn find_complete(&self) -> Result<Vec<Loop>> {
        self.find_by_status(LoopStatus::Complete)
    }

    /// Find all failed loops.
    pub fn find_failed(&self) -> Result<Vec<Loop>> {
        self.find_by_status(LoopStatus::Failed)
    }

    /// List all loops.
    pub fn list_all(&self) -> Result<Vec<Loop>> {
        self.storage.list(LOOPS_COLLECTION)
    }

    /// Get a loop by ID.
    pub fn get(&self, id: &str) -> Result<Option<Loop>> {
        self.storage.get(LOOPS_COLLECTION, id)
    }

    /// Create a new loop.
    pub fn create(&self, record: &Loop) -> Result<()> {
        self.storage.create(LOOPS_COLLECTION, record)
    }

    /// Update an existing loop.
    pub fn update(&self, record: &Loop) -> Result<()> {
        self.storage.update(LOOPS_COLLECTION, &record.id, record)
    }

    /// Delete a loop.
    pub fn delete(&self, id: &str) -> Result<()> {
        self.storage.delete(LOOPS_COLLECTION, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::loop_record::LoopType;
    use crate::storage::JsonlStorage;
    use tempfile::TempDir;

    fn create_test_storage() -> (JsonlStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = JsonlStorage::new(temp_dir.path()).unwrap();
        (storage, temp_dir)
    }

    #[test]
    fn test_create_and_get_loop() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let record = Loop::new_plan("Test task");
        loop_store.create(&record).unwrap();

        let retrieved = loop_store.get(&record.id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id, record.id);
    }

    #[test]
    fn test_find_by_status() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let plan1 = Loop::new_plan("Task 1");
        let plan2 = Loop::new_plan("Task 2");

        loop_store.create(&plan1).unwrap();
        loop_store.create(&plan2).unwrap();

        // Both should be Pending by default
        let pending = loop_store.find_pending().unwrap();
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn test_find_by_parent() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let parent = Loop::new_plan("Parent task");
        let child1 = Loop::new_spec(&parent, 0);
        let child2 = Loop::new_spec(&parent, 1);

        loop_store.create(&parent).unwrap();
        loop_store.create(&child1).unwrap();
        loop_store.create(&child2).unwrap();

        let children = loop_store.find_by_parent(&parent.id).unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn test_update_loop() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let mut record = Loop::new_plan("Test task");
        loop_store.create(&record).unwrap();

        record.status = LoopStatus::Running;
        record.iteration = 1;
        loop_store.update(&record).unwrap();

        let retrieved = loop_store.get(&record.id).unwrap().unwrap();
        assert_eq!(retrieved.status, LoopStatus::Running);
        assert_eq!(retrieved.iteration, 1);
    }

    #[test]
    fn test_delete_loop() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let record = Loop::new_plan("Test task");
        loop_store.create(&record).unwrap();

        loop_store.delete(&record.id).unwrap();

        let retrieved = loop_store.get(&record.id).unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_list_all() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        loop_store.create(&Loop::new_plan("Task 1")).unwrap();
        loop_store.create(&Loop::new_plan("Task 2")).unwrap();
        loop_store.create(&Loop::new_plan("Task 3")).unwrap();

        let all = loop_store.list_all().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_find_running() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let mut record = Loop::new_plan("Test task");
        record.status = LoopStatus::Running;
        loop_store.create(&record).unwrap();

        let running = loop_store.find_running().unwrap();
        assert_eq!(running.len(), 1);
    }

    #[test]
    fn test_find_complete() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let mut record = Loop::new_plan("Test task");
        record.status = LoopStatus::Complete;
        loop_store.create(&record).unwrap();

        let complete = loop_store.find_complete().unwrap();
        assert_eq!(complete.len(), 1);
    }

    #[test]
    fn test_find_failed() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let mut record = Loop::new_plan("Test task");
        record.status = LoopStatus::Failed;
        loop_store.create(&record).unwrap();

        let failed = loop_store.find_failed().unwrap();
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn test_nested_hierarchy() {
        let (storage, _temp) = create_test_storage();
        let loop_store = LoopStore::new(&storage);

        let plan = Loop::new_plan("Build feature");
        let spec = Loop::new_spec(&plan, 0);
        let phase = Loop::new_phase(&spec, 0, "Phase 1", 3);
        let code = Loop::new_code(&phase);

        loop_store.create(&plan).unwrap();
        loop_store.create(&spec).unwrap();
        loop_store.create(&phase).unwrap();
        loop_store.create(&code).unwrap();

        // Verify hierarchy
        assert!(loop_store.get(&plan.id).unwrap().is_some());
        assert!(loop_store.get(&spec.id).unwrap().is_some());
        assert!(loop_store.get(&phase.id).unwrap().is_some());
        assert!(loop_store.get(&code.id).unwrap().is_some());

        // Verify parent relationships
        let plan_children = loop_store.find_by_parent(&plan.id).unwrap();
        assert_eq!(plan_children.len(), 1);
        assert_eq!(plan_children[0].loop_type, LoopType::Spec);

        let spec_children = loop_store.find_by_parent(&spec.id).unwrap();
        assert_eq!(spec_children.len(), 1);
        assert_eq!(spec_children[0].loop_type, LoopType::Phase);
    }
}
