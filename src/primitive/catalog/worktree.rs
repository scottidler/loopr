use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Creates an isolated git worktree for an agent.
pub struct CreateWorktree;

impl Primitive for CreateWorktree {
    fn name(&self) -> &'static str {
        "create-worktree"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let work_id = params["work-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'work-id'"))?;
            let base_ref = params["base-ref"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'base-ref'"))?;

            debug!("create-worktree: work-id={} base-ref={}", work_id, base_ref);

            let worktree_path = ctx.worktree_mgr.create_branch(work_id, base_ref)?;

            let mut values = HashMap::new();
            values.insert(
                "worktree-path".to_string(),
                serde_json::json!(worktree_path.display().to_string()),
            );

            Ok(PrimitiveOutput {
                values,
                summary: format!("created worktree for work '{}' at {}", work_id, worktree_path.display()),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "worktree-path".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "work-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "base-ref".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Removes a worktree from the filesystem.
pub struct CleanupWorktree;

impl Primitive for CleanupWorktree {
    fn name(&self) -> &'static str {
        "cleanup-worktree"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let work_id = params["work-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'work-id'"))?;

            debug!("cleanup-worktree: work-id={}", work_id);

            let worktree_path = ctx.worktree_mgr.worktree_dir.join(format!("agent-{}", work_id));
            let output = std::process::Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&worktree_path)
                .current_dir(ctx.repo_path)
                .output()?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eyre::bail!("cleanup-worktree failed: {}", stderr);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("cleaned up worktree for work '{}'", work_id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "work-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Deletes the agent branch after work is integrated or abandoned.
pub struct DeleteAgentBranch;

impl Primitive for DeleteAgentBranch {
    fn name(&self) -> &'static str {
        "delete-agent-branch"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let work_id = params["work-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'work-id'"))?;

            debug!("delete-agent-branch: work-id={}", work_id);

            let branch_name = format!("agent/{}", work_id);
            let output = std::process::Command::new("git")
                .args(["branch", "-D", &branch_name])
                .current_dir(ctx.repo_path)
                .output()?;

            // Idempotent: ignore "not found" errors
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("not found") {
                    eyre::bail!("delete-agent-branch failed: {}", stderr);
                }
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("deleted branch '{}'", branch_name),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "work-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Rebases a worktree to a new base reference (clears staleness).
pub struct RefreshWorktree;

impl Primitive for RefreshWorktree {
    fn name(&self) -> &'static str {
        "refresh-worktree"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let work_id = params["work-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'work-id'"))?;
            let new_base_ref = params["new-base-ref"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'new-base-ref'"))?;

            debug!("refresh-worktree: work-id={} new-base-ref={}", work_id, new_base_ref);

            let worktree_path = ctx.worktree_mgr.worktree_dir.join(format!("agent-{}", work_id));
            let output = std::process::Command::new("git")
                .args(["rebase", new_base_ref])
                .current_dir(&worktree_path)
                .output()?;

            let success = output.status.success();
            if !success {
                // Abort the failed rebase
                let _ = std::process::Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(&worktree_path)
                    .output();
            }

            let mut values = HashMap::new();
            values.insert("success".to_string(), serde_json::json!(success));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "refresh worktree '{}' onto {}: {}",
                    work_id,
                    new_base_ref,
                    if success { "ok" } else { "conflict - aborted" }
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "success".to_string(),
            field_type: OutputType::Bool,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "work-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "new-base-ref".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}
