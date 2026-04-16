use eyre::Result;

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle ProposePlan action.
pub(super) fn handle_propose_plan(
    ctx: &AgentContext,
    title: &str,
    description: &str,
    acceptance_criteria: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    // Create Plan as Draft via bridge
    let resp = bridge.request(
        "plan.create",
        serde_json::json!({
            "title": title,
            "description": description,
            "acceptance_criteria": acceptance_criteria,
        }),
    );
    if resp.is_error() {
        let msg = resp
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "plan creation failed".to_string());
        return Ok(ActionResult::ActionError(msg));
    }
    let plan_id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    ctx.info(&format!("ProposePlan: created draft plan {}", plan_id));
    Ok(ActionResult::RecordCreated {
        collection: "plans".to_string(),
        id: plan_id,
    })
}

/// Handle ReviseParent action.
pub(super) fn handle_revise_parent(
    ctx: &AgentContext,
    collection: &str,
    id: &str,
    reason: &str,
    diagnostic: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    // Transition parent back to Draft
    let transition_resp = bridge.request(
        "record.transition",
        serde_json::json!({
            "collection": collection,
            "id": id,
            "target_status": "draft",
            "role": "coordinator",
            "skip_validation": true,
            "skip_reason": "bubble-up revision: parent needs refinement"
        }),
    );
    if transition_resp.is_error() {
        let msg = transition_resp
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "unknown transition error".to_string());
        return Ok(ActionResult::ActionError(format!(
            "revise_parent {}/{} transition failed: {}",
            collection, id, msg
        )));
    }

    // Create a diagnostic Learning with scope derived from the collection
    let scope = match collection {
        "plans" | "plan" => "plan",
        "specs" | "spec" => "spec",
        "phases" | "phase" => "phase",
        _ => "plan",
    };
    let learning_content = format!(
        "Bubble-up revision for {}/{}: {}\nDiagnostic: {}",
        collection, id, reason, diagnostic
    );
    let _ = bridge.request(
        "learning.create",
        serde_json::json!({
            "content": learning_content,
            "scope": scope,
            "source_id": id,
        }),
    );

    ctx.info(&format!(
        "ReviseParent {}/{}: reason={}, diagnostic={}",
        collection, id, reason, diagnostic
    ));
    Ok(ActionResult::Transitioned(format!(
        "{}/{} -> draft (bubble-up: {})",
        collection, id, reason
    )))
}

/// Handle OverridePhase action.
/// Transitions a Phase to Superseded or Abandoned with an audit trail.
pub(super) fn handle_override_phase(
    ctx: &AgentContext,
    phase_id: &str,
    target_status: &str,
    reason: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    let resp = bridge.request(
        "phase.transition",
        serde_json::json!({
            "id": phase_id,
            "target_status": target_status,
            "role": "coordinator",
        }),
    );

    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!(
            "override_phase transition failed: {}",
            msg
        )));
    }

    let _ = bridge.request(
        "learning.create",
        serde_json::json!({
            "content": format!("Override: Phase {} -> {} (reason: {})", phase_id, target_status, reason),
            "scope": "phase",
            "source_id": phase_id,
            "applicable_roles": ["coordinator"],
            "files": ["override", "audit"],
        }),
    );

    ctx.info(&format!(
        "OverridePhase: {} -> {} (reason: {})",
        phase_id, target_status, reason
    ));
    Ok(ActionResult::Transitioned(format!(
        "override: phase/{} -> {}",
        phase_id, target_status
    )))
}

/// Handle OverrideSpec action.
/// Transitions a Spec to Superseded or Abandoned with an audit trail.
pub(super) fn handle_override_spec(
    ctx: &AgentContext,
    spec_id: &str,
    target_status: &str,
    reason: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    let resp = bridge.request(
        "spec.transition",
        serde_json::json!({
            "id": spec_id,
            "target_status": target_status,
            "role": "coordinator",
        }),
    );

    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!(
            "override_spec transition failed: {}",
            msg
        )));
    }

    let _ = bridge.request(
        "learning.create",
        serde_json::json!({
            "content": format!("Override: Spec {} -> {} (reason: {})", spec_id, target_status, reason),
            "scope": "spec",
            "source_id": spec_id,
            "applicable_roles": ["coordinator"],
            "files": ["override", "audit"],
        }),
    );

    ctx.info(&format!(
        "OverrideSpec: {} -> {} (reason: {})",
        spec_id, target_status, reason
    ));
    Ok(ActionResult::Transitioned(format!(
        "override: spec/{} -> {}",
        spec_id, target_status
    )))
}

/// Handle InterviewQuestion action.
pub(super) fn handle_interview_question(ctx: &AgentContext, questions: &[String]) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    let interview_mode = bridge.config().agents.coordinator.interview_mode;
    if interview_mode == crate::config::InterviewMode::Auto {
        // Auto mode: synthesize answer from goal + repo context
        // CoordinatorGoal has been removed - derive goal from the active plan title.
        let goal_text = {
            let plans = ctx.stores.read_plans()?;
            plans
                .values()
                .find(|p| {
                    p.status() == crate::domain::plan::PlanStatus::Active
                        || p.status() == crate::domain::plan::PlanStatus::Draft
                })
                .map(|p| p.title.clone())
                .unwrap_or_else(|| "No goal set.".to_string())
        };
        let repo_path = &bridge.config().project.repo_path;
        let readme =
            std::fs::read_to_string(repo_path.join("README.md")).unwrap_or_else(|_| "no README found".to_string());
        let file_tree = std::fs::read_dir(repo_path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|_| "could not list files".to_string());
        let synthetic_answer = format!(
            "Based on the goal: \"{}\"\nRepository context: {}\nFile tree: {}",
            goal_text, readme, file_tree
        );
        // Call interview_respond to record the exchange
        let resp = bridge.request(
            "coordinator.interview_respond",
            serde_json::json!({ "answer": synthetic_answer }),
        );
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "interview auto-respond failed".to_string());
            return Ok(ActionResult::ActionError(msg));
        }
        ctx.info(&format!(
            "InterviewQuestion: auto-answered {} question(s)",
            questions.len()
        ));
        Ok(ActionResult::Done(format!(
            "auto-answered {} interview question(s)",
            questions.len()
        )))
    } else {
        // Interactive mode: send questions to TUI via bridge event
        let resp = bridge.request(
            "coordinator.interview_question",
            serde_json::json!({ "questions": questions }),
        );
        if resp.is_error() {
            let msg = resp
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "interview question failed".to_string());
            return Ok(ActionResult::ActionError(msg));
        }
        ctx.info(&format!("InterviewQuestion: {} questions sent", questions.len()));
        Ok(ActionResult::Done(format!(
            "sent {} interview question(s)",
            questions.len()
        )))
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::agents::executor::tests::{create_test_hierarchy, test_agent_context, test_stores};
    use crate::agents::executor::{ActionResult, execute_action};
    use crate::agents::{AgentAction, AgentKind};
    use crate::test_util::TestDir;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_propose_plan() {
        let dir = TestDir::new("loopr-exec-proposeplan");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Director);

        let action = AgentAction::ProposePlan {
            title: "Draft Plan".to_string(),
            description: "A proposed plan".to_string(),
            acceptance_criteria: "All tests pass".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "plans");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_create_work() {
        let dir = TestDir::new("loopr-exec-creatework");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Director);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreateWork {
            parent_id: phase_id,
            title: "New Work".to_string(),
            description: "Work desc".to_string(),
            files: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "works");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }
}
