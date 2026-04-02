use eyre::Result;

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle CreatePlan action.
pub(super) fn handle_create_plan(
    ctx: &AgentContext,
    title: &str,
    description: &str,
    acceptance_criteria: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    // Gap #27: Draft-awareness guard
    let list_resp = bridge.request("plan.list", serde_json::json!({}));
    if let Some(plans) = list_resp.result.as_ref().and_then(|v| v.as_array()) {
        let has_draft = plans
            .iter()
            .any(|p| p.get("status").and_then(|v| v.as_str()).map(|s| s.to_lowercase()) == Some("draft".to_string()));
        if has_draft {
            return Ok(ActionResult::ActionError(
                "A Draft Plan already exists. Iterate on the existing Draft instead of creating a new one.".into(),
            ));
        }
    }
    let resp = bridge.request(
        "plan.create",
        serde_json::json!({
            "title": title,
            "description": description,
            "acceptance_criteria": acceptance_criteria,
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("create_plan failed: {}", msg)));
    }
    let id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    agent_log.info(&format!("CreatePlan: {} (id: {})", title, id));
    // Auto-approve Plan in non-Interactive modes
    if bridge.config().agents.coordinator.interview_mode != crate::config::InterviewMode::Interactive {
        let approve_resp = bridge.request("coordinator.accept_plan", serde_json::json!({ "plan_id": id }));
        if approve_resp.is_error() {
            agent_log.warn(&format!(
                "auto-approve failed for plan {}: {:?}",
                id, approve_resp.error
            ));
        } else {
            agent_log.info(&format!("auto-approved plan {} (interview_mode != interactive)", id));
        }
    }
    Ok(ActionResult::RecordCreated {
        collection: "plans".to_string(),
        id,
    })
}

/// Handle CreateSpec action.
pub(super) fn handle_create_spec(
    ctx: &AgentContext,
    plan_id: &str,
    title: &str,
    description: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    // Gap #27: Draft-awareness guard for specs under same plan
    let list_resp = bridge.request("spec.list", serde_json::json!({}));
    if let Some(specs) = list_resp.result.as_ref().and_then(|v| v.as_array()) {
        let has_draft = specs.iter().any(|s| {
            s.get("plan_id").and_then(|v| v.as_str()) == Some(plan_id)
                && s.get("status").and_then(|v| v.as_str()).map(|s| s.to_lowercase()) == Some("draft".to_string())
        });
        if has_draft {
            return Ok(ActionResult::ActionError(
                "A Draft Spec already exists under this Plan. Iterate on the existing Draft.".into(),
            ));
        }
    }
    let resp = bridge.request(
        "spec.create",
        serde_json::json!({
            "plan_id": plan_id,
            "title": title,
            "description": description,
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("create_spec failed: {}", msg)));
    }
    let id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    agent_log.info(&format!("CreateSpec: {} (id: {})", title, id));
    Ok(ActionResult::RecordCreated {
        collection: "specs".to_string(),
        id,
    })
}

/// Handle CreatePhase action.
pub(super) fn handle_create_phase(
    ctx: &AgentContext,
    spec_id: &str,
    title: &str,
    description: &str,
    order: u32,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    let resp = bridge.request(
        "phase.create",
        serde_json::json!({
            "spec_id": spec_id,
            "title": title,
            "description": description,
            "order": order,
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("create_phase failed: {}", msg)));
    }
    let id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    agent_log.info(&format!("CreatePhase: {} (id: {})", title, id));
    Ok(ActionResult::RecordCreated {
        collection: "phases".to_string(),
        id,
    })
}

/// Handle ProposePlan action.
pub(super) fn handle_propose_plan(
    ctx: &AgentContext,
    title: &str,
    description: &str,
    acceptance_criteria: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

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
    agent_log.info(&format!("ProposePlan: created draft plan {}", plan_id));
    // Auto-approve Plan in non-Interactive modes
    if bridge.config().agents.coordinator.interview_mode != crate::config::InterviewMode::Interactive {
        let approve_resp = bridge.request("coordinator.accept_plan", serde_json::json!({ "plan_id": plan_id }));
        if approve_resp.is_error() {
            agent_log.warn(&format!(
                "auto-approve failed for plan {}: {:?}",
                plan_id, approve_resp.error
            ));
        } else {
            agent_log.info(&format!(
                "auto-approved plan {} (interview_mode != interactive)",
                plan_id
            ));
        }
    }
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
    let agent_log = &ctx.log;

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

    agent_log.info(&format!(
        "ReviseParent {}/{}: reason={}, diagnostic={}",
        collection, id, reason, diagnostic
    ));
    Ok(ActionResult::Transitioned(format!(
        "{}/{} -> draft (bubble-up: {})",
        collection, id, reason
    )))
}

/// Handle InterviewQuestion action.
pub(super) fn handle_interview_question(ctx: &AgentContext, questions: &[String]) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    let interview_mode = bridge.config().agents.coordinator.interview_mode;
    if interview_mode == crate::config::InterviewMode::Auto {
        // Auto mode: synthesize answer from goal + repo context
        let goal_text = {
            let goals = ctx.stores.read_coordinator_goals()?;
            goals
                .values()
                .find(|g| g.active)
                .map(|g| g.goal.clone())
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
        agent_log.info(&format!(
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
        agent_log.info(&format!("InterviewQuestion: {} questions sent", questions.len()));
        Ok(ActionResult::Done(format!(
            "sent {} interview question(s)",
            questions.len()
        )))
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    
    
    use crate::agents::executor::{execute_action, ActionResult};
    use crate::agents::executor::tests::{
        test_stores, test_agent_context,
        create_test_hierarchy,
    };
    use crate::agents::{AgentAction, AgentType};
    
    
    
    use crate::test_util::TestDir;
    
    
    
    
    
    
    

    #[tokio::test]
    async fn test_execute_create_plan() {
        let dir = TestDir::new("loopr-exec-createplan");
        let stores = test_stores(&dir);

        let action = AgentAction::CreatePlan {
            title: "New Plan".to_string(),
            description: "Plan desc".to_string(),
            acceptance_criteria: "Tests pass".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "plans");
            assert!(!id.is_empty());
            assert_ne!(id, "unknown");
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_spec() {
        let dir = TestDir::new("loopr-exec-createspec");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreateSpec {
            plan_id,
            title: "New Spec".to_string(),
            description: "Spec desc".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "specs");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_phase() {
        let dir = TestDir::new("loopr-exec-createphase");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, spec_id, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreatePhase {
            spec_id,
            title: "New Phase".to_string(),
            description: "Phase desc".to_string(),
            order: 2,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "phases");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_plan_error_path() {
        let dir = TestDir::new("loopr-exec-plnerr");
        let stores = test_stores(&dir);

        let action1 = AgentAction::CreatePlan {
            title: "First Plan".to_string(),
            description: "desc".to_string(),
            acceptance_criteria: "pass".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        let action2 = AgentAction::CreatePlan {
            title: "Second Plan".to_string(),
            description: "desc".to_string(),
            acceptance_criteria: "pass".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Draft Plan already exists")),
            "expected draft-awareness error, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_execute_create_spec_error_path() {
        let dir = TestDir::new("loopr-exec-specerr");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action1 = AgentAction::CreateSpec {
            plan_id: plan_id.clone(),
            title: "New Spec".to_string(),
            description: "desc".to_string(),
        };
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        let action2 = AgentAction::CreateSpec {
            plan_id: plan_id.clone(),
            title: "Another Spec".to_string(),
            description: "desc".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Draft Spec already exists")),
            "expected draft-awareness error for spec, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_execute_create_phase_error_path() {
        let dir = TestDir::new("loopr-exec-phaseerr");
        let stores = test_stores(&dir);

        let action = AgentAction::CreatePhase {
            spec_id: "nonexistent-spec".to_string(),
            title: "New Phase".to_string(),
            description: "desc".to_string(),
            order: 1,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("create_phase failed")),
            "expected error for nonexistent spec, got: {:?}",
            result
        );
    }
}
