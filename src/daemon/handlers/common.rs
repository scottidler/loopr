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
