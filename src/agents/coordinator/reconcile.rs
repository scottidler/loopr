use crate::agents::generation;
use crate::daemon::context::Stores;
use crate::domain::plan::{HierarchyStatus, Tier};
use crate::domain::work::WorkStatus;

/// Outcome of a single reconcile() call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub promoted: u32,
    pub completed: u32,
    pub goal_complete: bool,
}

/// Run the reactive reconciliation loop.
///
/// Three promotion passes (top-down) + two completion passes (bottom-up),
/// wrapped in a fixed-point loop so multi-level cascades resolve in one call.
/// Idempotent: running twice with no external state changes produces the same result.
pub fn reconcile(stores: &Stores) -> ReconcileOutcome {
    tracing::debug!("reconcile: entering");
    let mut total_promoted = 0u32;
    let mut total_completed = 0u32;

    // Fixed-point loop: repeat until no state changes.
    // Bounded by hierarchy depth (typically 3-4 iterations max).
    loop {
        let mut promoted = 0u32;
        let mut completed = 0u32;

        // --- Promotion passes (top-down) ---

        // Pass 1: Promote Specs (Pending -> Active when all spec-deps terminal)
        promoted += promote_specs(stores);

        // Pass 2: Promote Phases (Pending -> Active when parent Active + phase-deps terminal)
        promoted += promote_phases(stores);

        // Pass 3: Promote Works (Pending -> Ready when parent Active + work-deps Done)
        promoted += promote_works(stores);

        // --- Completion passes (bottom-up) ---

        // Pass 4: Phase completion (all child Works terminal -> Phase Complete)
        completed += complete_phases(stores);

        // Pass 5: Spec completion (all child Phases terminal -> Spec Complete)
        completed += complete_specs(stores);

        total_promoted += promoted;
        total_completed += completed;

        // Fixed-point: if nothing changed this iteration, we are converged.
        if promoted == 0 && completed == 0 {
            break;
        }
        tracing::debug!(
            "reconcile: loop iteration promoted={} completed={}",
            promoted,
            completed
        );
    }

    let goal_complete = detect_goal_complete(stores);

    tracing::debug!(
        "reconcile: done promoted={} completed={} goal_complete={}",
        total_promoted,
        total_completed,
        goal_complete,
    );

    ReconcileOutcome {
        promoted: total_promoted,
        completed: total_completed,
        goal_complete,
    }
}

// ---------------------------------------------------------------------------
// Promotion passes
// ---------------------------------------------------------------------------

/// Pass 1: Promote Pending Specs to Active when all deps are terminal.
fn promote_specs(stores: &Stores) -> u32 {
    let pending_specs: Vec<(String, Vec<String>)> = {
        let Ok(specs) = stores.read_specs() else {
            return 0;
        };
        specs
            .values()
            .filter(|s| s.status() == HierarchyStatus::Pending)
            .map(|s| (s.id.clone(), s.dependencies.clone()))
            .collect()
    };

    let mut promoted = 0u32;
    for (spec_id, deps) in pending_specs {
        if all_hierarchy_deps_terminal(stores, &deps, DependencyLevel::Spec) {
            let persisted = {
                let Ok(mut specs) = stores.write_specs() else {
                    continue;
                };
                if let Some(spec) = specs.get_mut(&spec_id)
                    && spec.status() == HierarchyStatus::Pending
                {
                    spec.force_status(HierarchyStatus::Active);
                    tracing::info!("reconcile: Spec {} promoted Pending -> Active", spec_id);
                    Some(spec.clone())
                } else {
                    None
                }
            };
            if let Some(spec) = persisted {
                persist_record(stores, spec);
                promoted += 1;
            }
        }
    }
    promoted
}

/// Pass 2: Promote Pending Phases to Active when parent is Active and all deps are terminal.
/// Also sets activated_at on promotion.
fn promote_phases(stores: &Stores) -> u32 {
    let pending_phases: Vec<(String, String, Vec<String>)> = {
        let Ok(phases) = stores.read_phases() else {
            return 0;
        };
        phases
            .values()
            .filter(|p| p.status() == HierarchyStatus::Pending)
            .map(|p| (p.id.clone(), p.parent_id.clone(), p.dependencies.clone()))
            .collect()
    };

    let mut promoted = 0u32;
    for (phase_id, parent_id, deps) in pending_phases {
        if parent_active(stores, &parent_id) && all_hierarchy_deps_terminal(stores, &deps, DependencyLevel::Phase) {
            let persisted = {
                let Ok(mut phases) = stores.write_phases() else {
                    continue;
                };
                if let Some(phase) = phases.get_mut(&phase_id)
                    && phase.status() == HierarchyStatus::Pending
                {
                    phase.force_status(HierarchyStatus::Active);
                    phase.activated_at = Some(crate::id::now_millis());
                    tracing::info!("reconcile: Phase {} promoted Pending -> Active", phase_id);
                    Some(phase.clone())
                } else {
                    None
                }
            };
            if let Some(phase) = persisted {
                persist_record(stores, phase);
                promoted += 1;
            }
        }
    }
    promoted
}

/// Pass 3: Promote Pending Works to Ready when parent is Active and all work-deps are Done.
/// Work deps use "Done" semantics (not terminal) - see design doc "Dependency Semantics".
fn promote_works(stores: &Stores) -> u32 {
    let pending_works: Vec<(String, String, Vec<String>)> = {
        let Ok(works) = stores.read_works() else {
            return 0;
        };
        works
            .values()
            .filter(|w| w.status() == WorkStatus::Pending)
            .map(|w| (w.id.clone(), w.parent_id.clone(), w.dependencies.clone()))
            .collect()
    };

    let mut promoted = 0u32;
    for (work_id, parent_id, deps) in pending_works {
        if parent_active(stores, &parent_id) && all_work_deps_done(stores, &deps) {
            let persisted = {
                let Ok(mut works) = stores.write_works() else {
                    continue;
                };
                if let Some(work) = works.get_mut(&work_id)
                    && work.status() == WorkStatus::Pending
                {
                    work.force_status(WorkStatus::Ready);
                    tracing::info!("reconcile: Work {} promoted Pending -> Ready", work_id);
                    Some(work.clone())
                } else {
                    None
                }
            };
            if let Some(work) = persisted {
                persist_record(stores, work);
                promoted += 1;
            }
        }
    }
    promoted
}

// ---------------------------------------------------------------------------
// Completion passes
// ---------------------------------------------------------------------------

/// Pass 4: Mark Active Phases as Complete when all child Works are terminal.
fn complete_phases(stores: &Stores) -> u32 {
    let active_phases: Vec<String> = {
        let Ok(phases) = stores.read_phases() else {
            return 0;
        };
        phases
            .values()
            .filter(|p| p.status() == HierarchyStatus::Active)
            .map(|p| p.id.clone())
            .collect()
    };

    let mut completed = 0u32;
    for phase_id in active_phases {
        if all_children_terminal_works(stores, &phase_id) {
            let persisted = {
                let Ok(mut phases) = stores.write_phases() else {
                    continue;
                };
                if let Some(phase) = phases.get_mut(&phase_id)
                    && phase.status() == HierarchyStatus::Active
                {
                    phase.force_status(HierarchyStatus::Complete);
                    phase.updated_at = crate::id::now_millis();
                    tracing::info!("reconcile: Phase {} completed (all Works terminal)", phase_id);
                    Some(phase.clone())
                } else {
                    None
                }
            };
            if let Some(phase) = persisted {
                persist_record(stores, phase);
                completed += 1;
            }
        }
    }
    completed
}

/// Pass 5: Mark Active Specs as Complete when all child Phases are terminal.
fn complete_specs(stores: &Stores) -> u32 {
    let active_specs: Vec<String> = {
        let Ok(specs) = stores.read_specs() else {
            return 0;
        };
        specs
            .values()
            .filter(|s| s.status() == HierarchyStatus::Active)
            .map(|s| s.id.clone())
            .collect()
    };

    let mut completed = 0u32;
    for spec_id in active_specs {
        if all_children_terminal_phases(stores, &spec_id) {
            let persisted = {
                let Ok(mut specs) = stores.write_specs() else {
                    continue;
                };
                if let Some(spec) = specs.get_mut(&spec_id)
                    && spec.status() == HierarchyStatus::Active
                {
                    spec.force_status(HierarchyStatus::Complete);
                    spec.updated_at = crate::id::now_millis();
                    tracing::info!("reconcile: Spec {} completed (all Phases terminal)", spec_id);
                    Some(spec.clone())
                } else {
                    None
                }
            };
            if let Some(spec) = persisted {
                persist_record(stores, spec);
                completed += 1;
            }
        }
    }
    completed
}

// ---------------------------------------------------------------------------
// Goal complete detection
// ---------------------------------------------------------------------------

/// Detect whether the active Plan's goal is complete.
/// Brief mode: all Works parented to Plan are terminal.
/// Full mode: all Specs are terminal.
fn detect_goal_complete(stores: &Stores) -> bool {
    let Some(plan) = generation::find_active_plan(stores) else {
        return false;
    };

    if plan.tier == Tier::Brief {
        // Brief: all Works parented to Plan are terminal
        let works = generation::find_works_for_parent(stores, &plan.id);
        if works.is_empty() {
            return false;
        }
        works
            .iter()
            .all(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
    } else {
        // Full: all Specs for this Plan are terminal
        let Ok(specs) = stores.read_specs() else {
            return false;
        };
        let plan_specs: Vec<_> = specs.values().filter(|s| s.parent_id == plan.id).collect();
        if plan_specs.is_empty() {
            return false;
        }
        plan_specs
            .iter()
            .all(|s| matches!(s.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Which store to check for dependency resolution.
#[derive(Debug, Clone, Copy)]
enum DependencyLevel {
    Spec,
    Phase,
}

/// Check if parent record is Active. Uses ID prefix to determine store.
/// - "sp-" -> Spec store (Phase's parent)
/// - "pl-" -> Plan store (Spec's parent, or Brief mode Work's parent)
/// - "ph-" -> Phase store (Work's parent in Full mode)
fn parent_active(stores: &Stores, parent_id: &str) -> bool {
    if parent_id.starts_with("sp-") {
        stores
            .read_specs()
            .ok()
            .and_then(|s| s.get(parent_id).map(|spec| spec.status() == HierarchyStatus::Active))
            .unwrap_or(false)
    } else if parent_id.starts_with("pl-") {
        stores
            .read_plans()
            .ok()
            .and_then(|p| p.get(parent_id).map(|plan| plan.status() == HierarchyStatus::Active))
            .unwrap_or(false)
    } else if parent_id.starts_with("ph-") {
        stores
            .read_phases()
            .ok()
            .and_then(|p| p.get(parent_id).map(|phase| phase.status() == HierarchyStatus::Active))
            .unwrap_or(false)
    } else {
        false
    }
}

/// Hierarchy dep check: all deps must be terminal (Complete or Abandoned).
/// An Abandoned dep does not block advancement - the quality gate at GoalComplete
/// handles abandon ratios.
fn all_hierarchy_deps_terminal(stores: &Stores, deps: &[String], level: DependencyLevel) -> bool {
    if deps.is_empty() {
        return true;
    }
    match level {
        DependencyLevel::Spec => {
            let Ok(specs) = stores.read_specs() else {
                return false;
            };
            deps.iter().all(|dep_id| {
                specs
                    .get(dep_id)
                    .map(|s| matches!(s.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
                    .unwrap_or(false)
            })
        }
        DependencyLevel::Phase => {
            let Ok(phases) = stores.read_phases() else {
                return false;
            };
            deps.iter().all(|dep_id| {
                phases
                    .get(dep_id)
                    .map(|p| matches!(p.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
                    .unwrap_or(false)
            })
        }
    }
}

/// Work dep check: all deps must be Done (not just terminal).
/// Abandoned work deps block downstream - if Work A writes main.py and is Abandoned,
/// Work B (which also writes main.py) must not execute against broken state.
fn all_work_deps_done(stores: &Stores, deps: &[String]) -> bool {
    if deps.is_empty() {
        return true;
    }
    let Ok(works) = stores.read_works() else {
        return false;
    };
    deps.iter().all(|dep_id| {
        works
            .get(dep_id)
            .map(|w| w.status() == WorkStatus::Done)
            .unwrap_or(false)
    })
}

/// Check if all child Works of a Phase are terminal (Done or Abandoned).
/// Returns true for a Phase with zero Works (vacuous truth - empty parent completes immediately).
fn all_children_terminal_works(stores: &Stores, phase_id: &str) -> bool {
    let Ok(works) = stores.read_works() else {
        return false;
    };
    let children: Vec<_> = works.values().filter(|w| w.parent_id == phase_id).collect();
    if children.is_empty() {
        return true;
    }
    children
        .iter()
        .all(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
}

/// Check if all child Phases of a Spec are terminal (Complete or Abandoned).
/// Returns true for a Spec with zero Phases (vacuous truth).
fn all_children_terminal_phases(stores: &Stores, spec_id: &str) -> bool {
    let Ok(phases) = stores.read_phases() else {
        return false;
    };
    let children: Vec<_> = phases.values().filter(|p| p.parent_id == spec_id).collect();
    if children.is_empty() {
        return true;
    }
    children
        .iter()
        .all(|p| matches!(p.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
}

/// Persist a record to TaskStore (JSONL). Follows TaskStore write ordering:
/// in-memory update has already been done under the lock; this persists to disk.
fn persist_record<R: taskstore::Record + serde::Serialize + Clone>(stores: &Stores, record: R) {
    if let Some(ref store) = stores.store
        && let Ok(mut s) = store.lock()
        && let Err(e) = s.update(record)
    {
        tracing::warn!("reconcile: failed to persist record: {}", e);
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
