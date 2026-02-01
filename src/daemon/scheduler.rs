//! Scheduler - Selects loops to run based on priority
//!
//! Priority: Code > Phase > Spec > Plan (depth-first completion)

use crate::domain::{Loop, LoopType};

/// Scheduler selects loops to run given available slots
#[derive(Debug, Clone)]
pub struct Scheduler {
    /// Maximum concurrent loops (for reference, actual limit checked externally)
    pub max_concurrent: usize,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(4)
    }
}

impl Scheduler {
    /// Create a new scheduler with max concurrent limit
    pub fn new(max_concurrent: usize) -> Self {
        Self { max_concurrent }
    }

    /// Select loops to run given available slots
    /// Priority: Code > Phase > Spec > Plan (depth-first)
    pub fn select(&self, pending: Vec<Loop>, slots: usize) -> Vec<Loop> {
        if slots == 0 || pending.is_empty() {
            return Vec::new();
        }

        let mut sorted = pending;
        sorted.sort_by(|a, b| {
            // Lower priority number = higher priority
            let priority_a = Self::loop_priority(&a.loop_type);
            let priority_b = Self::loop_priority(&b.loop_type);
            priority_a.cmp(&priority_b)
        });

        sorted.into_iter().take(slots).collect()
    }

    /// Get priority for a loop type (lower = higher priority)
    /// Code loops complete first (depth-first)
    fn loop_priority(loop_type: &LoopType) -> u8 {
        match loop_type {
            LoopType::Code => 0,  // Highest priority
            LoopType::Phase => 1,
            LoopType::Spec => 2,
            LoopType::Plan => 3,  // Lowest priority
        }
    }

    /// Check if a loop can run given dependencies
    /// A loop can run if its parent (if any) is in a state that allows children
    pub fn can_run(&self, _loop_record: &Loop, parent: Option<&Loop>) -> bool {
        match parent {
            None => true, // No parent, can always run
            Some(p) => {
                // Parent must be complete or have spawned children
                p.status.is_terminal() || !p.output_artifacts.is_empty()
            }
        }
    }

    /// Select with dependency checking
    pub fn select_with_deps(
        &self,
        pending: Vec<Loop>,
        slots: usize,
        get_parent: impl Fn(&str) -> Option<Loop>,
    ) -> Vec<Loop> {
        if slots == 0 || pending.is_empty() {
            return Vec::new();
        }

        let mut runnable: Vec<Loop> = pending
            .into_iter()
            .filter(|l| {
                let parent = l.parent_id.as_ref().and_then(|pid| get_parent(pid));
                self.can_run(l, parent.as_ref())
            })
            .collect();

        runnable.sort_by(|a, b| {
            let priority_a = Self::loop_priority(&a.loop_type);
            let priority_b = Self::loop_priority(&b.loop_type);
            priority_a.cmp(&priority_b)
        });

        runnable.into_iter().take(slots).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::LoopStatus;

    fn make_loop(id: &str, loop_type: LoopType) -> Loop {
        Loop {
            id: id.to_string(),
            loop_type,
            parent_id: None,
            input_artifact: None,
            output_artifacts: Vec::new(),
            prompt_path: std::path::PathBuf::from("prompts/test.md"),
            validation_command: String::new(),
            max_iterations: 10,
            worktree: std::path::PathBuf::from(".loopr/worktrees/test"),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: serde_json::Value::Null,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_scheduler_new() {
        let s = Scheduler::new(8);
        assert_eq!(s.max_concurrent, 8);
    }

    #[test]
    fn test_scheduler_default() {
        let s = Scheduler::default();
        assert_eq!(s.max_concurrent, 4);
    }

    #[test]
    fn test_select_empty() {
        let s = Scheduler::new(4);
        let result = s.select(Vec::new(), 2);
        assert!(result.is_empty());
    }

    #[test]
    fn test_select_zero_slots() {
        let s = Scheduler::new(4);
        let pending = vec![make_loop("1", LoopType::Code)];
        let result = s.select(pending, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn test_select_single() {
        let s = Scheduler::new(4);
        let pending = vec![make_loop("1", LoopType::Plan)];
        let result = s.select(pending, 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "1");
    }

    #[test]
    fn test_select_priority_order() {
        let s = Scheduler::new(4);
        let pending = vec![
            make_loop("plan", LoopType::Plan),
            make_loop("code", LoopType::Code),
            make_loop("spec", LoopType::Spec),
            make_loop("phase", LoopType::Phase),
        ];
        let result = s.select(pending, 4);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].id, "code");  // Highest priority
        assert_eq!(result[1].id, "phase");
        assert_eq!(result[2].id, "spec");
        assert_eq!(result[3].id, "plan");  // Lowest priority
    }

    #[test]
    fn test_select_respects_slots() {
        let s = Scheduler::new(4);
        let pending = vec![
            make_loop("plan", LoopType::Plan),
            make_loop("code", LoopType::Code),
            make_loop("spec", LoopType::Spec),
            make_loop("phase", LoopType::Phase),
        ];
        let result = s.select(pending, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "code");
        assert_eq!(result[1].id, "phase");
    }

    #[test]
    fn test_loop_priority_code_highest() {
        assert_eq!(Scheduler::loop_priority(&LoopType::Code), 0);
    }

    #[test]
    fn test_loop_priority_plan_lowest() {
        assert_eq!(Scheduler::loop_priority(&LoopType::Plan), 3);
    }

    #[test]
    fn test_can_run_no_parent() {
        let s = Scheduler::new(4);
        let l = make_loop("1", LoopType::Code);
        assert!(s.can_run(&l, None));
    }

    #[test]
    fn test_can_run_parent_complete() {
        let s = Scheduler::new(4);
        let child = make_loop("child", LoopType::Code);
        let mut parent = make_loop("parent", LoopType::Phase);
        parent.status = LoopStatus::Complete;
        assert!(s.can_run(&child, Some(&parent)));
    }

    #[test]
    fn test_can_run_parent_with_artifacts() {
        let s = Scheduler::new(4);
        let child = make_loop("child", LoopType::Code);
        let mut parent = make_loop("parent", LoopType::Phase);
        parent.output_artifacts = vec![std::path::PathBuf::from("artifact.md")];
        assert!(s.can_run(&child, Some(&parent)));
    }

    #[test]
    fn test_can_run_parent_not_ready() {
        let s = Scheduler::new(4);
        let child = make_loop("child", LoopType::Code);
        let parent = make_loop("parent", LoopType::Phase);
        // Parent still pending, no artifacts
        assert!(!s.can_run(&child, Some(&parent)));
    }

    #[test]
    fn test_select_with_deps_filters_unrunnable() {
        let s = Scheduler::new(4);

        let mut child = make_loop("child", LoopType::Code);
        child.parent_id = Some("parent".to_string());

        let orphan = make_loop("orphan", LoopType::Plan);

        let pending = vec![child.clone(), orphan.clone()];

        // Parent not complete and no artifacts
        let parent_incomplete = Loop {
            id: "parent".to_string(),
            loop_type: LoopType::Phase,
            parent_id: None,
            input_artifact: None,
            output_artifacts: Vec::new(),
            prompt_path: std::path::PathBuf::from("prompts/test.md"),
            validation_command: String::new(),
            max_iterations: 10,
            worktree: std::path::PathBuf::from(".loopr/worktrees/test"),
            iteration: 0,
            status: LoopStatus::Running,
            progress: String::new(),
            context: serde_json::Value::Null,
            created_at: 0,
            updated_at: 0,
        };

        let result = s.select_with_deps(pending, 4, |_| Some(parent_incomplete.clone()));

        // Only orphan should be selected (child's parent not ready)
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "orphan");
    }

    #[test]
    fn test_select_with_deps_allows_runnable() {
        let s = Scheduler::new(4);

        let mut child = make_loop("child", LoopType::Code);
        child.parent_id = Some("parent".to_string());

        let pending = vec![child.clone()];

        // Parent complete
        let parent_complete = Loop {
            id: "parent".to_string(),
            loop_type: LoopType::Phase,
            parent_id: None,
            input_artifact: None,
            output_artifacts: Vec::new(),
            prompt_path: std::path::PathBuf::from("prompts/test.md"),
            validation_command: String::new(),
            max_iterations: 10,
            worktree: std::path::PathBuf::from(".loopr/worktrees/test"),
            iteration: 0,
            status: LoopStatus::Complete,
            progress: String::new(),
            context: serde_json::Value::Null,
            created_at: 0,
            updated_at: 0,
        };

        let result = s.select_with_deps(pending, 4, |_| Some(parent_complete.clone()));

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "child");
    }
}
