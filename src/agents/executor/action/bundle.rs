use std::path::Path;

use eyre::{Result, eyre};

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;
use crate::agents::executor::util::resolve_latest_published_tick_id;
use crate::domain::bundle::BundleStatus;

/// Handle ProposeBundle action.
pub(super) async fn handle_propose_bundle(
    ctx: &AgentContext,
    worktree_path: &Path,
    work_id: Option<&str>,
    description: &str,
    claims: &[String],
    noop_reason: Option<&str>,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;
    let is_noop = noop_reason.is_some();
    let wi_id = work_id.ok_or_else(|| eyre!("propose_bundle requires work_id"))?;

    // Guard: reject noop bundles with uncommitted changes.
    // The LLM may have written files via write_file but then
    // incorrectly taken the noop path. Return an error so a
    // fresh session can self-correct with a normal bundle.
    if is_noop {
        let status_output = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree_path)
            .output()
            .await;
        if let Ok(output) = status_output {
            let status = String::from_utf8_lossy(&output.stdout);
            if !status.trim().is_empty() {
                agent_log.warn(&format!(
                    "Rejected noop bundle: worktree has uncommitted changes:\n{}",
                    status.trim()
                ));
                return Ok(ActionResult::ActionError(format!(
                    "Cannot propose a noop bundle while the worktree has uncommitted \
                     changes:\n{}\n\
                     You wrote files in this session that are not committed. \
                     Use `commit` first, then propose a normal bundle (without \
                     noop_reason). Do NOT use noop_reason if you made any changes.",
                    status.trim()
                )));
            }
        }
    }

    // For normal bundles: auto-commit any pending changes before creating
    // the bundle. Skip for noop bundles (no changes to commit).
    if !is_noop {
        let auto_commit = tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(worktree_path)
            .output()
            .await;
        if let Ok(add_out) = auto_commit
            && add_out.status.success()
        {
            let commit_out = tokio::process::Command::new("git")
                .args(["commit", "-m", &format!("impl: {}", description)])
                .current_dir(worktree_path)
                .output()
                .await;
            match commit_out {
                Ok(out) if out.status.success() => {
                    agent_log.info("Auto-committed pending changes before propose_bundle");
                }
                _ => {
                    agent_log.info("No pending changes to auto-commit before propose_bundle");
                }
            }
        }
    }

    // F2: Derive branch name deterministically from work_id - matches
    // WorktreeManager::create() which uses format!("agent/{}", work_id).
    // For noop bundles, use empty branch_name (Integrator skips merge).
    let branch_name = if is_noop {
        agent_log.info(&format!("Noop bundle: {}", noop_reason.unwrap_or("(no reason)")));
        String::new()
    } else {
        format!("agent/{}", wi_id)
    };

    // Capture the current HEAD SHA for audit and pre-merge verification
    let head_commit = if !is_noop {
        tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(worktree_path)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    };

    // Fix #1: Resolve base_tick_id from latest Published Tick
    let base_tick_id = resolve_latest_published_tick_id(bridge.stores());

    let mut params = serde_json::json!({
        "work_id": wi_id,
        "branch_name": branch_name,
        "claims": claims,
        "description": description,
    });
    if let Some(ref sha) = head_commit {
        params["head_commit"] = serde_json::Value::String(sha.clone());
    }
    if let Some(reason) = noop_reason {
        params["noop_reason"] = serde_json::Value::String(reason.to_string());
    }
    if let Some(tick_id) = &base_tick_id {
        params["base_tick_id"] = serde_json::Value::String(tick_id.clone());
    }

    let resp = bridge.request("bundle.create", params.clone());
    if resp.is_error() {
        // Fix #7: On stale bundle rejection (-32002), retry once
        if let Some(ref err) = resp.error
            && err.code == -32002
        {
            agent_log.warn(&format!(
                "Bundle stale ({}), retrying with fresh base_tick_id",
                err.message
            ));
            let new_base_tick_id = resolve_latest_published_tick_id(bridge.stores());
            if let Some(tick_id) = &new_base_tick_id {
                params["base_tick_id"] = serde_json::Value::String(tick_id.clone());
            }
            let retry_resp = bridge.request("bundle.create", params);
            if retry_resp.is_error() {
                return Err(eyre!("propose bundle failed after retry: {:?}", retry_resp.error));
            }
            return Ok(ActionResult::BundleProposed(description.to_string()));
        }
        return Err(eyre!("propose bundle failed: {:?}", resp.error));
    }
    Ok(ActionResult::BundleProposed(description.to_string()))
}

/// Handle TriageBundle action.
pub(super) fn handle_triage_bundle(ctx: &AgentContext, bundle_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    if !bundle_id.starts_with("bd-") {
        return Ok(ActionResult::ActionError(format!(
            "triage_bundle: '{}' is not a bundle ID (expected bd-* prefix). Use the bundle ID, not the work ID.",
            bundle_id
        )));
    }
    // Pre-flight status guard: only Proposed bundles can be triaged
    let bundles = bridge.stores().read_bundles()?;
    match bundles.get(bundle_id) {
        None => {
            return Ok(ActionResult::ActionError(format!(
                "triage_bundle: bundle {} not found",
                bundle_id
            )));
        }
        Some(bundle) if bundle.status != BundleStatus::Proposed => {
            let hint = match bundle.status {
                BundleStatus::Reviewed => "Use accept_bundle instead.",
                _ => "No triage action needed.",
            };
            return Ok(ActionResult::ActionError(format!(
                "triage_bundle: bundle {} is {} not Proposed. {}",
                bundle_id, bundle.status, hint
            )));
        }
        _ => {}
    }
    drop(bundles);
    let resp = bridge.request(
        "bundle.transition",
        serde_json::json!({
            "id": bundle_id,
            "target_status": "Triaged",
            "role": "coordinator",
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("triage_bundle failed: {}", msg)));
    }
    agent_log.info(&format!("TriageBundle: {} -> Triaged", bundle_id));
    Ok(ActionResult::Transitioned(format!("bundle/{} -> Triaged", bundle_id)))
}

/// Handle AcceptBundle action.
pub(super) fn handle_accept_bundle(ctx: &AgentContext, bundle_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    if !bundle_id.starts_with("bd-") {
        return Ok(ActionResult::ActionError(format!(
            "accept_bundle: '{}' is not a bundle ID (expected bd-* prefix). Use the bundle ID, not the work ID.",
            bundle_id
        )));
    }
    // Pre-flight status guard: only Triaged or Reviewed bundles can be accepted
    let bundles = bridge.stores().read_bundles()?;
    match bundles.get(bundle_id) {
        None => {
            return Ok(ActionResult::ActionError(format!(
                "accept_bundle: bundle {} not found",
                bundle_id
            )));
        }
        Some(bundle) if !matches!(bundle.status, BundleStatus::Triaged | BundleStatus::Reviewed) => {
            let hint = match bundle.status {
                BundleStatus::Proposed => "Use triage_bundle first.",
                _ => "No accept action needed.",
            };
            return Ok(ActionResult::ActionError(format!(
                "accept_bundle: bundle {} is {} not Triaged/Reviewed. {}",
                bundle_id, bundle.status, hint
            )));
        }
        _ => {}
    }
    drop(bundles);
    let resp = bridge.request(
        "bundle.transition",
        serde_json::json!({
            "id": bundle_id,
            "target_status": "Accepted",
            "role": "coordinator",
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("accept_bundle failed: {}", msg)));
    }
    agent_log.info(&format!("AcceptBundle: {} -> Accepted", bundle_id));
    Ok(ActionResult::Transitioned(format!("bundle/{} -> Accepted", bundle_id)))
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
    
    
    use crate::domain::tick::TickStatus;
    use crate::test_util::TestDir;
    
    
    
    
    
    
    

    #[tokio::test]
    async fn test_execute_propose_bundle() {
        let dir = TestDir::new("loopr-exec-propbundle");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec!["Implemented feature X".to_string()],
            noop_reason: None,
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref desc) if desc == "My bundle"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_propose_bundle_no_work() {
        let dir = TestDir::new("loopr-exec-propnowi");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec![],
            noop_reason: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work_id"));
    }

    // --- Group C: Agent management + domain actions ---

    #[tokio::test]
    async fn test_execute_triage_bundle() {
        let dir = TestDir::new("loopr-exec-triage");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/test",
                "description": "Test bundle",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::TriageBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Triaged")),
            "expected Transitioned to Triaged, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle() {
        let dir = TestDir::new("loopr-exec-accept");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accept",
                "description": "Accept test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Accepted")),
            "expected Transitioned to Accepted, got: {:?}",
            result
        );
    }

    // --- Group D: Lifecycle paths ---

    #[tokio::test]
    async fn test_execute_triage_bundle_full() {
        let dir = TestDir::new("loopr-exec-triagefull");
        let stores = test_stores(&dir);

        let action = AgentAction::TriageBundle {
            bundle_id: "bd-nonexistent".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not found")),
            "expected not-found error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_triage_bundle_rejects_work_id() {
        let dir = TestDir::new("loopr-exec-triage-wkid");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::TriageBundle {
            bundle_id: "wk-12345".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not a bundle ID")),
            "expected prefix validation error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle_rejects_work_id() {
        let dir = TestDir::new("loopr-exec-accept-wkid");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::AcceptBundle {
            bundle_id: "wk-12345".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not a bundle ID")),
            "expected prefix validation error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle_full() {
        let dir = TestDir::new("loopr-exec-acceptfull");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accepterr",
                "description": "Accept err test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("Use triage_bundle first")),
            "expected corrective hint for Proposed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_triage_bundle_rejects_reviewed_bundle() {
        let dir = TestDir::new("loopr-exec-triage-rev");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/triage-rev",
                "description": "Triage reviewed test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        let action = AgentAction::TriageBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("Use accept_bundle instead")),
            "expected corrective hint for Reviewed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_accept_bundle_rejects_proposed_bundle() {
        let dir = TestDir::new("loopr-exec-accept-prop");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accept-prop",
                "description": "Accept proposed test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("Use triage_bundle first")),
            "expected corrective hint for Proposed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_propose_bundle_includes_base_tick_id() {
        let dir = TestDir::new("loopr-exec-propbase");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let tick = crate::domain::tick::Tick {
            id: "tick-pub-1".to_string(),
            number: 1,
            status: TickStatus::Published,
            bundle_ids: vec![],
            attempted_bundle_ids: vec![],
            integration_sha: Some("abc123".to_string()),
            validation_log: String::new(),
            created_at: crate::id::now_millis(),
            updated_at: crate::id::now_millis(),
        };
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Bundle with base tick".to_string(),
            claims: vec!["claim".to_string()],
            noop_reason: None,
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Bundle with base tick"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_propose_bundle_uses_deterministic_branch_name() {
        let dir = TestDir::new("loopr-exec-f2branch");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);
        ctx.bridge.request(
            "work.transition",
            serde_json::json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator"}),
        );

        let _ = tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await;
        std::fs::write(dir.join("impl.txt"), "implementation").unwrap();
        let _ = tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        let action = AgentAction::ProposeBundle {
            description: "Test bundle".to_string(),
            claims: vec!["implemented feature".to_string()],
            noop_reason: None,
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(_)),
            "expected BundleProposed, got: {:?}",
            result
        );
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.values().next().expect("should have one bundle");
        assert_eq!(
            bundle.branch_name,
            format!("agent/{}", wi_id),
            "bundle branch should be deterministic agent/<work_id>"
        );
    }

    // --- determine_work_handback tests ---

    #[tokio::test]
    async fn test_noop_propose_bundle_creates_empty_branch() {
        let dir = TestDir::new("loopr-exec-noop-branch");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("Phase 1 already implemented this".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Work already complete"),
            "expected BundleProposed, got: {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles
            .values()
            .find(|b| b.work_id == wi_id)
            .expect("should have bundle");
        assert!(
            bundle.branch_name.is_empty(),
            "noop bundle should have empty branch_name"
        );
        assert_eq!(bundle.noop_reason.as_deref(), Some("Phase 1 already implemented this"));
    }

    #[tokio::test]
    async fn test_noop_bundle_handler_rejects_empty_branch_without_noop() {
        let dir = TestDir::new("loopr-exec-noop-reject");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "",
                "description": "should fail",
            }),
        );
        assert!(resp.is_error(), "empty branch_name without noop_reason should fail");
        let err_msg = resp.error.as_ref().unwrap().message.clone();
        assert!(
            err_msg.contains("branch_name"),
            "error should mention branch_name: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_noop_guard_rejects_dirty_worktree() {
        let dir = TestDir::new("loopr-exec-noop-dirty");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        std::fs::write(dir.join("todo.lua"), "print('hello')").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("already done".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("uncommitted changes")),
            "expected ActionError for dirty worktree noop, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_noop_guard_allows_clean_worktree() {
        let dir = TestDir::new("loopr-exec-noop-clean");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        std::fs::write(dir.join(".gitignore"), ".taskstore/\n*.log\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "add gitignore"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("Phase 1 already implemented this".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Work already complete"),
            "expected BundleProposed for clean noop, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_noop_bundle_handler_allows_empty_branch_with_noop() {
        let dir = TestDir::new("loopr-exec-noop-allow");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "",
                "noop_reason": "already done by phase 1",
                "description": "noop bundle",
            }),
        );
        assert!(
            !resp.is_error(),
            "empty branch_name with noop_reason should succeed: {:?}",
            resp.error
        );
        let bundle_id = resp.result.as_ref().unwrap()["id"].as_str().unwrap();
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(bundle_id).expect("bundle should exist");
        assert!(bundle.branch_name.is_empty());
        assert_eq!(bundle.noop_reason.as_deref(), Some("already done by phase 1"));
    }
}
