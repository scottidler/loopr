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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::executor::{execute_action, ActionResult};
    use crate::agents::executor::tests::{
        test_stores, test_agent_context,
        create_test_hierarchy,
    };
    use crate::agents::{AgentAction, AgentKind};
    
    use crate::daemon::context::Stores;
    use crate::domain::bundle::BundleStatus;
    use crate::test_util::TestDir;
    use std::sync::Arc;
    

    #[tokio::test]
    async fn test_transition_action_uses_correct_param() {
        let dir = TestDir::new("loopr-exec-transparam");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id.clone(),
            target_status: "Abandoned".to_string(),
            role: Some("coordinator".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_auto_transitions_draft() {
        let dir = TestDir::new("loopr-exec-autotrans");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        let wi_resp = ctx.bridge.request("work.get", serde_json::json!({"id": wi_id}));
        let wi_status = wi_resp
            .result
            .as_ref()
            .unwrap()
            .get("status")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(
            wi_status, "InProgress",
            "work item should be InProgress after auto-transition"
        );

        assert!(
            matches!(result, ActionResult::AgentSpawned { .. } | ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_create_work() {
        let dir = TestDir::new("loopr-exec-createwi");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, phase_id, wi_id) = create_test_hierarchy(&ctx.bridge);
        ctx.bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id, "target_status": "Ready", "role": "coordinator"
            }),
        );

        let action = AgentAction::CreateWork {
            phase_id,
            title: "New WI".to_string(),
            description: "WI desc".to_string(),
            resource_tags: vec!["src/".to_string()],
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

    // --- Group B: Git operations ---

    #[tokio::test]
    async fn test_execute_spawn_researcher() {
        let dir = TestDir::new("loopr-exec-spawnres");
        let stores = test_stores(&dir);

        let action = AgentAction::SpawnResearcher {
            query: "How does auth work?".to_string(),
            scope_id: "plan-1".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::AgentSpawned { ref agent_type, .. } if agent_type == "researcher")
                || matches!(result, ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_transition_role_inference() {
        let dir = TestDir::new("loopr-exec-roleinfer");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id,
            target_status: "Abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned with inferred role, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_transition_role_inference_all_collections() {
        let dir = TestDir::new("loopr-exec-transall");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (plan_id, spec_id, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "plans".to_string(),
            id: plan_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("plans")),
            "expected Transitioned for plans, got: {:?}",
            result
        );

        let action = AgentAction::Transition {
            collection: "specs".to_string(),
            id: spec_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Transitioned(_)));

        let action = AgentAction::Transition {
            collection: "phases".to_string(),
            id: phase_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Transitioned(_)));
    }

    #[tokio::test]
    async fn test_transition_assignee_validation_for_inprogress() {
        let dir = TestDir::new("loopr-exec-assignee");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id.clone(),
            target_status: "InProgress".to_string(),
            role: Some("coordinator".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err(), "InProgress without assignee should fail: {:?}", result);
    }

    #[tokio::test]
    async fn test_execute_create_work_error_path() {
        let dir = TestDir::new("loopr-exec-wierr");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateWork {
            phase_id: "nonexistent-phase".to_string(),
            title: "New WI".to_string(),
            description: "desc".to_string(),
            resource_tags: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("create_work failed")),
            "expected error for nonexistent phase, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_spawn_researcher_via_action() {
        let dir = TestDir::new("loopr-exec-spawnres2");
        let stores = test_stores(&dir);

        let action = AgentAction::SpawnResearcher {
            query: "What patterns are used in the codebase?".to_string(),
            scope_id: "spec-1".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::AgentSpawned { ref agent_type, .. } if agent_type == "researcher")
                || matches!(result, ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_dependency_not_met() {
        let dir = TestDir::new("loopr-exec-depnotmet");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let dep_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Dep WI",
                "description": "dep desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
            }),
        );
        let dep_id = dep_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let wi_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Work WI",
                "description": "work desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
                "dependencies": [dep_id],
            }),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::DependencyNotMet { .. }),
            "expected DependencyNotMet, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_dependency_met() {
        let dir = TestDir::new("loopr-exec-depmet");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let dep_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Dep WI Done",
                "description": "dep desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
            }),
        );
        let dep_id = dep_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        {
            let mut wis = stores.works.write().unwrap();
            if let Some(wi) = wis.get_mut(&dep_id) {
                wi.status = crate::domain::work::WorkStatus::Done;
                wi.updated_at = crate::id::now_millis();
                if let Some(store_arc) = &stores.store {
                    let _ = store_arc.lock().unwrap().update(wi.clone());
                }
            }
        }

        let wi_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Work WI With Met Dep",
                "description": "work desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
                "dependencies": [dep_id],
            }),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::AgentSpawned { .. }),
            "expected AgentSpawned, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_create_work_with_dependencies() {
        let dir = TestDir::new("loopr-exec-wideps");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, phase_id, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Dependent WI".to_string(),
            description: "depends on first".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec![wi_id.clone()],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(matches!(result, ActionResult::RecordCreated { .. }));

        if let ActionResult::RecordCreated { id, .. } = result {
            let wi = stores.works.read().unwrap().get(&id).cloned().unwrap();
            assert_eq!(wi.dependencies, vec![wi_id]);
        }
    }

    #[tokio::test]
    async fn test_create_work_duplicate_rejected() {
        let dir = TestDir::new("loopr-exec-widup");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let action1 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Unique WI".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        let action2 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Unique WI".to_string(),
            description: "different desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Duplicate")),
            "expected duplicate error, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_create_work_duplicate_case_insensitive() {
        let dir = TestDir::new("loopr-exec-widupcase");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let action1 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Add Login".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let _ = execute_action(&action1, &ctx, &dir, None).await;

        let action2 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "add login".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Duplicate")),
            "expected case-insensitive duplicate error, got: {:?}",
            result2
        );
    }

    // --- Fix #1: resolve_latest_published_tick_id tests ---

    #[tokio::test]
    async fn test_assign_agent_to_done_work_returns_directive_error() {
        let dir = TestDir::new("loopr-exec-assigndone");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        {
            let mut works = stores.works.write().unwrap();
            if let Some(wi) = works.get_mut(&wi_id) {
                wi.status = crate::domain::work::WorkStatus::Done;
                let wi_clone = wi.clone();
                if let Some(store) = &stores.store {
                    let _ = store.lock().unwrap().update(wi_clone);
                }
            }
        }

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        match result {
            ActionResult::ActionError(msg) => {
                assert!(msg.contains("INVALID"), "error should be directive: {}", msg);
                assert!(
                    msg.contains("MUST NOT assign"),
                    "error should instruct LLM not to assign: {}",
                    msg
                );
                assert!(
                    msg.contains("Ready tasks instead"),
                    "error should redirect to Ready tasks: {}",
                    msg
                );
            }
            other => panic!("expected ActionError, got: {:?}", other),
        }
    }

    // --- Merged Bundle Override Guard tests ---

    fn create_work_at_inreview_with_bundle(
        bridge: &AgentIpcBridge,
        stores: &Arc<Stores>,
        bundle_status: &str,
    ) -> (String, String) {
        let (_, _, _, wi_id) = create_test_hierarchy(bridge);

        bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator",
                "assignee": "test-impl",
            }),
        );

        let bundle_resp = bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/guard-test",
                "description": "guard test bundle",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id,
                "target_status": "InReview",
                "role": "implementer",
            }),
        );

        let chain: Vec<(&str, &str)> = match bundle_status {
            "Proposed" => vec![],
            "Triaged" => vec![("Triaged", "coordinator")],
            "Reviewed" => vec![("Triaged", "coordinator"), ("Reviewed", "reviewer")],
            "Accepted" => vec![
                ("Triaged", "coordinator"),
                ("Reviewed", "reviewer"),
                ("Accepted", "coordinator"),
            ],
            "Integrating" => vec![
                ("Triaged", "coordinator"),
                ("Reviewed", "reviewer"),
                ("Accepted", "coordinator"),
                ("Integrating", "integrator"),
            ],
            "Merged" => vec![
                ("Triaged", "coordinator"),
                ("Reviewed", "reviewer"),
                ("Accepted", "coordinator"),
                ("Integrating", "integrator"),
                ("Merged", "integrator"),
            ],
            "Rejected" => vec![("Triaged", "coordinator"), ("Rejected", "coordinator")],
            _ => vec![],
        };

        for (status, role) in chain {
            let mut params = serde_json::json!({
                "id": bundle_id,
                "target_status": status,
                "role": role,
            });
            if status == "Reviewed" {
                params["verification"] = serde_json::json!("tests passed");
            }
            bridge.request("bundle.transition", params);
        }

        {
            let mut bundles = stores.write_bundles().unwrap();
            if let Some(b) = bundles.get_mut(&bundle_id) {
                b.status = match bundle_status {
                    "Merged" => BundleStatus::Merged,
                    "Integrating" => BundleStatus::Integrating,
                    "Rejected" => BundleStatus::Rejected,
                    _ => b.status,
                };
            }
        }

        (wi_id, bundle_id)
    }

    #[tokio::test]
    async fn test_override_guard_merged_blocks_ready() {
        let dir = TestDir::new("loopr-exec-guard-merged");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (wi_id, bundle_id) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Merged");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "stale rejection".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        match result {
            ActionResult::ActionError(msg) => {
                assert!(
                    msg.contains(&bundle_id),
                    "error should name the blocking bundle: {}",
                    msg
                );
                assert!(msg.contains("Merged"), "error should mention Merged status: {}", msg);
                assert!(
                    msg.contains("Do not retry"),
                    "error should tell LLM to back off: {}",
                    msg
                );
            }
            other => panic!("expected ActionError, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_override_guard_integrating_blocks_ready() {
        let dir = TestDir::new("loopr-exec-guard-integrating");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (wi_id, bundle_id) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Integrating");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "stale rejection".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        match result {
            ActionResult::ActionError(msg) => {
                assert!(
                    msg.contains(&bundle_id),
                    "error should name the blocking bundle: {}",
                    msg
                );
                assert!(
                    msg.contains("Integrating"),
                    "error should mention Integrating status: {}",
                    msg
                );
            }
            other => panic!("expected ActionError, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_override_guard_rejected_allows_ready() {
        let dir = TestDir::new("loopr-exec-guard-rejected");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (wi_id, _) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Rejected");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "no valid bundle".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "override to Ready should succeed with only Rejected bundles, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_override_guard_merged_allows_abandoned() {
        let dir = TestDir::new("loopr-exec-guard-abandoned");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (wi_id, _) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Merged");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Abandoned".to_string(),
            reason: "pruning dead end".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "override to Abandoned should bypass guard even with Merged bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_override_guard_mixed_bundles_merged_blocks() {
        let dir = TestDir::new("loopr-exec-guard-mixed");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (wi_id, _) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Merged");

        let bundle2_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/guard-test-2",
                "description": "second bundle",
            }),
        );
        let bundle2_id = bundle2_resp.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        {
            let mut bundles = stores.write_bundles().unwrap();
            if let Some(b) = bundles.get_mut(&bundle2_id) {
                b.status = BundleStatus::Rejected;
            }
        }

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "stale rejection".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::ActionError(_)),
            "Merged bundle should block even when a Rejected bundle also exists, got: {:?}",
            result
        );
    }

    // --- Noop Bundle Pathway tests ---
}
