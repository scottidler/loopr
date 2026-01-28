//! Scheduler for selecting which loops to run.
//!
//! The Scheduler determines which pending loops should be started based on:
//! - Priority (calculated from type, age, depth, retries)
//! - Dependencies (parent must be complete)
//! - Concurrency limits (global and per-type)
//! - Rate limit state (back off when API is throttling)

use std::collections::HashMap;

use crate::scheduler::priority::{PriorityConfig, is_runnable};
use crate::scheduler::rate_limit::RateLimitState;
use crate::store::{LoopRecord, LoopStatus, LoopType, TaskStore};

/// Configuration for concurrency limits.
#[derive(Debug, Clone)]
pub struct ConcurrencyConfig {
    /// Maximum total concurrent loops.
    pub max_loops: usize,
    /// Maximum concurrent API calls (rate limit protection).
    pub max_api_calls: usize,
    /// Per-type limits (optional).
    pub per_type: HashMap<LoopType, usize>,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_loops: 50,
            max_api_calls: 10,
            per_type: HashMap::new(),
        }
    }
}

impl ConcurrencyConfig {
    /// Create a new concurrency config with the given limits.
    pub fn new(max_loops: usize, max_api_calls: usize) -> Self {
        Self {
            max_loops,
            max_api_calls,
            per_type: HashMap::new(),
        }
    }

    /// Set a per-type limit.
    pub fn with_type_limit(mut self, loop_type: LoopType, limit: usize) -> Self {
        self.per_type.insert(loop_type, limit);
        self
    }
}

/// Scheduler for selecting runnable loops.
pub struct Scheduler {
    /// Concurrency configuration.
    concurrency: ConcurrencyConfig,
    /// Priority configuration.
    priority_config: PriorityConfig,
}

impl Scheduler {
    /// Create a new Scheduler with default configuration.
    pub fn new() -> Self {
        Self {
            concurrency: ConcurrencyConfig::default(),
            priority_config: PriorityConfig::default(),
        }
    }

    /// Create a Scheduler with custom concurrency config.
    pub fn with_concurrency(mut self, config: ConcurrencyConfig) -> Self {
        self.concurrency = config;
        self
    }

    /// Create a Scheduler with custom priority config.
    pub fn with_priority(mut self, config: PriorityConfig) -> Self {
        self.priority_config = config;
        self
    }

    /// Get the maximum concurrent loops.
    pub fn max_concurrent(&self) -> usize {
        self.concurrency.max_loops
    }

    /// Select loops to run given current capacity.
    ///
    /// Returns loops sorted by priority, respecting:
    /// - Available slots (max_concurrent - currently_running)
    /// - Rate limit state (returns empty if rate limited)
    /// - Per-type limits
    /// - Dependency requirements (parent must be complete)
    pub fn select_runnable(
        &self,
        store: &TaskStore,
        currently_running: usize,
        rate_limit: Option<&RateLimitState>,
    ) -> Vec<LoopRecord> {
        // Don't start new loops if rate limited
        if let Some(rl) = rate_limit
            && rl.is_rate_limited()
        {
            tracing::debug!(
                until = ?rl.backoff_until,
                "Rate limited, not selecting new loops"
            );
            return vec![];
        }

        let available_slots = self.concurrency.max_loops.saturating_sub(currently_running);
        if available_slots == 0 {
            return vec![];
        }

        // Query all pending/paused loops
        let pending: Vec<LoopRecord> = store.list_runnable().unwrap_or_default();

        // Filter to runnable (dependencies satisfied)
        let mut runnable: Vec<LoopRecord> = pending.into_iter().filter(|r| is_runnable(r, store)).collect();

        // Sort by priority (descending - higher priority first)
        runnable.sort_by(|a, b| {
            let pa = self.priority_config.calculate_priority(a, store);
            let pb = self.priority_config.calculate_priority(b, store);
            pb.cmp(&pa) // Higher priority first
        });

        // Apply per-type limits if configured
        if !self.concurrency.per_type.is_empty() {
            runnable = self.apply_type_limits(runnable, store);
        }

        // Take up to available slots
        runnable.truncate(available_slots);

        runnable
    }

    /// Select loops that can run without API calls during rate limiting.
    ///
    /// This allows validation phases to continue even when API is rate limited.
    pub fn select_during_rate_limit(&self, store: &TaskStore) -> Vec<LoopRecord> {
        // Select loops that are in "validating" state - they run tests, not API calls
        // For now, return loops that are running but might be in validation
        store
            .list_by_status(LoopStatus::Running)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                // Check if loop is in validation phase (context field)
                r.context
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .map(|p| p == "validating")
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Apply per-type limits to the runnable list.
    fn apply_type_limits(&self, runnable: Vec<LoopRecord>, store: &TaskStore) -> Vec<LoopRecord> {
        let mut counts: HashMap<LoopType, usize> = HashMap::new();

        // Count currently running by type
        if let Ok(running) = store.list_by_status(LoopStatus::Running) {
            for record in running {
                *counts.entry(record.loop_type).or_insert(0) += 1;
            }
        }

        let mut result = Vec::new();
        for record in runnable {
            let current = counts.get(&record.loop_type).copied().unwrap_or(0);
            let limit = self
                .concurrency
                .per_type
                .get(&record.loop_type)
                .copied()
                .unwrap_or(usize::MAX);

            if current < limit {
                *counts.entry(record.loop_type).or_insert(0) += 1;
                result.push(record);
            }
        }

        result
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_temp_store() -> (TaskStore, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let store = TaskStore::open_at(temp_dir.path()).unwrap();
        (store, temp_dir)
    }

    #[test]
    fn test_scheduler_new() {
        let scheduler = Scheduler::new();
        assert_eq!(scheduler.max_concurrent(), 50);
    }

    #[test]
    fn test_scheduler_with_concurrency() {
        let config = ConcurrencyConfig::new(100, 20);
        let scheduler = Scheduler::new().with_concurrency(config);
        assert_eq!(scheduler.max_concurrent(), 100);
    }

    #[test]
    fn test_select_runnable_empty() {
        let (store, _temp) = create_temp_store();
        let scheduler = Scheduler::new();

        let selected = scheduler.select_runnable(&store, 0, None);
        assert!(selected.is_empty());
    }

    #[test]
    fn test_select_runnable_single() {
        let (mut store, _temp) = create_temp_store();
        let scheduler = Scheduler::new();

        let record = LoopRecord::new_ralph("Test", 5);
        store.save(&record).unwrap();

        let selected = scheduler.select_runnable(&store, 0, None);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, record.id);
    }

    #[test]
    fn test_select_runnable_respects_capacity() {
        let (mut store, _temp) = create_temp_store();
        let config = ConcurrencyConfig::new(2, 10);
        let scheduler = Scheduler::new().with_concurrency(config);

        // Create 5 pending loops
        for i in 0..5 {
            let record = LoopRecord::new_ralph(&format!("Test {}", i), 5);
            store.save(&record).unwrap();
        }

        // With 1 running, only 1 slot available
        let selected = scheduler.select_runnable(&store, 1, None);
        assert_eq!(selected.len(), 1);

        // With 0 running, 2 slots available
        let selected = scheduler.select_runnable(&store, 0, None);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn test_select_runnable_no_slots() {
        let (mut store, _temp) = create_temp_store();
        let config = ConcurrencyConfig::new(2, 10);
        let scheduler = Scheduler::new().with_concurrency(config);

        let record = LoopRecord::new_ralph("Test", 5);
        store.save(&record).unwrap();

        // All slots used
        let selected = scheduler.select_runnable(&store, 2, None);
        assert!(selected.is_empty());
    }

    #[test]
    fn test_select_runnable_by_priority() {
        let (mut store, _temp) = create_temp_store();
        let scheduler = Scheduler::new();

        // Create loops of different types
        let plan = LoopRecord::new_plan("Plan", 10);
        let ralph = LoopRecord::new_ralph("Ralph", 5);

        store.save(&plan).unwrap();
        store.save(&ralph).unwrap();

        let selected = scheduler.select_runnable(&store, 0, None);
        assert_eq!(selected.len(), 2);

        // Ralph should be first (higher priority)
        assert_eq!(selected[0].loop_type, LoopType::Ralph);
        assert_eq!(selected[1].loop_type, LoopType::Plan);
    }

    #[test]
    fn test_select_runnable_filters_running() {
        let (mut store, _temp) = create_temp_store();
        let scheduler = Scheduler::new();

        let mut running = LoopRecord::new_ralph("Running", 5);
        running.status = LoopStatus::Running;
        store.save(&running).unwrap();

        let pending = LoopRecord::new_ralph("Pending", 5);
        store.save(&pending).unwrap();

        let selected = scheduler.select_runnable(&store, 0, None);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, pending.id);
    }

    #[test]
    fn test_select_runnable_respects_parent() {
        let (mut store, _temp) = create_temp_store();
        let scheduler = Scheduler::new();

        // Parent is not complete
        let mut parent = LoopRecord::new_plan("Parent", 10);
        parent.status = LoopStatus::Running;
        store.save(&parent).unwrap();

        let child = LoopRecord::new_spec(&parent.id, "Content", 10);
        store.save(&child).unwrap();

        let selected = scheduler.select_runnable(&store, 0, None);
        assert!(selected.is_empty());

        // Complete the parent
        parent.status = LoopStatus::Complete;
        store.update(&parent).unwrap();

        let selected = scheduler.select_runnable(&store, 0, None);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, child.id);
    }

    #[test]
    fn test_select_runnable_with_rate_limit() {
        use std::time::Duration;

        let (mut store, _temp) = create_temp_store();
        let scheduler = Scheduler::new();

        let record = LoopRecord::new_ralph("Test", 5);
        store.save(&record).unwrap();

        // Not rate limited
        let rate_limit = RateLimitState::new();
        let selected = scheduler.select_runnable(&store, 0, Some(&rate_limit));
        assert_eq!(selected.len(), 1);

        // Rate limited
        let mut rate_limit = RateLimitState::new();
        rate_limit.record_rate_limit(Duration::from_secs(60));
        let selected = scheduler.select_runnable(&store, 0, Some(&rate_limit));
        assert!(selected.is_empty());
    }

    #[test]
    fn test_per_type_limits() {
        let (mut store, _temp) = create_temp_store();

        let config = ConcurrencyConfig::new(100, 10).with_type_limit(LoopType::Ralph, 2);

        let scheduler = Scheduler::new().with_concurrency(config);

        // Create 5 ralph loops
        for i in 0..5 {
            let record = LoopRecord::new_ralph(&format!("Ralph {}", i), 5);
            store.save(&record).unwrap();
        }

        let selected = scheduler.select_runnable(&store, 0, None);
        // Only 2 ralphs due to type limit
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn test_per_type_limits_with_running() {
        let (mut store, _temp) = create_temp_store();

        let config = ConcurrencyConfig::new(100, 10).with_type_limit(LoopType::Ralph, 2);

        let scheduler = Scheduler::new().with_concurrency(config);

        // 1 ralph already running
        let mut running = LoopRecord::new_ralph("Running", 5);
        running.status = LoopStatus::Running;
        store.save(&running).unwrap();

        // Create 5 pending ralph loops
        for i in 0..5 {
            let record = LoopRecord::new_ralph(&format!("Ralph {}", i), 5);
            store.save(&record).unwrap();
        }

        let selected = scheduler.select_runnable(&store, 1, None);
        // Only 1 more ralph due to type limit (2 total, 1 running)
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn test_concurrency_config_default() {
        let config = ConcurrencyConfig::default();
        assert_eq!(config.max_loops, 50);
        assert_eq!(config.max_api_calls, 10);
        assert!(config.per_type.is_empty());
    }

    #[test]
    fn test_concurrency_config_builder() {
        let config = ConcurrencyConfig::new(100, 20)
            .with_type_limit(LoopType::Plan, 2)
            .with_type_limit(LoopType::Ralph, 50);

        assert_eq!(config.max_loops, 100);
        assert_eq!(config.max_api_calls, 20);
        assert_eq!(config.per_type.get(&LoopType::Plan), Some(&2));
        assert_eq!(config.per_type.get(&LoopType::Ralph), Some(&50));
    }
}
