use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::{debug, warn};

use crate::domain::tick::TickStatus;
use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Verifies Published Tick SHAs are reachable from HEAD.
pub struct AuditTickShas;

impl Primitive for AuditTickShas {
    fn name(&self) -> &'static str {
        "audit-tick-shas"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("audit-tick-shas: starting");

            let published: Vec<(String, Option<String>)> = {
                let ticks = ctx.stores.read_ticks()?;
                ticks
                    .values()
                    .filter(|t| t.status() == TickStatus::Published)
                    .map(|t| (t.id.clone(), t.integration_sha.clone()))
                    .collect()
            };

            let mut unreachable = Vec::new();
            for (tick_id, sha_opt) in &published {
                let Some(sha) = sha_opt else { continue };
                let output = std::process::Command::new("git")
                    .args(["merge-base", "--is-ancestor", sha, "HEAD"])
                    .current_dir(ctx.repo_path)
                    .output();
                match output {
                    Ok(o) if !o.status.success() => {
                        warn!("audit-tick-shas: tick {} sha {} unreachable", tick_id, sha);
                        unreachable.push(serde_json::json!({
                            "tick-id": tick_id,
                            "sha": sha,
                        }));
                    }
                    Err(e) => {
                        warn!("audit-tick-shas: git error for tick {}: {}", tick_id, e);
                    }
                    _ => {}
                }
            }

            let catastrophic = !unreachable.is_empty();

            let mut values = HashMap::new();
            values.insert("unreachable".to_string(), serde_json::Value::Array(unreachable.clone()));
            values.insert("catastrophic".to_string(), serde_json::json!(catastrophic));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "audit-tick-shas: {} published, {} unreachable",
                    published.len(),
                    unreachable.len()
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        audit_output_schema("unreachable")
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Verifies merged Bundle commits are ancestors of their Tick's SHA.
pub struct AuditMergeAncestry;

impl Primitive for AuditMergeAncestry {
    fn name(&self) -> &'static str {
        "audit-merge-ancestry"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("audit-merge-ancestry: starting");

            let cutoff_ms = crate::id::now_millis() - (30 * 24 * 60 * 60 * 1000_i64);

            let merged_bundles: Vec<(String, String, String)> = {
                let bundles = ctx.stores.read_bundles()?;
                let ticks = ctx.stores.read_ticks()?;
                bundles
                    .values()
                    .filter(|b| format!("{:?}", b.status()) == "Merged" && b.created_at > cutoff_ms)
                    .filter_map(|b| {
                        let head = b.head_commit.as_ref()?;
                        let tick = ticks.values().find(|t| t.bundle_ids.contains(&b.id))?;
                        let tick_sha = tick.integration_sha.as_ref()?;
                        Some((b.id.clone(), head.clone(), tick_sha.clone()))
                    })
                    .collect()
            };

            let mut broken = Vec::new();
            for (bundle_id, bundle_sha, tick_sha) in &merged_bundles {
                let output = std::process::Command::new("git")
                    .args(["merge-base", "--is-ancestor", bundle_sha, tick_sha])
                    .current_dir(ctx.repo_path)
                    .output();
                match output {
                    Ok(o) if !o.status.success() => {
                        warn!("audit-merge-ancestry: bundle {} not ancestor of tick sha", bundle_id);
                        broken.push(serde_json::json!({
                            "bundle-id": bundle_id,
                            "tick-sha": tick_sha,
                        }));
                    }
                    Err(e) => {
                        warn!("audit-merge-ancestry: git error for bundle {}: {}", bundle_id, e);
                    }
                    _ => {}
                }
            }

            let catastrophic = !broken.is_empty();

            let mut values = HashMap::new();
            values.insert("broken".to_string(), serde_json::Value::Array(broken.clone()));
            values.insert("catastrophic".to_string(), serde_json::json!(catastrophic));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "audit-merge-ancestry: {} checked, {} broken",
                    merged_bundles.len(),
                    broken.len()
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        audit_output_schema("broken")
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Verifies every non-terminal Bundle still has its agent branch.
pub struct AuditBranches;

impl Primitive for AuditBranches {
    fn name(&self) -> &'static str {
        "audit-branches"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("audit-branches: starting");
            let non_terminal: Vec<(String, String)> = {
                let bundles = ctx.stores.read_bundles()?;
                bundles
                    .values()
                    .filter(|b| !b.status().is_terminal())
                    .map(|b| (b.id.clone(), b.branch_name.clone()))
                    .collect()
            };
            let mut mismatches = Vec::new();
            for (bundle_id, branch) in &non_terminal {
                let output = std::process::Command::new("git")
                    .args(["rev-parse", "--verify", branch])
                    .current_dir(ctx.repo_path)
                    .output();
                match output {
                    Ok(o) if !o.status.success() => {
                        warn!("audit-branches: bundle {} missing branch {}", bundle_id, branch);
                        mismatches.push(serde_json::json!({
                            "bundle-id": bundle_id,
                            "expected-branch": branch,
                        }));
                    }
                    Err(e) => warn!("audit-branches: git error for {}: {}", bundle_id, e),
                    _ => {}
                }
            }
            let mut values = HashMap::new();
            values.insert("mismatches".to_string(), serde_json::Value::Array(mismatches.clone()));
            values.insert("catastrophic".to_string(), serde_json::json!(false));
            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "audit-branches: {} checked, {} missing",
                    non_terminal.len(),
                    mismatches.len()
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        audit_output_schema("mismatches")
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Shared output schema for all audit primitives: a named JSON array + catastrophic bool.
fn audit_output_schema(array_name: &str) -> Vec<OutputField> {
    vec![
        OutputField {
            name: array_name.to_string(),
            field_type: OutputType::Json,
        },
        OutputField {
            name: "catastrophic".to_string(),
            field_type: OutputType::Bool,
        },
    ]
}
