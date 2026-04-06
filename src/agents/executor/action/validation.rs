use eyre::Result;

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle ValidateDocument action.
pub(super) fn handle_validate_document(ctx: &AgentContext, collection: &str, id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    let resp = bridge.request(
        "validator.validate",
        serde_json::json!({ "collection": collection, "id": id }),
    );
    if resp.is_error() {
        let msg = resp
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "unknown validation error".to_string());
        return Ok(ActionResult::ActionError(format!(
            "validate_document {}/{} failed: {}",
            collection, id, msg
        )));
    }
    let verdict = resp
        .result
        .as_ref()
        .and_then(|v| v.get("verdict"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let summary = resp
        .result
        .as_ref()
        .and_then(|v| v.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let issues: Vec<String> = resp
        .result
        .as_ref()
        .and_then(|v| v.get("issues"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|issue| issue.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    ctx.info(&format!(
        "ValidateDocument {}/{}: verdict={}, issues={}",
        collection,
        id,
        verdict,
        issues.len()
    ));
    Ok(ActionResult::DocumentValidated {
        verdict,
        summary,
        issues,
    })
}

/// Handle EvaluateCoverage action.
pub(super) fn handle_evaluate_coverage(
    ctx: &AgentContext,
    parent_collection: &str,
    parent_id: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    let resp = bridge.request(
        "coverage.evaluate",
        serde_json::json!({ "parent_collection": parent_collection, "parent_id": parent_id }),
    );
    if resp.is_error() {
        let msg = resp
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "unknown coverage error".to_string());
        return Ok(ActionResult::ActionError(format!(
            "evaluate_coverage {}/{} failed: {}",
            parent_collection, parent_id, msg
        )));
    }
    let verdict = resp
        .result
        .as_ref()
        .and_then(|v| v.get("verdict"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let summary = resp
        .result
        .as_ref()
        .and_then(|v| v.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let gaps: Vec<String> = resp
        .result
        .as_ref()
        .and_then(|v| v.get("gaps"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|gap| gap.get("description").and_then(|d| d.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    ctx.info(&format!(
        "EvaluateCoverage {}/{}: verdict={}, gaps={}",
        parent_collection,
        parent_id,
        verdict,
        gaps.len()
    ));
    Ok(ActionResult::CoverageEvaluated { verdict, summary, gaps })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {

    use crate::agents::executor::tests::{create_test_hierarchy, test_agent_context, test_stores};
    use crate::agents::executor::{ActionResult, execute_action};
    use crate::agents::{AgentAction, AgentKind};

    use crate::test_util::TestDir;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_validate_document() {
        let dir = TestDir::new("loopr-exec-valdoc");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ValidateDocument {
            collection: "plan".to_string(),
            id: plan_id,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(
                result,
                ActionResult::DocumentValidated { .. } | ActionResult::ActionError(_)
            ),
            "expected DocumentValidated or ActionError, got: {:?}",
            result
        );
    }
}
