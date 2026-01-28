//! Loop execution module for Loopr.
//!
//! This module provides the core loop execution logic including:
//! - `RalphLoop`: The leaf-level loop that does actual coding work
//! - `PlanLoop`, `SpecLoop`, `PhaseLoop`: Higher-level loops that produce artifacts
//! - `Worktree`: Git worktree management for isolated execution
//! - `Validation`: Basic validation (run command, check exit code)
//! - `artifacts`: Parsing plan.md, spec.md, phase.md to extract child definitions
//!
//! ## Loop Hierarchy
//!
//! ```text
//! PlanLoop → produces plan.md → spawns SpecLoops
//!   SpecLoop → produces spec.md → spawns PhaseLoops
//!     PhaseLoop → produces phase.md → spawns RalphLoops
//!       RalphLoop → produces code (leaf node)
//! ```

#![allow(dead_code)]
#![allow(unused_imports)]

mod artifacts;
mod hierarchy;
mod ralph;
mod validation;
mod worktree;

// Artifact parsing
pub use artifacts::{
    PhaseDefinition, SpecDefinition, extract_phase_goal, parse_phases_from_spec, parse_specs_from_plan,
    validate_phase_format, validate_plan_format, validate_spec_format,
};

// Loop hierarchy types
pub use hierarchy::{
    HierarchyLoopConfig, IterationFeedback, PhaseLoop, PlanLoop, SpecLoop, invalidate_children, save_spawned_children,
    spawn_children_from_artifact,
};

// Ralph loop (leaf-level)
pub use ralph::{IterationResult, LoopAction, LoopError, LoopRunner, RalphLoop, RalphLoopConfig};

// Validation
pub use validation::{ValidationConfig, ValidationFeedback, ValidationResult, Validator};

// Worktree management
pub use worktree::{Worktree, WorktreeConfig, WorktreeError};
