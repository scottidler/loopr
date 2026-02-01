//! Tick Loop - Daemon main loop processing
//!
//! The tick loop periodically:
//! - Processes IPC messages
//! - Reaps completed loops
//! - Schedules pending loops
//! - Manages concurrency limits

use std::time::Duration;

/// Configuration for the daemon tick loop
#[derive(Debug, Clone)]
pub struct TickConfig {
    /// Interval between ticks
    pub tick_interval: Duration,
    /// Maximum concurrent loops
    pub max_concurrent_loops: usize,
    /// Minimum free disk space in GB before pausing new loops
    pub disk_quota_min_gb: u64,
}

impl Default for TickConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_millis(100),
            max_concurrent_loops: 4,
            disk_quota_min_gb: 1,
        }
    }
}

impl TickConfig {
    /// Create a new tick config
    pub fn new(tick_interval: Duration, max_concurrent_loops: usize) -> Self {
        Self {
            tick_interval,
            max_concurrent_loops,
            disk_quota_min_gb: 1,
        }
    }

    /// Set the disk quota minimum
    pub fn with_disk_quota(mut self, min_gb: u64) -> Self {
        self.disk_quota_min_gb = min_gb;
        self
    }
}

/// Tick result indicates what happened during a tick
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickResult {
    /// Tick completed normally
    Ok,
    /// Tick started some loops
    StartedLoops(usize),
    /// Tick reaped some completed loops
    ReapedLoops(usize),
    /// Daemon should shut down
    Shutdown,
    /// Error occurred during tick
    Error(String),
}

/// Tick state tracks what's happening between ticks
#[derive(Debug, Default)]
pub struct TickState {
    /// Number of ticks since start
    pub tick_count: u64,
    /// Number of loops currently running
    pub running_loops: usize,
    /// Number of loops started this session
    pub total_started: u64,
    /// Number of loops completed this session
    pub total_completed: u64,
    /// Number of loops failed this session
    pub total_failed: u64,
    /// Whether shutdown has been requested
    pub shutdown_requested: bool,
}

impl TickState {
    /// Create a new tick state
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new tick
    pub fn tick(&mut self) {
        self.tick_count += 1;
    }

    /// Record loops started
    pub fn started(&mut self, count: usize) {
        self.running_loops += count;
        self.total_started += count as u64;
    }

    /// Record loops completed
    pub fn completed(&mut self, count: usize) {
        self.running_loops = self.running_loops.saturating_sub(count);
        self.total_completed += count as u64;
    }

    /// Record loops failed
    pub fn failed(&mut self, count: usize) {
        self.running_loops = self.running_loops.saturating_sub(count);
        self.total_failed += count as u64;
    }

    /// Request shutdown
    pub fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
    }

    /// Check if we can start more loops
    pub fn available_slots(&self, max_concurrent: usize) -> usize {
        max_concurrent.saturating_sub(self.running_loops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_config_default() {
        let config = TickConfig::default();
        assert_eq!(config.tick_interval, Duration::from_millis(100));
        assert_eq!(config.max_concurrent_loops, 4);
        assert_eq!(config.disk_quota_min_gb, 1);
    }

    #[test]
    fn test_tick_config_new() {
        let config = TickConfig::new(Duration::from_secs(1), 8);
        assert_eq!(config.tick_interval, Duration::from_secs(1));
        assert_eq!(config.max_concurrent_loops, 8);
    }

    #[test]
    fn test_tick_config_with_disk_quota() {
        let config = TickConfig::default().with_disk_quota(10);
        assert_eq!(config.disk_quota_min_gb, 10);
    }

    #[test]
    fn test_tick_state_new() {
        let state = TickState::new();
        assert_eq!(state.tick_count, 0);
        assert_eq!(state.running_loops, 0);
        assert!(!state.shutdown_requested);
    }

    #[test]
    fn test_tick_state_tick() {
        let mut state = TickState::new();
        state.tick();
        assert_eq!(state.tick_count, 1);
        state.tick();
        assert_eq!(state.tick_count, 2);
    }

    #[test]
    fn test_tick_state_started() {
        let mut state = TickState::new();
        state.started(3);
        assert_eq!(state.running_loops, 3);
        assert_eq!(state.total_started, 3);
    }

    #[test]
    fn test_tick_state_completed() {
        let mut state = TickState::new();
        state.started(5);
        state.completed(2);
        assert_eq!(state.running_loops, 3);
        assert_eq!(state.total_completed, 2);
    }

    #[test]
    fn test_tick_state_failed() {
        let mut state = TickState::new();
        state.started(5);
        state.failed(1);
        assert_eq!(state.running_loops, 4);
        assert_eq!(state.total_failed, 1);
    }

    #[test]
    fn test_tick_state_request_shutdown() {
        let mut state = TickState::new();
        assert!(!state.shutdown_requested);
        state.request_shutdown();
        assert!(state.shutdown_requested);
    }

    #[test]
    fn test_tick_state_available_slots() {
        let mut state = TickState::new();
        assert_eq!(state.available_slots(4), 4);
        state.started(2);
        assert_eq!(state.available_slots(4), 2);
        state.started(3);
        assert_eq!(state.available_slots(4), 0);
    }

    #[test]
    fn test_tick_state_saturating_sub() {
        let mut state = TickState::new();
        state.completed(5); // Should not go negative
        assert_eq!(state.running_loops, 0);
    }

    #[test]
    fn test_tick_result_variants() {
        assert_eq!(TickResult::Ok, TickResult::Ok);
        assert_eq!(TickResult::StartedLoops(3), TickResult::StartedLoops(3));
        assert_ne!(TickResult::StartedLoops(1), TickResult::StartedLoops(2));
        assert_eq!(TickResult::Shutdown, TickResult::Shutdown);
    }
}
