use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::domain::plan::HierarchyStatus;
use crate::domain::validation::{ValidationReport, ValidationVerdict};
use crate::ipc::protocol::{DaemonEvent, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

/// Check the validation gate for Draft -> Active transitions.
/// Returns `Some(RpcError)` if the gate blocks the transition, `None` if allowed.
/// Gate only applies when:
/// 1. Validator is enabled (stores.validator is Some)
/// 2. Transition is Draft -> Active
/// 3. skip_validation param is not true
#[allow(clippy::too_many_arguments)]
pub(super) fn check_validation_gate(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    from: HierarchyStatus,
    target: HierarchyStatus,
    collection: &str,
    id: &str,
    skip_validation: bool,
    skip_reason: Option<&str>,
) -> Option<RpcError> {
    // Gate only applies to Draft -> Active
    if from != HierarchyStatus::Draft || target != HierarchyStatus::Active {
        return None;
    }

    // Gate only applies when validator is enabled
    stores.validator.as_ref()?;

    // Coordinator can skip validation with explicit flag
    if skip_validation {
        // Gap #8: Audit trail for skip-validation
        let reason = skip_reason.unwrap_or("no reason given");
        let _ = event_tx.send(DaemonEvent::new(
            "validation.skipped",
            json!({"collection": collection, "id": id, "reason": reason}),
        ));
        return None;
    }

    // Check for a passing ValidationReport in TaskStore
    if let Some(store) = &stores.store {
        let Ok(store) = store.lock() else {
            return Some(RpcError::internal("taskstore lock poisoned"));
        };
        let reports: Vec<ValidationReport> = store
            .list(&[Filter {
                field: "target_id".into(),
                op: FilterOp::Eq,
                value: IndexValue::String(id.to_string()),
            }])
            .unwrap_or_default();

        // Find the latest report (highest updated_at)
        let latest = reports.iter().max_by_key(|r| r.created_at);

        // Gap #23: Apply ValidatorStrictness
        let strictness = stores.config.strategy.validator_strictness;
        match latest {
            Some(report) => match report.verdict {
                ValidationVerdict::Fail => match strictness {
                    crate::config::ValidatorStrictness::SuggestOnly => None,
                    _ => Some(RpcError::validation_required(collection, id)),
                },
                ValidationVerdict::Warn => match strictness {
                    crate::config::ValidatorStrictness::HardFailOnAnyAmbiguity => {
                        Some(RpcError::validation_required(collection, id))
                    }
                    _ => None,
                },
                ValidationVerdict::Pass => None,
            },
            None => {
                // No report exists -> block
                Some(RpcError::validation_required(collection, id))
            }
        }
    } else {
        // No TaskStore -> no gate (shouldn't happen when validator is enabled, but be safe)
        None
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::config::ValidatorStrictness;
    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_integrator_config, test_stores_with_validator, test_stores_with_validator_strictness,
        test_worktree_mgr,
    };
    use crate::domain::plan::Plan;
    use crate::domain::validation::{ValidationReport, ValidationVerdict};
    use crate::ipc::protocol::DaemonRequest;
    use serde_json::json;

    #[test]
    fn test_non_draft_to_active_transition_no_gate() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator",
                    "skip_validation": true
                }),
            ),
        );

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "complete",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "complete");
    }

    #[test]
    fn test_latest_report_wins_for_validation_gate() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let fail_report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Fail,
            vec![],
            "Failed".to_string(),
            "test-model".to_string(),
        );
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(fail_report)
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));

        let pass_report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Pass,
            vec![],
            "Passed".to_string(),
            "test-model".to_string(),
        );
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(pass_report)
            .unwrap();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_validation_gate_hard_fail_on_warn() {
        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::HardFailOnAnyAmbiguity);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan = Plan::new("Gate Test".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Warn,
            vec![],
            "Ambiguous criteria".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            resp.is_error(),
            "HardFailOnAnyAmbiguity should block Draft->Active on Warn report"
        );
    }

    #[test]
    fn test_validation_gate_suggest_only_on_fail() {
        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::SuggestOnly);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan = Plan::new("Gate Test".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Fail,
            vec![],
            "Failed criteria".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            !resp.is_error(),
            "SuggestOnly should NOT block Draft->Active even on Fail report: {:?}",
            resp.error
        );
    }

    #[test]
    fn test_validation_gate_no_report_enabled() {
        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::HardFailOnAnyAmbiguity);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan = Plan::new("No Reports".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            resp.is_error(),
            "should block Draft->Active when no validation report exists"
        );
    }
}
