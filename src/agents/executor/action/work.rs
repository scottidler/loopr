use eyre::Result;

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle CreateWork action.
pub(super) fn handle_create_work(
    ctx: &AgentContext,
    phase_id: &str,
    title: &str,
    description: &str,
    resource_tags: &[String],
    acceptance_criteria: &[String],
    dependencies: &[String],
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;
    let resp = bridge.request(
        "work.create",
        serde_json::json!({
            "phase_id": phase_id,
            "title": title,
            "description": description,
            "resource_tags": resource_tags,
            "acceptance_criteria": acceptance_criteria,
            "dependencies": dependencies,
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("create_work failed: {}", msg)));
    }
    let id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    agent_log.info(&format!("CreateWork: {} (id: {}, deps: {:?})", title, id, dependencies));
    Ok(ActionResult::RecordCreated {
        collection: "works".to_string(),
        id,
    })
}

/// Handle AssignAgent action.
pub(super) fn handle_assign_agent(ctx: &AgentContext, agent_type_str: &str, target_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    // For implementer assignments, check dependencies and auto-transition work
    if agent_type_str == "implementer" {
        let get_resp = bridge.request("work.get", serde_json::json!({ "id": target_id }));

        // Check dependencies: all must be Done before assignment
        if let Some(result) = &get_resp.result {
            let deps: Vec<String> = result
                .get("dependencies")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            for dep_id in &deps {
                let dep_resp = bridge.request("work.get", serde_json::json!({ "id": dep_id }));
                let dep_status = dep_resp
                    .result
                    .as_ref()
                    .and_then(|v| v.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                if dep_status != "Done" {
                    let dep_title = dep_resp
                        .result
                        .as_ref()
                        .and_then(|v| v.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    return Ok(ActionResult::DependencyNotMet {
                        work_id: target_id.to_string(),
                        message: format!(
                            "dependency '{}' ({}) is {} (must be Done)",
                            dep_title, dep_id, dep_status
                        ),
                    });
                }
            }
        }

        let wi_status = get_resp
            .result
            .as_ref()
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match wi_status {
            "Draft" => {
                // Draft -> Ready -> InProgress (assignee set on InProgress transition)
                let r1 = bridge.request(
                    "work.transition",
                    serde_json::json!({ "id": target_id, "target_status": "Ready", "role": "coordinator" }),
                );
                if r1.is_error() {
                    let msg = r1.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    return Ok(ActionResult::ActionError(format!(
                        "auto-transition Draft->Ready failed: {}",
                        msg
                    )));
                }
                let r2 = bridge.request(
                    "work.transition",
                    serde_json::json!({ "id": target_id, "target_status": "InProgress", "role": "coordinator", "assignee": agent_type_str }),
                );
                if r2.is_error() {
                    let msg = r2.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    return Ok(ActionResult::ActionError(format!(
                        "auto-transition Ready->InProgress failed: {}",
                        msg
                    )));
                }
                agent_log.info(&format!(
                    "AssignAgent: auto-transitioned work {} Draft->Ready->InProgress",
                    target_id
                ));
            }
            "Ready" => {
                // Ready -> InProgress
                let r = bridge.request(
                    "work.transition",
                    serde_json::json!({ "id": target_id, "target_status": "InProgress", "role": "coordinator", "assignee": agent_type_str }),
                );
                if r.is_error() {
                    let msg = r.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    return Ok(ActionResult::ActionError(format!(
                        "auto-transition Ready->InProgress failed: {}",
                        msg
                    )));
                }
                agent_log.info(&format!(
                    "AssignAgent: auto-transitioned work {} Ready->InProgress",
                    target_id
                ));
            }
            "InProgress" => {
                // Already correct
            }
            other => {
                return Ok(ActionResult::ActionError(format!(
                    "INVALID: Work '{}' is in state '{}' and cannot be assigned an agent. \
                     Only Draft, Ready, or InProgress works are assignable. \
                     You MUST NOT assign agents to completed or abandoned work. \
                     Review the Actionable Works list and assign agents to Ready tasks instead.",
                    target_id, other
                )));
            }
        }
    }

    // Map target_id to the correct param based on agent type
    let mut params = serde_json::json!({ "agent_type": agent_type_str });
    match agent_type_str {
        "implementer" => {
            if !target_id.starts_with("wk-") {
                return Ok(ActionResult::ActionError(format!(
                    "assign_agent implementer: '{}' is not a work ID (expected wk-* prefix)",
                    target_id
                )));
            }
            params["work_id"] = serde_json::json!(target_id);
        }
        "reviewer" => {
            if !target_id.starts_with("bd-") {
                return Ok(ActionResult::ActionError(format!(
                    "assign_agent reviewer: '{}' is not a bundle ID (expected bd-* prefix)",
                    target_id
                )));
            }
            params["bundle_id"] = serde_json::json!(target_id);
        }
        _ => {
            params["target_id"] = serde_json::json!(target_id);
        }
    }
    let resp = bridge.request("agent.start", params);
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("assign_agent failed: {}", msg)));
    }
    let session_id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    agent_log.info(&format!(
        "AssignAgent: {} -> {} (session: {})",
        agent_type_str, target_id, session_id
    ));
    Ok(ActionResult::AgentSpawned {
        session_id,
        agent_type: agent_type_str.to_string(),
    })
}

/// Handle SpawnResearcher action.
pub(super) fn handle_spawn_researcher(ctx: &AgentContext, query: &str, scope_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;
    let resp = bridge.request(
        "agent.start",
        serde_json::json!({
            "agent_type": "researcher",
            "target_id": scope_id,
            "query": query,
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("spawn_researcher failed: {}", msg)));
    }
    let session_id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    agent_log.info(&format!(
        "SpawnResearcher: {} (scope: {}, session: {})",
        query, scope_id, session_id
    ));
    Ok(ActionResult::AgentSpawned {
        session_id,
        agent_type: "researcher".to_string(),
    })
}

/// Handle TransitionWork (generic Transition action).
pub(super) fn handle_transition(
    ctx: &AgentContext,
    collection: &str,
    id: &str,
    target_status: &str,
    role: Option<&str>,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_type = ctx.session.agent_type;
    // If role is not specified, infer from agent_type
    let effective_role = role
        .map(|r| r.to_string())
        .unwrap_or_else(|| agent_type.default_role().to_string());
    let params = serde_json::json!({ "id": id, "target_status": target_status, "role": effective_role });
    let method = format!("{}.transition", super::super::util::normalize_collection(collection));
    let resp = bridge.request(&method, params);
    if resp.is_error() {
        return Err(eyre::eyre!("transition failed: {:?}", resp.error));
    }
    Ok(ActionResult::Transitioned(format!(
        "{}/{} -> {}",
        collection, id, target_status
    )))
}

/// Handle OverrideWork action.
pub(super) fn handle_override_work(
    ctx: &AgentContext,
    work_id: &str,
    target_status: &str,
    reason: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    // Guard: reject override-to-Ready if any bundle is Merged or Integrating
    if target_status == "Ready" {
        let bundles = bridge.stores().read_bundles()?;
        let blocking = bundles.values().find(|b| {
            b.work_id == *work_id
                && matches!(
                    b.status,
                    crate::domain::bundle::BundleStatus::Merged | crate::domain::bundle::BundleStatus::Integrating
                )
        });
        let blocked = blocking.map(|b| (b.id.clone(), b.status));
        drop(bundles);
        if let Some((bundle_id, status)) = blocked {
            let msg = format!(
                "Cannot override Work {} to Ready: bundle {} is {:?}. \
                 Code is merged or merge is in flight. \
                 Do not retry this override - let the Integrator finish.",
                work_id, bundle_id, status
            );
            agent_log.warn(&format!("OverrideWork: {}", msg));
            return Ok(ActionResult::ActionError(msg));
        }
    }

    // Kill any active agent sessions on this Work
    {
        let sessions = bridge.stores().read_agent_sessions()?;
        let active_sessions: Vec<String> = sessions
            .values()
            .filter(|s| s.work_id.as_deref() == Some(work_id) && !s.status.is_terminal())
            .map(|s| s.id.clone())
            .collect();
        drop(sessions);

        for sid in &active_sessions {
            let stop_resp = bridge.request("agent.stop", serde_json::json!({ "session_id": sid }));
            if stop_resp.is_error() {
                agent_log.warn(&format!(
                    "OverrideWork: failed to stop session {}: {:?}",
                    sid, stop_resp.error
                ));
            } else {
                agent_log.info(&format!("OverrideWork: stopped session {}", sid));
            }
        }
    }

    // Override transition
    let resp = bridge.request(
        "work.transition",
        serde_json::json!({
            "id": work_id,
            "target_status": target_status,
            "role": "coordinator",
            "override": true,
            "reason": reason,
        }),
    );

    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        agent_log.warn(&format!(
            "OverrideWork: transition failed for {} (may have already recovered): {}",
            work_id, msg
        ));
        return Ok(ActionResult::ActionError(format!(
            "override_work transition failed: {}",
            msg
        )));
    }

    // Release any active locks held by this Work
    {
        let locks = bridge.stores().read_locks()?;
        let work_locks: Vec<String> = locks
            .values()
            .filter(|l| l.holder_id == *work_id && matches!(l.status, crate::domain::lock::LockStatus::Active))
            .map(|l| l.id.clone())
            .collect();
        drop(locks);

        for lid in &work_locks {
            let release_resp = bridge.request("lock.release", serde_json::json!({ "id": lid }));
            if release_resp.is_error() {
                agent_log.warn(&format!(
                    "OverrideWork: failed to release lock {}: {:?}",
                    lid, release_resp.error
                ));
            } else {
                agent_log.info(&format!("OverrideWork: released lock {}", lid));
            }
        }
    }

    // Create audit Learning
    let _ = bridge.request(
        "learning.create",
        serde_json::json!({
            "content": format!("Override: Work {} -> {} (reason: {})", work_id, target_status, reason),
            "scope": "work",
            "source_id": work_id,
            "applicable_roles": ["coordinator"],
            "resource_tags": ["override", "audit"],
        }),
    );

    agent_log.info(&format!(
        "OverrideWork: {} -> {} (reason: {})",
        work_id, target_status, reason
    ));
    Ok(ActionResult::Transitioned(format!(
        "override: work/{} -> {}",
        work_id, target_status
    )))
}
