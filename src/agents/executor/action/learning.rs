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
    resource_tags: Option<&[String]>,
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
    if let Some(tags) = resource_tags {
        params["resource_tags"] = serde_json::json!(tags);
    }
    let resp = bridge.request("learning.create", params);
    if resp.is_error() {
        return Err(eyre!("create learning failed: {:?}", resp.error));
    }
    Ok(ActionResult::LearningCreated(content.to_string()))
}
