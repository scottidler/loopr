use eyre::{Result, eyre};

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle CreateLearning action.
pub(super) fn handle_create_learning(
    ctx: &AgentContext,
    work_id: Option<&str>,
    content: &str,
    scope: &str,
    source_id: &str,
    applicable_roles: Option<&[String]>,
    files: Option<&[String]>,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    // M9: Defense-in-depth - if source_id doesn't look like a record ID,
    // use work_id if available (fixes Reviewer sending title as source_id)
    let effective_source_id = if !source_id.starts_with("wi-")
        && !source_id.starts_with("phase-")
        && !source_id.starts_with("plan-")
        && !source_id.starts_with("spec-")
    {
        work_id.unwrap_or(source_id).to_string()
    } else {
        source_id.to_string()
    };
    let mut params = serde_json::json!({
        "content": content,
        "scope": scope,
        "source_id": effective_source_id,
    });
    if let Some(roles) = applicable_roles {
        params["applicable_roles"] = serde_json::json!(roles);
    }
    if let Some(tags) = files {
        params["files"] = serde_json::json!(tags);
    }
    let resp = bridge.request("learning.create", params);
    if resp.is_error() {
        return Err(eyre!("create learning failed: {:?}", resp.error));
    }
    Ok(ActionResult::LearningCreated(content.to_string()))
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {

    use crate::agents::executor::tests::{test_agent_context, test_stores};
    use crate::agents::executor::{ActionResult, execute_action};
    use crate::agents::{AgentAction, AgentKind};

    use crate::test_util::TestDir;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_create_learning_with_all_fields() {
        let dir = TestDir::new("loopr-exec-learning");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateLearning {
            content: "Always add tests".to_string(),
            scope: "global".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: Some(vec!["implementer".to_string()]),
            files: Some(vec!["src/".to_string()]),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::LearningCreated(ref c) if c == "Always add tests"),
            "expected LearningCreated, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_create_learning_minimal() {
        let dir = TestDir::new("loopr-exec-learnmin");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateLearning {
            content: "Minimal learning".to_string(),
            scope: "work".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: None,
            files: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::LearningCreated(_)));
    }
}
