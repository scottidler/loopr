use std::sync::Arc;

use tracing::{debug, warn};

use crate::agents::AgentKind;
use crate::daemon::context::Stores;
use crate::domain::lock::LockStatus;
use crate::domain::work::WorkStatus;

/// Priority score for a Ready Work item. Higher = picked first.
#[derive(Debug, Clone, PartialEq)]
struct WorkPriority {
    pub work_id: String,
    pub score: i64,
}

/// Determine the next Work to assign from the Ready pool and atomically claim it.
/// Returns None if no assignable Work exists or if the candidate was claimed by
/// another worker between the scoring and claiming phases.
///
/// Two-phase approach:
/// - Phase 1 (read lock): score all Ready candidates
/// - Phase 2 (write lock): re-verify the winner is still Ready, mark InProgress
///
/// Filters:
/// - Work must be in Ready state
/// - Work must be in the current Phase (if specified)
/// - All Work dependencies must be Done
/// - No active (non-terminal) Implementer session on the Work
///
/// Priority (higher score = picked first):
/// - +100 if no active locks contend with the Work's files
/// - +(10 - min(deps, 10)) * 10 for fewer dependencies
pub fn next_assignable_work(stores: &Arc<Stores>, current_phase_id: Option<&str>) -> Option<String> {
    // Phase 1: score candidates under read lock
    let candidate_id = {
        let works = stores.read_works().ok()?;
        let locks = stores.read_locks().ok()?;
        let sessions = stores.read_agent_sessions().ok()?;

        let mut candidates: Vec<WorkPriority> = works
            .values()
            .filter(|w| w.status() == WorkStatus::Ready)
            .filter(|w| current_phase_id.map(|pid| w.parent_id == pid).unwrap_or(true))
            // Exclude Works whose dependencies aren't Done
            .filter(|w| {
                w.dependencies.iter().all(|dep_id| {
                    works
                        .get(dep_id)
                        .map(|dep| dep.status() == WorkStatus::Done)
                        .unwrap_or(false)
                })
            })
            // Exclude Works that already have a non-terminal Implementer
            .filter(|w| {
                !sessions.values().any(|s| {
                    s.agent_type == AgentKind::Implementer
                        && s.work_id.as_deref() == Some(&w.id)
                        && !s.status().is_terminal()
                })
            })
            .map(|w| {
                let score = compute_priority(w, &locks);
                WorkPriority {
                    work_id: w.id.clone(),
                    score,
                }
            })
            .collect();

        // Sort by priority (highest first)
        candidates.sort_by(|a, b| b.score.cmp(&a.score));

        candidates.first().map(|c| c.work_id.clone())
    }?;

    // Phase 2: atomically claim under write lock
    let mut works = stores.write_works().ok()?;
    let work = works.get_mut(&candidate_id)?;
    if work.status() != WorkStatus::Ready {
        debug!(
            "next_assignable_work: candidate {} no longer Ready (status={:?}), lost race",
            candidate_id,
            work.status()
        );
        return None;
    }
    work.force_status(WorkStatus::InProgress);
    work.assignee = Some("implementer".to_string());
    // Persist to TaskStore while still holding the write lock
    if let Some(ref store_arc) = stores.store
        && let Ok(mut s) = store_arc.lock()
        && let Err(e) = s.update(work.clone())
    {
        warn!(
            "next_assignable_work: failed to persist claim for {}: {}",
            candidate_id, e
        );
    }
    Some(candidate_id)
}

/// Compute priority score for a Work item.
/// Higher score = higher priority.
fn compute_priority(
    work: &crate::domain::work::Work,
    locks: &std::sync::RwLockReadGuard<'_, std::collections::HashMap<String, crate::domain::lock::Lock>>,
) -> i64 {
    let mut score: i64 = 0;

    // Prefer Works with no resource contention (no active locks on their files)
    let has_contention = work.files.iter().any(|tag| {
        locks
            .values()
            .any(|l| l.resource == *tag && l.status() == LockStatus::Active)
    });
    if !has_contention {
        score += 100;
    }

    // Prefer Works with fewer/no dependencies (can start immediately)
    score += (10 - work.dependencies.len().min(10) as i64) * 10;

    // Penalize Works that have been attempted and reset multiple times.
    // Each failed attempt reduces priority, preventing a single broken
    // Work item from starving the rest of the pool.
    score -= (work.attempt_count.min(5) as i64) * 50;

    score
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentSession, AgentStatus};
    use crate::domain::lock::Lock;
    use crate::domain::work::Work;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn ready_work(stores: &Stores, phase_id: &str, title: &str) -> String {
        let mut w = Work::new(phase_id.to_string(), title.to_string());
        w.force_status(WorkStatus::Ready);
        let id = w.id.clone();
        stores.works.write().unwrap().insert(id.clone(), w);
        id
    }

    #[test]
    fn test_no_ready_works_returns_none() {
        let stores = test_stores();
        assert!(next_assignable_work(&stores, None).is_none());
    }

    #[test]
    fn test_single_ready_work_returned() {
        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Work A");
        assert_eq!(next_assignable_work(&stores, None), Some(id));
    }

    #[test]
    fn test_phase_filter_includes_matching() {
        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Work A");
        assert_eq!(next_assignable_work(&stores, Some("phase-1")), Some(id));
    }

    #[test]
    fn test_phase_filter_excludes_non_matching() {
        let stores = test_stores();
        ready_work(&stores, "phase-1", "Work A");
        assert!(next_assignable_work(&stores, Some("phase-2")).is_none());
    }

    #[test]
    fn test_dependency_filter_excludes_unmet() {
        let stores = test_stores();
        // Create dependency work in Draft (not Done)
        let mut dep = Work::new("phase-1".to_string(), "Dep".to_string());
        dep.force_status(WorkStatus::Draft);
        let dep_id = dep.id.clone();
        stores.works.write().unwrap().insert(dep_id.clone(), dep);

        // Create work with unmet dependency
        let mut w = Work::new("phase-1".to_string(), "Work".to_string());
        w.force_status(WorkStatus::Ready);
        w.dependencies = vec![dep_id];
        let id = w.id.clone();
        stores.works.write().unwrap().insert(id.clone(), w);

        assert!(next_assignable_work(&stores, None).is_none());
    }

    #[test]
    fn test_dependency_filter_includes_met() {
        let stores = test_stores();
        // Create dependency work in Done
        let mut dep = Work::new("phase-1".to_string(), "Dep".to_string());
        dep.force_status(WorkStatus::Done);
        let dep_id = dep.id.clone();
        stores.works.write().unwrap().insert(dep_id.clone(), dep);

        // Create work with met dependency
        let mut w = Work::new("phase-1".to_string(), "Work".to_string());
        w.force_status(WorkStatus::Ready);
        w.dependencies = vec![dep_id];
        let id = w.id.clone();
        stores.works.write().unwrap().insert(id.clone(), w);

        assert_eq!(next_assignable_work(&stores, None), Some(id));
    }

    #[test]
    fn test_dedup_filter_excludes_active_implementer() {
        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Work A");

        // Create an active implementer session for this work
        let mut session = AgentSession::new(AgentKind::Implementer, "model".to_string());
        session.work_id = Some(id.clone());
        session.force_status(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(next_assignable_work(&stores, None).is_none());
    }

    #[test]
    fn test_dedup_filter_allows_terminal_implementer() {
        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Work A");

        // Create a completed implementer session for this work
        let mut session = AgentSession::new(AgentKind::Implementer, "model".to_string());
        session.work_id = Some(id.clone());
        session.force_status(AgentStatus::Completed);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert_eq!(next_assignable_work(&stores, None), Some(id));
    }

    #[test]
    fn test_priority_no_contention_over_contention() {
        let stores = test_stores();
        // Work A: has contention (active lock on its resource_tag)
        let mut wa = Work::new("phase-1".to_string(), "Work A".to_string());
        wa.force_status(WorkStatus::Ready);
        wa.files = vec!["src/main.rs".to_string()];
        let wa_id = wa.id.clone();
        stores.works.write().unwrap().insert(wa_id.clone(), wa);

        // Create active lock on src/main.rs
        let lock = Lock::new("src/main.rs".to_string(), "wi-other".to_string(), "coord".to_string());
        stores.locks.write().unwrap().insert(lock.id.clone(), lock);

        // Work B: no contention
        let mut wb = Work::new("phase-1".to_string(), "Work B".to_string());
        wb.force_status(WorkStatus::Ready);
        wb.files = vec!["src/lib.rs".to_string()];
        let wb_id = wb.id.clone();
        stores.works.write().unwrap().insert(wb_id.clone(), wb);

        // Work B should be picked (no contention = +100)
        assert_eq!(next_assignable_work(&stores, None), Some(wb_id));
    }

    #[test]
    fn test_priority_fewer_deps_over_more_deps() {
        let stores = test_stores();

        // Create two Done dependencies
        let mut dep1 = Work::new("phase-1".to_string(), "Dep1".to_string());
        dep1.force_status(WorkStatus::Done);
        let dep1_id = dep1.id.clone();
        stores.works.write().unwrap().insert(dep1_id.clone(), dep1);

        let mut dep2 = Work::new("phase-1".to_string(), "Dep2".to_string());
        dep2.force_status(WorkStatus::Done);
        let dep2_id = dep2.id.clone();
        stores.works.write().unwrap().insert(dep2_id.clone(), dep2);

        // Work A: 2 deps (score: 100 + 80 = 180)
        let mut wa = Work::new("phase-1".to_string(), "Work A".to_string());
        wa.force_status(WorkStatus::Ready);
        wa.dependencies = vec![dep1_id.clone(), dep2_id.clone()];
        let wa_id = wa.id.clone();
        stores.works.write().unwrap().insert(wa_id.clone(), wa);

        // Work B: 0 deps (score: 100 + 100 = 200)
        let mut wb = Work::new("phase-1".to_string(), "Work B".to_string());
        wb.force_status(WorkStatus::Ready);
        let wb_id = wb.id.clone();
        stores.works.write().unwrap().insert(wb_id.clone(), wb);

        // Work B should be picked (fewer deps)
        assert_eq!(next_assignable_work(&stores, None), Some(wb_id));
    }

    #[test]
    fn test_non_ready_works_excluded() {
        let stores = test_stores();
        // Draft work
        let w = Work::new("phase-1".to_string(), "Draft".to_string());
        stores.works.write().unwrap().insert(w.id.clone(), w);

        // InProgress work
        let mut w2 = Work::new("phase-1".to_string(), "InProgress".to_string());
        w2.force_status(WorkStatus::InProgress);
        stores.works.write().unwrap().insert(w2.id.clone(), w2);

        // Done work
        let mut w3 = Work::new("phase-1".to_string(), "Done".to_string());
        w3.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w3.id.clone(), w3);

        assert!(next_assignable_work(&stores, None).is_none());
    }

    #[test]
    fn test_multiple_ready_works_returns_highest_priority() {
        let stores = test_stores();

        // Work A: has resource tags that are locked
        let mut wa = Work::new("phase-1".to_string(), "Locked".to_string());
        wa.force_status(WorkStatus::Ready);
        wa.files = vec!["contested.rs".to_string()];
        let wa_id = wa.id.clone();
        stores.works.write().unwrap().insert(wa_id.clone(), wa);

        let lock = Lock::new("contested.rs".to_string(), "other".to_string(), "coord".to_string());
        stores.locks.write().unwrap().insert(lock.id.clone(), lock);

        // Work B: no contention, no deps
        let mut wb = Work::new("phase-1".to_string(), "Free".to_string());
        wb.force_status(WorkStatus::Ready);
        let wb_id = wb.id.clone();
        stores.works.write().unwrap().insert(wb_id.clone(), wb);

        assert_eq!(next_assignable_work(&stores, None), Some(wb_id));
    }

    #[test]
    fn test_atomic_claim_marks_inprogress() {
        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Work A");

        // Claim the work
        let result = next_assignable_work(&stores, None);
        assert_eq!(result, Some(id.clone()));

        // Verify it was marked InProgress
        let works = stores.works.read().unwrap();
        assert_eq!(works[&id].status(), WorkStatus::InProgress);
    }

    #[test]
    fn test_atomic_claim_sets_assignee() {
        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Work A");

        next_assignable_work(&stores, None);

        let works = stores.works.read().unwrap();
        assert_eq!(works[&id].assignee, Some("implementer".to_string()));
    }

    #[test]
    fn test_atomic_claim_prevents_double_assignment() {
        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Work A");

        // First call claims it
        let first = next_assignable_work(&stores, None);
        assert_eq!(first, Some(id));

        // Second call finds nothing (work is now InProgress, not Ready)
        let second = next_assignable_work(&stores, None);
        assert!(second.is_none());
    }

    #[test]
    fn test_concurrent_claim_only_one_wins() {
        use std::sync::Barrier;
        use std::thread;

        let stores = test_stores();
        let id = ready_work(&stores, "phase-1", "Contested Work");
        let _ = id; // we just need one Ready work

        let barrier = Arc::new(Barrier::new(2));
        let stores1 = stores.clone();
        let stores2 = stores.clone();
        let b1 = barrier.clone();
        let b2 = barrier.clone();

        let t1 = thread::spawn(move || {
            b1.wait();
            next_assignable_work(&stores1, None)
        });
        let t2 = thread::spawn(move || {
            b2.wait();
            next_assignable_work(&stores2, None)
        });

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        // Exactly one thread should have claimed the work
        let winners: Vec<_> = [r1, r2].iter().filter(|r| r.is_some()).cloned().collect();
        assert_eq!(winners.len(), 1, "exactly one thread should claim the work");
    }

    #[test]
    fn test_priority_attempt_count_penalty() {
        let stores = test_stores();
        let locks = stores.locks.read().unwrap();

        // Work with no attempts (fresh)
        let w_fresh = Work::new("phase-1".to_string(), "Fresh".to_string());
        let fresh_score = compute_priority(&w_fresh, &locks);

        // Work with 3 failed attempts
        let mut w_cycled = Work::new("phase-1".to_string(), "Cycled".to_string());
        w_cycled.attempt_count = 3;
        let cycled_score = compute_priority(&w_cycled, &locks);

        assert!(
            cycled_score < fresh_score,
            "cycled work (score={}) should have lower priority than fresh work (score={})",
            cycled_score,
            fresh_score
        );
        assert_eq!(fresh_score - cycled_score, 150, "3 attempts * 50 = 150 penalty");
    }

    #[test]
    fn test_priority_attempt_penalty_caps_at_5() {
        let stores = test_stores();
        let locks = stores.locks.read().unwrap();

        let mut w_5 = Work::new("phase-1".to_string(), "5 attempts".to_string());
        w_5.attempt_count = 5;
        let score_5 = compute_priority(&w_5, &locks);

        let mut w_10 = Work::new("phase-1".to_string(), "10 attempts".to_string());
        w_10.attempt_count = 10;
        let score_10 = compute_priority(&w_10, &locks);

        assert_eq!(
            score_5, score_10,
            "penalty should cap at 5 attempts (min(count, 5) * 50)"
        );
    }

    #[test]
    fn test_fresh_work_beats_cycled_in_queue() {
        let stores = test_stores();

        // Cycled work: attempt_count = 2
        let mut wa = Work::new("phase-1".to_string(), "Cycled".to_string());
        wa.force_status(WorkStatus::Ready);
        wa.attempt_count = 2;
        let wa_id = wa.id.clone();
        stores.works.write().unwrap().insert(wa_id.clone(), wa);

        // Fresh work: attempt_count = 0
        let mut wb = Work::new("phase-1".to_string(), "Fresh".to_string());
        wb.force_status(WorkStatus::Ready);
        let wb_id = wb.id.clone();
        stores.works.write().unwrap().insert(wb_id.clone(), wb);

        // Fresh work should be picked (higher priority due to no attempt penalty)
        assert_eq!(
            next_assignable_work(&stores, None),
            Some(wb_id),
            "fresh work should be preferred over cycled work"
        );
    }
}
