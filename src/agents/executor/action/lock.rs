use eyre::{Result, eyre};

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle AcquireLock action.
pub(super) fn handle_acquire_lock(ctx: &AgentContext, resource: &str, holder_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    // Check if there's already an active lock on this resource
    let check_resp = bridge.request(
        "lock.list",
        serde_json::json!({ "resource": resource, "active_only": true }),
    );
    if check_resp.is_error() {
        return Err(eyre!("lock.list failed: {:?}", check_resp.error));
    }
    if let Some(result) = &check_resp.result
        && let Some(locks) = result.as_array()
        && !locks.is_empty()
    {
        // Resource already locked - return as ActionError so the LLM can self-correct
        let existing_holder = locks[0]
            .get("holder_id")
            .and_then(|v: &serde_json::Value| v.as_str())
            .unwrap_or("unknown");
        let existing_id = locks[0]
            .get("id")
            .and_then(|v: &serde_json::Value| v.as_str())
            .unwrap_or("unknown");
        return Ok(ActionResult::ActionError(format!(
            "resource '{}' already locked by {} (lock_id: {})",
            resource, existing_holder, existing_id
        )));
    }

    // Create the lock - granted_by is the holder_id (self-granted by coordinator)
    let resp = bridge.request(
        "lock.create",
        serde_json::json!({
            "resource": resource,
            "holder_id": holder_id,
            "granted_by": holder_id,
        }),
    );
    if resp.is_error() {
        return Err(eyre!("lock.create failed: {:?}", resp.error));
    }
    let lock_id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    ctx.info(&format!(
        "Lock acquired: {} on '{}' for {}",
        lock_id, resource, holder_id
    ));
    Ok(ActionResult::LockAcquired(lock_id))
}

/// Handle ReleaseLock action.
pub(super) fn handle_release_lock(ctx: &AgentContext, lock_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    let resp = bridge.request("lock.release", serde_json::json!({ "id": lock_id }));
    if resp.is_error() {
        return Err(eyre!("lock.release failed: {:?}", resp.error));
    }
    ctx.info(&format!("Lock released: {}", lock_id));
    Ok(ActionResult::LockReleased(lock_id.to_string()))
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {

    use crate::agents::executor::tests::{test_agent_context, test_agent_context_with_config, test_stores};
    use crate::agents::executor::{ActionResult, execute_action};
    use crate::agents::{AgentAction, AgentKind};
    use crate::config::Config;

    use crate::test_util::TestDir;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_action_acquire_lock() {
        let dir = TestDir::new("loopr-exec-acqlock");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-123".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::LockAcquired(lock_id) = &result {
            assert!(!lock_id.is_empty());
            assert_ne!(lock_id, "unknown");
        } else {
            panic!("expected LockAcquired result, got {:?}", result);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_action_acquire_lock_conflict() {
        let dir = TestDir::new("loopr-exec-lockconf");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-100".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::LockAcquired(_)));

        let action2 = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-200".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        if let ActionResult::ActionError(msg) = &result2 {
            assert!(
                msg.contains("already locked"),
                "expected conflict message, got: {}",
                msg
            );
        } else {
            panic!("expected ActionError for lock conflict, got {:?}", result2);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_action_release_lock() {
        let dir = TestDir::new("loopr-exec-rellock");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let acquire_action = AgentAction::AcquireLock {
            resource: "src/lib.rs".to_string(),
            holder_id: "wi-456".to_string(),
        };
        let acquire_result = execute_action(&acquire_action, &ctx, &dir, None).await.unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = acquire_result {
            id
        } else {
            panic!("expected LockAcquired");
        };

        let release_action = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        let release_result = execute_action(&release_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::LockReleased(id) = &release_result {
            assert_eq!(id, &lock_id);
        } else {
            panic!("expected LockReleased result, got {:?}", release_result);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_action_acquire_after_release() {
        let dir = TestDir::new("loopr-exec-reacq");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let action1 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-1".to_string(),
        };
        let r1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = r1 {
            id
        } else {
            panic!("expected LockAcquired");
        };

        let release = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        execute_action(&release, &ctx, &dir, None).await.unwrap();

        let action2 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-2".to_string(),
        };
        let r2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(matches!(r2, ActionResult::LockAcquired(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_lock_conflict_policy_ignore() {
        let dir = TestDir::new("loopr-exec-lockign");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "locked.txt", "holder_id": "agent-other", "granted_by": "agent-other" }),
        );

        let action = AgentAction::WriteFile {
            path: "locked.txt".to_string(),
            content: "advisory allows this".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileWritten(_)),
            "expected write to succeed under advisory policy, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_lock_conflict_policy_warn() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-lockwarn");
        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentKind::Coordinator, config);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "strict.txt", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );

        let action = AgentAction::WriteFile {
            path: "strict.txt".to_string(),
            content: "should be blocked".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked") && msg.contains("LockStrict")),
            "expected lock-blocked error, got: {:?}",
            result
        );
    }
}
