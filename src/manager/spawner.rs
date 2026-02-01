//! Child spawner module
//!
//! Handles decisions about spawning child loops based on parent loop outcomes.

use crate::artifact::plan::parse_plan_specs;
use crate::artifact::spec::parse_spec_phases;
use crate::domain::loop_record::{Loop, LoopType};

/// Decision about what children to spawn from a completed loop
#[derive(Debug, Clone)]
pub enum SpawnDecision {
    /// No children to spawn
    None,
    /// Spawn spec loops from plan
    Specs(Vec<SpawnSpec>),
    /// Spawn phase loops from spec
    Phases(Vec<SpawnPhase>),
    /// Spawn a single code loop from phase
    Code,
}

/// Descriptor for spawning a spec loop
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Index of this spec (1-based)
    pub index: u32,
    /// Name/title of the spec
    pub name: String,
    /// Description or goal
    pub description: String,
}

/// Descriptor for spawning a phase loop
#[derive(Debug, Clone)]
pub struct SpawnPhase {
    /// Index of this phase (1-based)
    pub index: u32,
    /// Name/title of the phase
    pub name: String,
    /// Number of implementation steps
    pub step_count: usize,
}

/// Determines what child loops to spawn from a completed parent loop
pub struct ChildSpawner;

impl ChildSpawner {
    /// Determine what children to spawn based on completed loop and its artifact
    pub fn decide(loop_instance: &Loop, artifact: Option<&str>) -> SpawnDecision {
        match loop_instance.loop_type {
            LoopType::Plan => Self::decide_from_plan(artifact),
            LoopType::Spec => Self::decide_from_spec(artifact),
            LoopType::Phase => SpawnDecision::Code,
            LoopType::Code => SpawnDecision::None,
        }
    }

    fn decide_from_plan(artifact: Option<&str>) -> SpawnDecision {
        let Some(content) = artifact else {
            return SpawnDecision::None;
        };

        let specs = parse_plan_specs(content);
        if specs.is_empty() {
            return SpawnDecision::None;
        }

        let spawn_specs = specs
            .into_iter()
            .map(|s| SpawnSpec {
                index: s.index,
                name: s.name,
                description: s.goal,
            })
            .collect();

        SpawnDecision::Specs(spawn_specs)
    }

    fn decide_from_spec(artifact: Option<&str>) -> SpawnDecision {
        let Some(content) = artifact else {
            return SpawnDecision::None;
        };

        let phases = parse_spec_phases(content);
        if phases.is_empty() {
            return SpawnDecision::None;
        }

        let spawn_phases = phases
            .into_iter()
            .map(|p| SpawnPhase {
                index: p.index,
                name: p.name,
                step_count: p.steps.len(),
            })
            .collect();

        SpawnDecision::Phases(spawn_phases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_decision_none_for_code() {
        let parent = Loop::new_plan("task");
        let code_loop = Loop::new_code(&parent);
        let decision = ChildSpawner::decide(&code_loop, None);
        assert!(matches!(decision, SpawnDecision::None));
    }

    #[test]
    fn test_spawn_decision_code_for_phase() {
        let parent = Loop::new_plan("task");
        let spec = Loop::new_spec(&parent, 1);
        let phase = Loop::new_phase(&spec, 1, "Phase 1", 1);
        let decision = ChildSpawner::decide(&phase, None);
        assert!(matches!(decision, SpawnDecision::Code));
    }

    #[test]
    fn test_spawn_decision_none_without_artifact() {
        let plan = Loop::new_plan("task");
        let decision = ChildSpawner::decide(&plan, None);
        assert!(matches!(decision, SpawnDecision::None));
    }

    #[test]
    fn test_spawn_decision_specs_from_plan() {
        let plan = Loop::new_plan("task");
        let artifact = r#"
# Plan

## Specs

### Spec 1: Authentication
Goal: Implement authentication

### Spec 2: Dashboard
Goal: Build dashboard
"#;
        let decision = ChildSpawner::decide(&plan, Some(artifact));
        match decision {
            SpawnDecision::Specs(specs) => {
                assert_eq!(specs.len(), 2);
                assert_eq!(specs[0].index, 1);
                assert_eq!(specs[0].name, "Authentication");
                assert_eq!(specs[1].index, 2);
                assert_eq!(specs[1].name, "Dashboard");
            }
            _ => panic!("Expected Specs decision"),
        }
    }

    #[test]
    fn test_spawn_decision_phases_from_spec() {
        let parent = Loop::new_plan("task");
        let spec = Loop::new_spec(&parent, 1);
        let artifact = r#"
# Spec

## Phases

### Phase 1: Setup
Steps:
- Step 1
- Step 2

### Phase 2: Implementation
Steps:
- Step 1
"#;
        let decision = ChildSpawner::decide(&spec, Some(artifact));
        match decision {
            SpawnDecision::Phases(phases) => {
                assert_eq!(phases.len(), 2);
                assert_eq!(phases[0].index, 1);
                assert_eq!(phases[0].name, "Setup");
                assert_eq!(phases[1].index, 2);
                assert_eq!(phases[1].name, "Implementation");
            }
            _ => panic!("Expected Phases decision"),
        }
    }

    #[test]
    fn test_spawn_spec_fields() {
        let spec = SpawnSpec {
            index: 1,
            name: "Auth".to_string(),
            description: "Authentication".to_string(),
        };
        assert_eq!(spec.index, 1);
        assert_eq!(spec.name, "Auth");
        assert_eq!(spec.description, "Authentication");
    }

    #[test]
    fn test_spawn_phase_fields() {
        let phase = SpawnPhase {
            index: 2,
            name: "Build".to_string(),
            step_count: 5,
        };
        assert_eq!(phase.index, 2);
        assert_eq!(phase.name, "Build");
        assert_eq!(phase.step_count, 5);
    }

    #[test]
    fn test_spawn_decision_empty_plan() {
        let plan = Loop::new_plan("task");
        let artifact = "# Plan\n\nNo specs here.";
        let decision = ChildSpawner::decide(&plan, Some(artifact));
        assert!(matches!(decision, SpawnDecision::None));
    }

    #[test]
    fn test_spawn_decision_empty_spec() {
        let parent = Loop::new_plan("task");
        let spec = Loop::new_spec(&parent, 1);
        let artifact = "# Spec\n\nNo phases here.";
        let decision = ChildSpawner::decide(&spec, Some(artifact));
        assert!(matches!(decision, SpawnDecision::None));
    }

    #[test]
    fn test_spawn_decision_debug() {
        let decision = SpawnDecision::None;
        let debug_str = format!("{:?}", decision);
        assert!(debug_str.contains("None"));
    }

    #[test]
    fn test_spawn_decision_clone() {
        let specs = SpawnDecision::Specs(vec![SpawnSpec {
            index: 1,
            name: "Test".to_string(),
            description: "Desc".to_string(),
        }]);
        let cloned = specs.clone();
        match cloned {
            SpawnDecision::Specs(s) => assert_eq!(s.len(), 1),
            _ => panic!("Clone failed"),
        }
    }
}
