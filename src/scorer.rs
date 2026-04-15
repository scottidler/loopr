use std::path::Path;

use eyre::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agents::session::AgentSession;
use crate::agents::status::AgentStatus;
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::work::{Work, WorkStatus};

/// Structured score output from an E2E run. Written to `score.json` in the
/// E2E output directory so AutoResearch can evaluate each trial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub version: u32,
    pub duration_secs: u64,
    pub completion: CompletionMetrics,
    pub quality: QualityMetrics,
    pub efficiency: EfficiencyMetrics,
    pub validation: ValidationMetrics,
    pub composite_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionMetrics {
    pub works_total: u32,
    pub works_done: u32,
    pub works_abandoned: u32,
    pub works_blocked: u32,
    pub completion_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityMetrics {
    pub bundles_total: u32,
    pub bundles_accepted_first_try: u32,
    pub bundles_accepted_after_revision: u32,
    pub bundles_rejected_terminal: u32,
    pub first_try_acceptance_rate: f64,
    pub noop_bundles: u32,
    pub merge_conflicts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EfficiencyMetrics {
    pub total_sessions: u32,
    pub sessions_completed: u32,
    pub sessions_failed: u32,
    pub avg_attempts_per_work: f64,
    pub avg_rejections_per_bundle: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationMetrics {
    pub tests_passed: bool,
    pub validation_commands_passed: u32,
    pub validation_commands_total: u32,
}

/// Compute a score from the TaskStore at the given path.
///
/// Reads JSONL files for Works, Bundles, AgentSessions, and Ticks, then
/// computes the composite score using the weights defined in the design doc.
pub fn compute(store_path: &Path, duration_secs: u64) -> Result<Score> {
    let mut store = taskstore::Store::open(store_path).context("Failed to open TaskStore")?;

    // Rebuild indexes so list() queries work.
    store
        .rebuild_indexes::<Work>()
        .context("Failed to rebuild Work index")?;
    store
        .rebuild_indexes::<Bundle>()
        .context("Failed to rebuild Bundle index")?;
    store
        .rebuild_indexes::<AgentSession>()
        .context("Failed to rebuild AgentSession index")?;
    store
        .rebuild_indexes::<Tick>()
        .context("Failed to rebuild Tick index")?;

    let works = store.list::<Work>(&[]).unwrap_or_default();
    let bundles = store.list::<Bundle>(&[]).unwrap_or_default();
    let sessions = store.list::<AgentSession>(&[]).unwrap_or_default();
    let ticks = store.list::<Tick>(&[]).unwrap_or_default();

    let completion = compute_completion(&works);
    let quality = compute_quality(&works, &bundles);
    let efficiency = compute_efficiency(&works, &bundles, &sessions);
    let validation = compute_validation(&ticks);

    let composite_score = compute_composite(&completion, &quality, &efficiency, &validation);

    Ok(Score {
        version: 1,
        duration_secs,
        completion,
        quality,
        efficiency,
        validation,
        composite_score,
    })
}

fn compute_completion(works: &[Work]) -> CompletionMetrics {
    let works_total = works.len() as u32;
    let works_done = works.iter().filter(|w| w.status() == WorkStatus::Done).count() as u32;
    let works_abandoned = works.iter().filter(|w| w.status() == WorkStatus::Abandoned).count() as u32;
    let works_blocked = works.iter().filter(|w| w.status() == WorkStatus::Blocked).count() as u32;
    let completion_rate = if works_total > 0 { works_done as f64 / works_total as f64 } else { 0.0 };
    CompletionMetrics {
        works_total,
        works_done,
        works_abandoned,
        works_blocked,
        completion_rate,
    }
}

fn compute_quality(works: &[Work], bundles: &[Bundle]) -> QualityMetrics {
    // Exclude Superseded bundles from counting - they were replaced by newer attempts.
    let active_bundles: Vec<&Bundle> = bundles
        .iter()
        .filter(|b| b.status() != BundleStatus::Superseded)
        .collect();

    let bundles_total = active_bundles.len() as u32;
    let noop_bundles = active_bundles.iter().filter(|b| b.noop_reason.is_some()).count() as u32;

    // Superseded bundles are a proxy for integration merge conflicts.
    let merge_conflicts = bundles
        .iter()
        .filter(|b| b.status() == BundleStatus::Superseded)
        .count() as u32;

    // For first-try acceptance: a work's first (oldest) non-superseded bundle was Merged/Accepted
    // without any prior rejections.
    let mut first_try = 0u32;
    let mut after_revision = 0u32;
    let mut rejected_terminal = 0u32;

    for work in works {
        let work_bundles: Vec<&Bundle> = active_bundles
            .iter()
            .copied()
            .filter(|b| b.work_id == work.id)
            .collect();

        if work_bundles.is_empty() {
            continue;
        }

        let final_status = work.status();
        let has_accepted = work_bundles.iter().any(|b| {
            matches!(
                b.status(),
                BundleStatus::Accepted | BundleStatus::Integrating | BundleStatus::Merged
            )
        });
        let has_rejected = work_bundles.iter().any(|b| b.status() == BundleStatus::Rejected);

        if has_accepted && !has_rejected && work_bundles.len() == 1 {
            first_try += 1;
        } else if has_accepted && (has_rejected || work_bundles.len() > 1) {
            after_revision += 1;
        }

        if !has_accepted
            && matches!(
                final_status,
                WorkStatus::Done | WorkStatus::Superseded | WorkStatus::Abandoned
            )
        {
            rejected_terminal += 1;
        }
    }

    let done_count = works.iter().filter(|w| w.status() == WorkStatus::Done).count();
    let first_try_acceptance_rate = if done_count > 0 { first_try as f64 / done_count as f64 } else { 0.0 };

    QualityMetrics {
        bundles_total,
        bundles_accepted_first_try: first_try,
        bundles_accepted_after_revision: after_revision,
        bundles_rejected_terminal: rejected_terminal,
        first_try_acceptance_rate,
        noop_bundles,
        merge_conflicts,
    }
}

fn compute_efficiency(works: &[Work], bundles: &[Bundle], sessions: &[AgentSession]) -> EfficiencyMetrics {
    let total_sessions = sessions.len() as u32;
    let sessions_completed = sessions.iter().filter(|s| s.status() == AgentStatus::Completed).count() as u32;
    let sessions_failed = sessions.iter().filter(|s| s.status() == AgentStatus::Failed).count() as u32;

    let active_bundles: Vec<&Bundle> = bundles
        .iter()
        .filter(|b| b.status() != BundleStatus::Superseded)
        .collect();

    let works_with_bundles = works
        .iter()
        .filter(|w| active_bundles.iter().any(|b| b.work_id == w.id))
        .count();

    let avg_attempts_per_work = if works_with_bundles > 0 {
        active_bundles.len() as f64 / works_with_bundles as f64
    } else {
        0.0
    };

    let rejected_count = active_bundles
        .iter()
        .filter(|b| b.status() == BundleStatus::Rejected)
        .count();
    let avg_rejections_per_bundle = if active_bundles.is_empty() {
        0.0
    } else {
        rejected_count as f64 / active_bundles.len() as f64
    };

    EfficiencyMetrics {
        total_sessions,
        sessions_completed,
        sessions_failed,
        avg_attempts_per_work,
        avg_rejections_per_bundle,
    }
}

fn compute_validation(ticks: &[Tick]) -> ValidationMetrics {
    // Find the most recently updated Published tick to extract validation results.
    let published_tick = ticks
        .iter()
        .filter(|t| t.status() == TickStatus::Published)
        .max_by_key(|t| t.updated_at);

    if let Some(tick) = published_tick {
        let log = &tick.validation_log;
        let commands_run = log.matches("=== Running:").count() as u32;
        let commands_passed = log.matches("=== PASSED ===").count() as u32;
        let tests_passed = commands_run > 0 && commands_passed == commands_run;
        ValidationMetrics {
            tests_passed,
            validation_commands_passed: commands_passed,
            validation_commands_total: commands_run,
        }
    } else {
        // No Published tick: validation either didn't run or failed entirely.
        ValidationMetrics {
            tests_passed: false,
            validation_commands_passed: 0,
            validation_commands_total: 0,
        }
    }
}

fn compute_composite(
    completion: &CompletionMetrics,
    quality: &QualityMetrics,
    efficiency: &EfficiencyMetrics,
    validation: &ValidationMetrics,
) -> f64 {
    // Weights: 40% completion, 30% first-try acceptance, 20% validation, 10% efficiency
    let validation_component = if validation.validation_commands_total == 0 {
        0.0
    } else {
        validation.validation_commands_passed as f64 / validation.validation_commands_total as f64
    };

    let efficiency_component = if efficiency.total_sessions == 0 {
        1.0
    } else {
        1.0 - (efficiency.sessions_failed as f64 / efficiency.total_sessions as f64)
    };

    let score = 0.40 * completion.completion_rate
        + 0.30 * quality.first_try_acceptance_rate
        + 0.20 * validation_component
        + 0.10 * efficiency_component;

    // Clamp to [0.0, 1.0]
    score.clamp(0.0, 1.0)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn make_work(id: &str, status: WorkStatus) -> Work {
        let mut w = Work::new("phase-1".to_string(), "Test Work".to_string());
        w.id = id.to_string();
        w.force_status(status);
        w
    }

    fn make_bundle(id: &str, work_id: &str, status: BundleStatus, noop: bool) -> Bundle {
        let mut b = Bundle::new(
            work_id.to_string(),
            None,
            "branch".to_string(),
            vec!["claim".to_string()],
        );
        b.id = id.to_string();
        if noop {
            b.noop_reason = Some("already done".to_string());
        }
        b.force_status(status);
        b
    }

    fn make_session(id: &str, status: AgentStatus) -> AgentSession {
        use crate::agents::kind::AgentKind;
        let mut s = AgentSession::new(AgentKind::Implementer, "claude-sonnet-4-6".to_string());
        s.id = id.to_string();
        s.force_status(status);
        s
    }

    #[test]
    fn test_completion_all_done() {
        let works = vec![make_work("w1", WorkStatus::Done), make_work("w2", WorkStatus::Done)];
        let c = compute_completion(&works);
        assert_eq!(c.works_total, 2);
        assert_eq!(c.works_done, 2);
        assert!((c.completion_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_completion_mixed() {
        let works = vec![
            make_work("w1", WorkStatus::Done),
            make_work("w2", WorkStatus::Abandoned),
            make_work("w3", WorkStatus::Blocked),
            make_work("w4", WorkStatus::Done),
        ];
        let c = compute_completion(&works);
        assert_eq!(c.works_total, 4);
        assert_eq!(c.works_done, 2);
        assert_eq!(c.works_abandoned, 1);
        assert_eq!(c.works_blocked, 1);
        assert!((c.completion_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_completion_empty() {
        let c = compute_completion(&[]);
        assert_eq!(c.works_total, 0);
        assert!((c.completion_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_quality_first_try_acceptance() {
        let works = vec![make_work("w1", WorkStatus::Done)];
        let bundles = vec![make_bundle("b1", "w1", BundleStatus::Merged, false)];
        let q = compute_quality(&works, &bundles);
        assert_eq!(q.bundles_accepted_first_try, 1);
        assert_eq!(q.bundles_accepted_after_revision, 0);
        assert!((q.first_try_acceptance_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_quality_noop_bundles_counted() {
        let works = vec![make_work("w1", WorkStatus::Done)];
        let bundles = vec![make_bundle("b1", "w1", BundleStatus::Merged, true)];
        let q = compute_quality(&works, &bundles);
        assert_eq!(q.noop_bundles, 1);
    }

    #[test]
    fn test_quality_superseded_counted_as_merge_conflict() {
        let works: Vec<Work> = vec![];
        let bundles = vec![
            make_bundle("b1", "w1", BundleStatus::Superseded, false),
            make_bundle("b2", "w1", BundleStatus::Merged, false),
        ];
        let q = compute_quality(&works, &bundles);
        assert_eq!(q.merge_conflicts, 1);
    }

    #[test]
    fn test_efficiency_session_counts() {
        let works: Vec<Work> = vec![];
        let bundles: Vec<Bundle> = vec![];
        let sessions = vec![
            make_session("s1", AgentStatus::Completed),
            make_session("s2", AgentStatus::Completed),
            make_session("s3", AgentStatus::Failed),
        ];
        let e = compute_efficiency(&works, &bundles, &sessions);
        assert_eq!(e.total_sessions, 3);
        assert_eq!(e.sessions_completed, 2);
        assert_eq!(e.sessions_failed, 1);
    }

    #[test]
    fn test_validation_no_ticks() {
        let v = compute_validation(&[]);
        assert!(!v.tests_passed);
        assert_eq!(v.validation_commands_passed, 0);
        assert_eq!(v.validation_commands_total, 0);
    }

    #[test]
    fn test_validation_parses_log() {
        let mut tick = Tick::new(1);
        tick.validation_log = "=== Running: cargo test ===\ntest output\n=== PASSED ===\n\
=== Running: cargo clippy ===\nerror\n=== FAILED (exit code 1) ===\n"
            .to_string();
        tick.force_status(TickStatus::Published);

        let v = compute_validation(&[tick]);
        assert_eq!(v.validation_commands_total, 2);
        assert_eq!(v.validation_commands_passed, 1);
        assert!(!v.tests_passed);
    }

    #[test]
    fn test_validation_all_passed() {
        let mut tick = Tick::new(1);
        tick.validation_log =
            "=== Running: cargo test ===\nok\n=== PASSED ===\n=== Running: cargo fmt ===\n=== PASSED ===\n".to_string();
        tick.force_status(TickStatus::Published);

        let v = compute_validation(&[tick]);
        assert_eq!(v.validation_commands_total, 2);
        assert_eq!(v.validation_commands_passed, 2);
        assert!(v.tests_passed);
    }

    #[test]
    fn test_composite_score_zero_empty() {
        let completion = CompletionMetrics {
            works_total: 0,
            works_done: 0,
            works_abandoned: 0,
            works_blocked: 0,
            completion_rate: 0.0,
        };
        let quality = QualityMetrics {
            bundles_total: 0,
            bundles_accepted_first_try: 0,
            bundles_accepted_after_revision: 0,
            bundles_rejected_terminal: 0,
            first_try_acceptance_rate: 0.0,
            noop_bundles: 0,
            merge_conflicts: 0,
        };
        let efficiency = EfficiencyMetrics {
            total_sessions: 0,
            sessions_completed: 0,
            sessions_failed: 0,
            avg_attempts_per_work: 0.0,
            avg_rejections_per_bundle: 0.0,
        };
        let validation = ValidationMetrics {
            tests_passed: false,
            validation_commands_passed: 0,
            validation_commands_total: 0,
        };
        let score = compute_composite(&completion, &quality, &efficiency, &validation);
        // 0.40*0 + 0.30*0 + 0.20*0 + 0.10*1.0 (no sessions = perfect efficiency) = 0.1
        assert!((score - 0.1).abs() < 0.001, "score was {}", score);
    }

    #[test]
    fn test_composite_score_no_div_by_zero() {
        let completion = CompletionMetrics {
            works_total: 0,
            works_done: 0,
            works_abandoned: 0,
            works_blocked: 0,
            completion_rate: 0.0,
        };
        let quality = QualityMetrics {
            bundles_total: 0,
            bundles_accepted_first_try: 0,
            bundles_accepted_after_revision: 0,
            bundles_rejected_terminal: 0,
            first_try_acceptance_rate: 0.0,
            noop_bundles: 0,
            merge_conflicts: 0,
        };
        let efficiency = EfficiencyMetrics {
            total_sessions: 5,
            sessions_completed: 5,
            sessions_failed: 0,
            avg_attempts_per_work: 0.0,
            avg_rejections_per_bundle: 0.0,
        };
        let validation = ValidationMetrics {
            tests_passed: false,
            validation_commands_passed: 0,
            validation_commands_total: 0,
        };
        // Should not panic - div-by-zero guard in composite
        let score = compute_composite(&completion, &quality, &efficiency, &validation);
        assert!((0.0..=1.0).contains(&score));
    }
}
