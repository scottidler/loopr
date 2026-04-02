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
