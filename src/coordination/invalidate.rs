//! Invalidation cascade for loop coordination
//!
//! Handles invalidating descendant loops when a parent loop re-iterates.

use std::sync::Arc;

use crate::domain::loop_record::{Loop, LoopStatus};
use crate::domain::signal::{SignalRecord, SignalType};
use crate::error::Result;
use crate::storage::{Filter, Storage};

/// Collection name for loops in storage
const LOOPS_COLLECTION: &str = "loops";

/// Collection name for signals in storage
const SIGNALS_COLLECTION: &str = "signals";

/// Manages invalidation cascade for loop hierarchies
pub struct InvalidationManager<S: Storage> {
    storage: Arc<S>,
}

impl<S: Storage> InvalidationManager<S> {
    /// Create a new InvalidationManager with the given storage
    pub fn new(storage: Arc<S>) -> Self {
        Self { storage }
    }

    /// Find all descendants of a parent loop
    ///
    /// A loop is a descendant if its parent_id matches the parent or any
    /// of the parent's descendants (recursive).
    pub fn find_descendants(&self, parent_id: &str) -> Result<Vec<Loop>> {
        let mut descendants = Vec::new();
        let mut to_check = vec![parent_id.to_string()];

        while let Some(current_id) = to_check.pop() {
            // Find all loops with this parent_id
            let filters = vec![Filter::eq("parent_id", &current_id)];
            let children: Vec<Loop> = self.storage.query(LOOPS_COLLECTION, &filters)?;

            for child in children {
                // Add this child's ID to check for its descendants
                to_check.push(child.id.clone());
                descendants.push(child);
            }
        }

        Ok(descendants)
    }

    /// Invalidate all descendants of a parent loop
    ///
    /// This sends a Stop signal to each descendant and updates their status
    /// to Invalidated. Returns the count of invalidated loops.
    pub fn invalidate_descendants(&self, parent_id: &str, reason: &str) -> Result<u32> {
        let descendants = self.find_descendants(parent_id)?;
        let count = descendants.len() as u32;

        for mut descendant in descendants {
            // Skip loops that are already in a terminal state
            if descendant.status.is_terminal() {
                continue;
            }

            // Send a stop signal to this descendant
            let signal = SignalRecord::new(SignalType::Invalidate, reason)
                .from_loop(parent_id)
                .to_loop(&descendant.id);
            self.storage.create(SIGNALS_COLLECTION, &signal)?;

            // Update the loop status to Invalidated
            descendant.status = LoopStatus::Invalidated;
            self.storage
                .update(LOOPS_COLLECTION, &descendant.id, &descendant)?;
        }

        Ok(count)
    }

    /// Check if a loop is a descendant of another loop
    pub fn is_descendant_of(&self, loop_id: &str, potential_ancestor: &str) -> Result<bool> {
        let loop_record: Option<Loop> = self.storage.get(LOOPS_COLLECTION, loop_id)?;

        if let Some(record) = loop_record {
            if let Some(parent_id) = &record.parent_id {
                if parent_id == potential_ancestor {
                    return Ok(true);
                }
                // Recursively check the parent
                return self.is_descendant_of(parent_id, potential_ancestor);
            }
        }

        Ok(false)
    }

    /// Get the ancestor chain for a loop (from loop to root)
    pub fn get_ancestor_chain(&self, loop_id: &str) -> Result<Vec<String>> {
        let mut chain = Vec::new();
        let mut current_id = loop_id.to_string();

        loop {
            let loop_record: Option<Loop> = self.storage.get(LOOPS_COLLECTION, &current_id)?;

            if let Some(record) = loop_record {
                if let Some(parent_id) = record.parent_id {
                    chain.push(parent_id.clone());
                    current_id = parent_id;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(chain)
    }

    /// Check if a loop should be invalidated based on selector signals
    pub fn check_invalidation(&self, loop_id: &str) -> Result<Option<SignalRecord>> {
        // Get the ancestor chain for this loop
        let ancestors = self.get_ancestor_chain(loop_id)?;

        // Check for invalidation signals targeting descendants of any ancestor
        for ancestor in ancestors {
            let selector = format!("descendants:{}", ancestor);
            let filters = vec![
                Filter::eq("target_selector", &selector),
                Filter::eq("acknowledged_at", serde_json::Value::Null),
            ];
            let signals: Vec<SignalRecord> = self.storage.query(SIGNALS_COLLECTION, &filters)?;

            if let Some(signal) = signals.into_iter().next() {
                return Ok(Some(signal));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::JsonlStorage;
    use tempfile::TempDir;

    fn create_test_storage() -> (TempDir, Arc<JsonlStorage>) {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(JsonlStorage::new(temp_dir.path()).unwrap());
        (temp_dir, storage)
    }

    fn create_test_loop(id: &str, parent_id: Option<&str>) -> Loop {
        let mut l = Loop::new_plan("Test task");
        l.id = id.to_string();
        l.parent_id = parent_id.map(String::from);
        l
    }

    #[test]
    fn test_invalidation_manager_new() {
        let (_temp, storage) = create_test_storage();
        let _manager = InvalidationManager::new(storage);
    }

    #[test]
    fn test_find_descendants_empty() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage);

        let descendants = manager.find_descendants("nonexistent").unwrap();
        assert!(descendants.is_empty());
    }

    #[test]
    fn test_find_descendants_direct_children() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        // Create parent and children
        let parent = create_test_loop("parent-001", None);
        let child1 = create_test_loop("child-001", Some("parent-001"));
        let child2 = create_test_loop("child-002", Some("parent-001"));

        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child1).unwrap();
        storage.create(LOOPS_COLLECTION, &child2).unwrap();

        let descendants = manager.find_descendants("parent-001").unwrap();
        assert_eq!(descendants.len(), 2);
    }

    #[test]
    fn test_find_descendants_nested() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        // Create nested hierarchy: grandparent -> parent -> child
        let grandparent = create_test_loop("gp-001", None);
        let parent = create_test_loop("p-001", Some("gp-001"));
        let child = create_test_loop("c-001", Some("p-001"));

        storage.create(LOOPS_COLLECTION, &grandparent).unwrap();
        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        let descendants = manager.find_descendants("gp-001").unwrap();
        assert_eq!(descendants.len(), 2);

        let parent_descendants = manager.find_descendants("p-001").unwrap();
        assert_eq!(parent_descendants.len(), 1);
    }

    #[test]
    fn test_invalidate_descendants() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        // Create parent and child
        let parent = create_test_loop("parent-002", None);
        let mut child = create_test_loop("child-003", Some("parent-002"));
        child.status = LoopStatus::Running;

        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        let count = manager
            .invalidate_descendants("parent-002", "Parent re-iterated")
            .unwrap();
        assert_eq!(count, 1);

        // Verify child status was updated
        let updated: Option<Loop> = storage.get(LOOPS_COLLECTION, "child-003").unwrap();
        assert!(updated.is_some());
        assert_eq!(updated.unwrap().status, LoopStatus::Invalidated);
    }

    #[test]
    fn test_invalidate_skips_terminal_loops() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        // Create parent and already-complete child
        let parent = create_test_loop("parent-003", None);
        let mut child = create_test_loop("child-004", Some("parent-003"));
        child.status = LoopStatus::Complete;

        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        let count = manager
            .invalidate_descendants("parent-003", "Reason")
            .unwrap();
        assert_eq!(count, 1);

        // Child should still be Complete (not changed to Invalidated)
        let updated: Option<Loop> = storage.get(LOOPS_COLLECTION, "child-004").unwrap();
        assert_eq!(updated.unwrap().status, LoopStatus::Complete);
    }

    #[test]
    fn test_is_descendant_of_direct() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        let parent = create_test_loop("parent-004", None);
        let child = create_test_loop("child-005", Some("parent-004"));

        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        assert!(manager.is_descendant_of("child-005", "parent-004").unwrap());
        assert!(!manager.is_descendant_of("parent-004", "child-005").unwrap());
    }

    #[test]
    fn test_is_descendant_of_nested() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        let grandparent = create_test_loop("gp-002", None);
        let parent = create_test_loop("p-002", Some("gp-002"));
        let child = create_test_loop("c-002", Some("p-002"));

        storage.create(LOOPS_COLLECTION, &grandparent).unwrap();
        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        assert!(manager.is_descendant_of("c-002", "gp-002").unwrap());
        assert!(manager.is_descendant_of("c-002", "p-002").unwrap());
        assert!(manager.is_descendant_of("p-002", "gp-002").unwrap());
    }

    #[test]
    fn test_is_descendant_of_nonexistent() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage);

        assert!(!manager.is_descendant_of("nonexistent", "also-nonexistent").unwrap());
    }

    #[test]
    fn test_get_ancestor_chain_empty() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        let root = create_test_loop("root-001", None);
        storage.create(LOOPS_COLLECTION, &root).unwrap();

        let chain = manager.get_ancestor_chain("root-001").unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_get_ancestor_chain_nested() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        let grandparent = create_test_loop("gp-003", None);
        let parent = create_test_loop("p-003", Some("gp-003"));
        let child = create_test_loop("c-003", Some("p-003"));

        storage.create(LOOPS_COLLECTION, &grandparent).unwrap();
        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        let chain = manager.get_ancestor_chain("c-003").unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0], "p-003");
        assert_eq!(chain[1], "gp-003");
    }

    #[test]
    fn test_check_invalidation_no_signals() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        let parent = create_test_loop("parent-005", None);
        let child = create_test_loop("child-006", Some("parent-005"));

        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        let signal = manager.check_invalidation("child-006").unwrap();
        assert!(signal.is_none());
    }

    #[test]
    fn test_check_invalidation_with_signal() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        let parent = create_test_loop("parent-006", None);
        let child = create_test_loop("child-007", Some("parent-006"));

        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        // Send invalidation signal targeting descendants of parent
        let signal = SignalRecord::new(SignalType::Invalidate, "Test invalidation")
            .to_selector("descendants:parent-006");
        storage.create(SIGNALS_COLLECTION, &signal).unwrap();

        let found = manager.check_invalidation("child-007").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().signal_type, SignalType::Invalidate);
    }

    #[test]
    fn test_invalidation_creates_signals() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        let parent = create_test_loop("parent-007", None);
        let mut child = create_test_loop("child-008", Some("parent-007"));
        child.status = LoopStatus::Running;

        storage.create(LOOPS_COLLECTION, &parent).unwrap();
        storage.create(LOOPS_COLLECTION, &child).unwrap();

        manager
            .invalidate_descendants("parent-007", "Test reason")
            .unwrap();

        // Check that a signal was created for the child
        let filters = vec![Filter::eq("target_loop", "child-008")];
        let signals: Vec<SignalRecord> = storage.query(SIGNALS_COLLECTION, &filters).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].signal_type, SignalType::Invalidate);
    }

    #[test]
    fn test_find_descendants_multiple_branches() {
        let (_temp, storage) = create_test_storage();
        let manager = InvalidationManager::new(storage.clone());

        // Create tree: root -> [branch1, branch2] -> [leaf1, leaf2]
        let root = create_test_loop("root-002", None);
        let branch1 = create_test_loop("branch1", Some("root-002"));
        let branch2 = create_test_loop("branch2", Some("root-002"));
        let leaf1 = create_test_loop("leaf1", Some("branch1"));
        let leaf2 = create_test_loop("leaf2", Some("branch2"));

        storage.create(LOOPS_COLLECTION, &root).unwrap();
        storage.create(LOOPS_COLLECTION, &branch1).unwrap();
        storage.create(LOOPS_COLLECTION, &branch2).unwrap();
        storage.create(LOOPS_COLLECTION, &leaf1).unwrap();
        storage.create(LOOPS_COLLECTION, &leaf2).unwrap();

        let descendants = manager.find_descendants("root-002").unwrap();
        assert_eq!(descendants.len(), 4);
    }
}
