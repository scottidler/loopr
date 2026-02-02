//! Signal management for loop coordination
//!
//! SignalManager handles sending, checking, and acknowledging signals
//! between loops for coordination purposes (stop, pause, resume, invalidate).

use std::sync::Arc;

use taskstore::{Filter, FilterOp, IndexValue};

use crate::domain::signal::{SignalRecord, SignalType};
use crate::error::Result;
use crate::id::now_ms;
use crate::storage::StorageWrapper;

/// Manages signal-based coordination between loops
pub struct SignalManager {
    storage: Arc<StorageWrapper>,
}

impl SignalManager {
    /// Create a new SignalManager with the given storage
    pub fn new(storage: Arc<StorageWrapper>) -> Self {
        Self { storage }
    }

    /// Write a signal to storage
    pub fn send(&self, signal: SignalRecord) -> Result<()> {
        self.storage.create(&signal)
    }

    /// Check for unacknowledged signals targeting a specific loop
    pub fn check(&self, loop_id: &str) -> Result<Option<SignalRecord>> {
        let filters = vec![
            Filter {
                field: "target_loop".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(loop_id.to_string()),
            },
            Filter {
                field: "acknowledged".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::Bool(false),
            },
        ];
        let signals: Vec<SignalRecord> = self.storage.list(&filters)?;

        // Return the first unacknowledged signal for this loop
        Ok(signals.into_iter().next())
    }

    /// Check for signals matching a selector (e.g., "descendants:001")
    pub fn check_selector(&self, selector: &str) -> Result<Vec<SignalRecord>> {
        let filters = vec![
            Filter {
                field: "target_selector".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(selector.to_string()),
            },
            Filter {
                field: "acknowledged".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::Bool(false),
            },
        ];
        self.storage.list(&filters)
    }

    /// Acknowledge a signal by ID
    pub fn acknowledge(&self, signal_id: &str) -> Result<()> {
        let signal: Option<SignalRecord> = self.storage.get(signal_id)?;
        if let Some(mut signal) = signal {
            signal.acknowledged_at = Some(now_ms());
            self.storage.update(&signal)?;
        }
        Ok(())
    }

    /// Get all unacknowledged signals
    pub fn pending(&self) -> Result<Vec<SignalRecord>> {
        let filters = vec![Filter {
            field: "acknowledged".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::Bool(false),
        }];
        self.storage.list(&filters)
    }

    /// Send a stop signal to a specific loop
    pub fn send_stop(&self, target_loop_id: &str, reason: &str) -> Result<()> {
        let signal = SignalRecord::new(SignalType::Stop, reason).to_loop(target_loop_id);
        self.send(signal)
    }

    /// Send a pause signal to a specific loop
    pub fn send_pause(&self, target_loop_id: &str, reason: &str) -> Result<()> {
        let signal = SignalRecord::new(SignalType::Pause, reason).to_loop(target_loop_id);
        self.send(signal)
    }

    /// Send a resume signal to a specific loop
    pub fn send_resume(&self, target_loop_id: &str, reason: &str) -> Result<()> {
        let signal = SignalRecord::new(SignalType::Resume, reason).to_loop(target_loop_id);
        self.send(signal)
    }

    /// Send an invalidate signal to all descendants of a loop
    pub fn send_invalidate(&self, parent_loop_id: &str, reason: &str) -> Result<()> {
        let selector = format!("descendants:{}", parent_loop_id);
        let signal = SignalRecord::new(SignalType::Invalidate, reason)
            .from_loop(parent_loop_id)
            .to_selector(&selector);
        self.send(signal)
    }

    /// Check if a loop has any pending stop or invalidate signals
    pub fn has_stop_signal(&self, loop_id: &str) -> Result<bool> {
        if let Some(signal) = self.check(loop_id)? {
            Ok(signal.is_stop_signal())
        } else {
            Ok(false)
        }
    }

    /// Check if a loop has any pending pause signals
    pub fn has_pause_signal(&self, loop_id: &str) -> Result<bool> {
        if let Some(signal) = self.check(loop_id)? {
            Ok(signal.is_pause_signal())
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageWrapper;
    use tempfile::TempDir;

    fn create_test_storage() -> (TempDir, Arc<StorageWrapper>) {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(StorageWrapper::open(temp_dir.path()).unwrap());
        (temp_dir, storage)
    }

    #[test]
    fn test_signal_manager_new() {
        let (_temp, storage) = create_test_storage();
        let _manager = SignalManager::new(storage);
    }

    #[test]
    fn test_send_and_check_signal() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        let signal = SignalRecord::new(SignalType::Stop, "User requested stop").to_loop("loop-001");
        manager.send(signal).unwrap();

        let found = manager.check("loop-001").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.signal_type, SignalType::Stop);
        assert_eq!(found.target_loop, Some("loop-001".to_string()));
    }

    #[test]
    fn test_check_returns_none_for_unknown_loop() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        let found = manager.check("nonexistent-loop").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_acknowledge_signal() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        let signal = SignalRecord::new(SignalType::Pause, "Pause for review").to_loop("loop-002");
        let signal_id = signal.id.clone();
        manager.send(signal).unwrap();

        // Before acknowledgment
        let found = manager.check("loop-002").unwrap();
        assert!(found.is_some());

        // Acknowledge
        manager.acknowledge(&signal_id).unwrap();

        // After acknowledgment, check should return None
        let found = manager.check("loop-002").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_pending_signals() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        manager.send_stop("loop-001", "Stop 1").unwrap();
        manager.send_pause("loop-002", "Pause 2").unwrap();

        let pending = manager.pending().unwrap();
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn test_send_stop_helper() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        manager.send_stop("loop-003", "Test stop").unwrap();

        let found = manager.check("loop-003").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.signal_type, SignalType::Stop);
        assert_eq!(found.reason, "Test stop");
    }

    #[test]
    fn test_send_pause_helper() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        manager.send_pause("loop-004", "Test pause").unwrap();

        let found = manager.check("loop-004").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.signal_type, SignalType::Pause);
    }

    #[test]
    fn test_send_resume_helper() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        manager.send_resume("loop-005", "Test resume").unwrap();

        let found = manager.check("loop-005").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.signal_type, SignalType::Resume);
    }

    #[test]
    fn test_send_invalidate_helper() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        manager.send_invalidate("parent-001", "Parent re-iterated").unwrap();

        let signals = manager.check_selector("descendants:parent-001").unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].signal_type, SignalType::Invalidate);
        assert_eq!(signals[0].source_loop, Some("parent-001".to_string()));
    }

    #[test]
    fn test_check_selector() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        let signal = SignalRecord::new(SignalType::Invalidate, "Test").to_selector("descendants:root-001");
        manager.send(signal).unwrap();

        let signals = manager.check_selector("descendants:root-001").unwrap();
        assert_eq!(signals.len(), 1);

        let empty = manager.check_selector("descendants:other").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_has_stop_signal() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        assert!(!manager.has_stop_signal("loop-006").unwrap());

        manager.send_stop("loop-006", "Stop!").unwrap();
        assert!(manager.has_stop_signal("loop-006").unwrap());
    }

    #[test]
    fn test_has_pause_signal() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        assert!(!manager.has_pause_signal("loop-007").unwrap());

        manager.send_pause("loop-007", "Pause!").unwrap();
        assert!(manager.has_pause_signal("loop-007").unwrap());
    }

    #[test]
    fn test_acknowledge_nonexistent_signal() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        // Should not error on nonexistent signal
        manager.acknowledge("nonexistent-signal-id").unwrap();
    }

    #[test]
    fn test_multiple_signals_same_loop() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        manager.send_pause("loop-008", "First pause").unwrap();
        manager.send_stop("loop-008", "Then stop").unwrap();

        // Check returns the first unacknowledged signal
        let found = manager.check("loop-008").unwrap();
        assert!(found.is_some());

        let pending = manager.pending().unwrap();
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn test_invalidate_signal_is_stop_signal() {
        let (_temp, storage) = create_test_storage();
        let manager = SignalManager::new(storage);

        let signal = SignalRecord::new(SignalType::Invalidate, "Parent changed").to_loop("loop-009");
        manager.send(signal).unwrap();

        assert!(manager.has_stop_signal("loop-009").unwrap());
    }
}
