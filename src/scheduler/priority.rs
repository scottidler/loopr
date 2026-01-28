//! Priority calculation for loop scheduling.
//!
//! Loops are prioritized by:
//! - Base priority by type (ralph > phase > spec > plan)
//! - Age boost (+1 per minute waiting, capped at +50)
//! - Depth boost (+10 per level deep in hierarchy)
//! - Retry penalty (-5 per failed iteration, capped at -30)

use crate::store::{LoopRecord, LoopStatus, LoopType, TaskStore, now_ms};

/// Base priorities by loop type.
/// Higher = more important = runs first.
pub const PRIORITY_RALPH: i32 = 100;
pub const PRIORITY_PHASE: i32 = 80;
pub const PRIORITY_SPEC: i32 = 60;
pub const PRIORITY_PLAN: i32 = 40;

/// Age boost: +1 priority per minute waiting.
pub const AGE_BOOST_PER_MINUTE: i32 = 1;
/// Maximum age boost (50 minutes = max +50).
pub const AGE_BOOST_MAX: i32 = 50;

/// Depth boost: +10 priority per level in the hierarchy.
pub const DEPTH_BOOST_PER_LEVEL: i32 = 10;

/// Retry penalty: -5 priority per failed iteration.
pub const RETRY_PENALTY_PER_ITERATION: i32 = 5;
/// Maximum retry penalty.
pub const RETRY_PENALTY_MAX: i32 = 30;

/// Get base priority for a loop type.
pub fn base_priority(loop_type: LoopType) -> i32 {
    match loop_type {
        LoopType::Ralph => PRIORITY_RALPH,
        LoopType::Phase => PRIORITY_PHASE,
        LoopType::Spec => PRIORITY_SPEC,
        LoopType::Plan => PRIORITY_PLAN,
    }
}

/// Calculate how deep a loop is in the hierarchy by traversing parent_loop chain.
/// PlanLoop = 0, SpecLoop = 1, PhaseLoop = 2, RalphLoop = 3 (typically).
pub fn calculate_depth(loop_record: &LoopRecord, store: &TaskStore) -> i32 {
    let mut depth = 0;
    let mut current_parent = loop_record.parent_loop.clone();

    while let Some(parent_id) = current_parent {
        depth += 1;
        match store.get(&parent_id) {
            Ok(Some(parent)) => current_parent = parent.parent_loop,
            _ => break, // Parent not found, stop traversing
        }
    }

    depth
}

/// Calculate the priority score for a loop record.
///
/// Higher scores run first. Factors:
/// - Base priority by type (ralph=100, phase=80, spec=60, plan=40)
/// - Age boost: +1 per minute waiting, max +50
/// - Depth boost: +10 per level in hierarchy
/// - Retry penalty: -5 per failed iteration, max -30
pub fn calculate_priority(loop_record: &LoopRecord, store: &TaskStore) -> i32 {
    let mut priority = base_priority(loop_record.loop_type);

    // Age boost: +1 per minute waiting, max +50
    let age_ms = now_ms() - loop_record.created_at;
    let age_minutes = (age_ms / 60_000) as i32;
    let age_boost = age_minutes.min(AGE_BOOST_MAX);
    priority += age_boost;

    // Depth boost: +10 per level deep (encourages completing branches)
    let depth = calculate_depth(loop_record, store);
    priority += depth * DEPTH_BOOST_PER_LEVEL;

    // Retry penalty: -5 per failed iteration (deprioritize struggling loops)
    let retry_penalty = (loop_record.iteration as i32).saturating_sub(1) * RETRY_PENALTY_PER_ITERATION;
    priority -= retry_penalty.min(RETRY_PENALTY_MAX);

    priority
}

/// Check if a loop can be run based on dependencies.
///
/// A loop is runnable if:
/// 1. Status is `pending` or `paused`
/// 2. Parent loop (if any) has status `complete`
/// 3. Triggering artifact exists (if specified)
pub fn is_runnable(loop_record: &LoopRecord, store: &TaskStore) -> bool {
    // Must be in a startable status
    if !loop_record.status.can_start() {
        return false;
    }

    // Check parent dependency
    if let Some(ref parent_id) = loop_record.parent_loop {
        match store.get(parent_id) {
            Ok(Some(parent)) => {
                if parent.status != LoopStatus::Complete {
                    return false;
                }
            }
            _ => return false, // Parent doesn't exist or error
        }
    }

    // Check artifact exists (if triggered_by is specified)
    if let Some(ref triggered_by) = loop_record.triggered_by {
        let artifact_path = resolve_artifact_path(loop_record, triggered_by, store);
        if !artifact_path.exists() {
            return false;
        }
    }

    true
}

/// Resolve the artifact path for a loop.
///
/// The artifact path is relative to the loop's parent directory.
fn resolve_artifact_path(loop_record: &LoopRecord, triggered_by: &str, store: &TaskStore) -> std::path::PathBuf {
    // If parent exists, look in parent's loop directory
    if let Some(ref parent_id) = loop_record.parent_loop {
        store.loop_dir(parent_id).join(triggered_by)
    } else {
        // Top-level loop, artifact is relative to store base
        store.base_dir().join(triggered_by)
    }
}

/// Priority configuration (for customization).
#[derive(Debug, Clone)]
pub struct PriorityConfig {
    pub plan: i32,
    pub spec: i32,
    pub phase: i32,
    pub ralph: i32,
    pub age_boost_per_minute: i32,
    pub age_boost_max: i32,
    pub depth_boost_per_level: i32,
    pub retry_penalty_per_iteration: i32,
    pub retry_penalty_max: i32,
}

impl Default for PriorityConfig {
    fn default() -> Self {
        Self {
            plan: PRIORITY_PLAN,
            spec: PRIORITY_SPEC,
            phase: PRIORITY_PHASE,
            ralph: PRIORITY_RALPH,
            age_boost_per_minute: AGE_BOOST_PER_MINUTE,
            age_boost_max: AGE_BOOST_MAX,
            depth_boost_per_level: DEPTH_BOOST_PER_LEVEL,
            retry_penalty_per_iteration: RETRY_PENALTY_PER_ITERATION,
            retry_penalty_max: RETRY_PENALTY_MAX,
        }
    }
}

impl PriorityConfig {
    /// Get base priority for a loop type with this config.
    pub fn base_priority(&self, loop_type: LoopType) -> i32 {
        match loop_type {
            LoopType::Ralph => self.ralph,
            LoopType::Phase => self.phase,
            LoopType::Spec => self.spec,
            LoopType::Plan => self.plan,
        }
    }

    /// Calculate priority with custom config.
    pub fn calculate_priority(&self, loop_record: &LoopRecord, store: &TaskStore) -> i32 {
        let mut priority = self.base_priority(loop_record.loop_type);

        // Age boost
        let age_ms = now_ms() - loop_record.created_at;
        let age_minutes = (age_ms / 60_000) as i32;
        let age_boost = (age_minutes * self.age_boost_per_minute).min(self.age_boost_max);
        priority += age_boost;

        // Depth boost
        let depth = calculate_depth(loop_record, store);
        priority += depth * self.depth_boost_per_level;

        // Retry penalty
        let retry_penalty = (loop_record.iteration as i32).saturating_sub(1) * self.retry_penalty_per_iteration;
        priority -= retry_penalty.min(self.retry_penalty_max);

        priority
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
    fn test_base_priority() {
        assert_eq!(base_priority(LoopType::Ralph), 100);
        assert_eq!(base_priority(LoopType::Phase), 80);
        assert_eq!(base_priority(LoopType::Spec), 60);
        assert_eq!(base_priority(LoopType::Plan), 40);
    }

    #[test]
    fn test_calculate_depth_no_parent() {
        let (store, _temp) = create_temp_store();
        let record = LoopRecord::new_plan("Test", 10);
        assert_eq!(calculate_depth(&record, &store), 0);
    }

    #[test]
    fn test_calculate_depth_with_parents() {
        let (mut store, _temp) = create_temp_store();

        // Create parent chain: plan -> spec -> phase -> ralph
        let mut plan = LoopRecord::new_plan("Plan", 10);
        plan.status = LoopStatus::Complete;
        store.save(&plan).unwrap();

        let mut spec = LoopRecord::new_spec(&plan.id, "Content", 10);
        spec.status = LoopStatus::Complete;
        store.save(&spec).unwrap();

        let mut phase = LoopRecord::new_phase(&spec.id, "Spec", 1, "Phase 1", 3, 10);
        phase.status = LoopStatus::Complete;
        store.save(&phase).unwrap();

        let ralph = LoopRecord::new_ralph_from_phase(&phase.id, "Phase", "Task", 5);
        store.save(&ralph).unwrap();

        assert_eq!(calculate_depth(&plan, &store), 0);
        assert_eq!(calculate_depth(&spec, &store), 1);
        assert_eq!(calculate_depth(&phase, &store), 2);
        assert_eq!(calculate_depth(&ralph, &store), 3);
    }

    #[test]
    fn test_calculate_priority_basic() {
        let (store, _temp) = create_temp_store();

        let record = LoopRecord::new_ralph("Test", 5);
        let priority = calculate_priority(&record, &store);

        // Base 100 + age ~0 (just created) + depth 0 = ~100
        // Allow small variance due to time elapsed during test
        assert!((100..=110).contains(&priority), "priority={}", priority);
    }

    #[test]
    fn test_calculate_priority_with_depth() {
        let (mut store, _temp) = create_temp_store();

        let mut plan = LoopRecord::new_plan("Plan", 10);
        plan.status = LoopStatus::Complete;
        store.save(&plan).unwrap();

        let spec = LoopRecord::new_spec(&plan.id, "Content", 10);
        let priority = calculate_priority(&spec, &store);

        // Base 60 + age ~0 + depth 10 (1 level) = ~70
        // Allow small variance due to time elapsed during test
        assert!((70..=80).contains(&priority), "priority={}", priority);
    }

    #[test]
    fn test_calculate_priority_with_retries() {
        let (store, _temp) = create_temp_store();

        let mut record = LoopRecord::new_ralph("Test", 5);
        record.iteration = 5; // 4 retries
        let priority = calculate_priority(&record, &store);

        // Base 100 + age 0 + depth 0 - retry 20 = 80
        assert_eq!(priority, 80);
    }

    #[test]
    fn test_retry_penalty_capped() {
        let (store, _temp) = create_temp_store();

        let mut record = LoopRecord::new_ralph("Test", 10);
        record.iteration = 20; // Many retries
        let priority = calculate_priority(&record, &store);

        // Base 100 + age 0 + depth 0 - retry 30 (capped) = 70
        assert_eq!(priority, 70);
    }

    #[test]
    fn test_is_runnable_pending() {
        let (store, _temp) = create_temp_store();

        let record = LoopRecord::new_ralph("Test", 5);
        assert!(is_runnable(&record, &store));
    }

    #[test]
    fn test_is_runnable_running() {
        let (store, _temp) = create_temp_store();

        let mut record = LoopRecord::new_ralph("Test", 5);
        record.status = LoopStatus::Running;
        assert!(!is_runnable(&record, &store));
    }

    #[test]
    fn test_is_runnable_complete() {
        let (store, _temp) = create_temp_store();

        let mut record = LoopRecord::new_ralph("Test", 5);
        record.status = LoopStatus::Complete;
        assert!(!is_runnable(&record, &store));
    }

    #[test]
    fn test_is_runnable_paused() {
        let (store, _temp) = create_temp_store();

        let mut record = LoopRecord::new_ralph("Test", 5);
        record.status = LoopStatus::Paused;
        assert!(is_runnable(&record, &store));
    }

    #[test]
    fn test_is_runnable_parent_not_complete() {
        let (mut store, _temp) = create_temp_store();

        let mut parent = LoopRecord::new_plan("Parent", 10);
        parent.status = LoopStatus::Running;
        store.save(&parent).unwrap();

        let child = LoopRecord::new_spec(&parent.id, "Content", 10);
        assert!(!is_runnable(&child, &store));
    }

    #[test]
    fn test_is_runnable_parent_complete() {
        let (mut store, _temp) = create_temp_store();

        let mut parent = LoopRecord::new_plan("Parent", 10);
        parent.status = LoopStatus::Complete;
        store.save(&parent).unwrap();

        let child = LoopRecord::new_spec(&parent.id, "Content", 10);
        assert!(is_runnable(&child, &store));
    }

    #[test]
    fn test_is_runnable_parent_missing() {
        let (store, _temp) = create_temp_store();

        let mut child = LoopRecord::new_spec("nonexistent", "Content", 10);
        child.status = LoopStatus::Pending;
        assert!(!is_runnable(&child, &store));
    }

    #[test]
    fn test_priority_config_default() {
        let config = PriorityConfig::default();
        assert_eq!(config.plan, 40);
        assert_eq!(config.ralph, 100);
        assert_eq!(config.age_boost_max, 50);
    }

    #[test]
    fn test_priority_config_custom() {
        let (store, _temp) = create_temp_store();

        let config = PriorityConfig {
            plan: 50,
            spec: 70,
            phase: 90,
            ralph: 110,
            age_boost_per_minute: 2,
            age_boost_max: 100,
            depth_boost_per_level: 20,
            retry_penalty_per_iteration: 10,
            retry_penalty_max: 50,
        };

        let record = LoopRecord::new_ralph("Test", 5);
        let priority = config.calculate_priority(&record, &store);

        // Base 110 + age ~0 + depth 0 = ~110
        // Allow small variance due to time elapsed during test
        assert!((110..=130).contains(&priority), "priority={}", priority);
    }

    #[test]
    fn test_ralph_higher_than_plan() {
        let (store, _temp) = create_temp_store();

        let ralph = LoopRecord::new_ralph("Ralph", 5);
        let plan = LoopRecord::new_plan("Plan", 10);

        let ralph_priority = calculate_priority(&ralph, &store);
        let plan_priority = calculate_priority(&plan, &store);

        assert!(ralph_priority > plan_priority);
    }

    #[test]
    fn test_depth_increases_priority() {
        let (mut store, _temp) = create_temp_store();

        let mut plan = LoopRecord::new_plan("Plan", 10);
        plan.status = LoopStatus::Complete;
        store.save(&plan).unwrap();

        let mut spec = LoopRecord::new_spec(&plan.id, "Content", 10);
        spec.status = LoopStatus::Complete;
        store.save(&spec).unwrap();

        // Two specs at different depths
        let shallow_spec = LoopRecord::new_spec(&plan.id, "Shallow", 10);
        let deep_phase = LoopRecord::new_phase(&spec.id, "Spec", 1, "Phase", 3, 10);

        let shallow_priority = calculate_priority(&shallow_spec, &store);
        let deep_priority = calculate_priority(&deep_phase, &store);

        // Deep phase (depth 2, base 80) vs shallow spec (depth 1, base 60)
        // Phase: 80 + 20 = 100, Spec: 60 + 10 = 70
        assert!(deep_priority > shallow_priority);
    }
}
