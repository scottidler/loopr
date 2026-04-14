use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::domain::work::WorkStatus;
use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Checks whether a Plan's goal is fully achieved. Pure query.
pub struct DetectGoalComplete;

impl Primitive for DetectGoalComplete {
    fn name(&self) -> &'static str {
        "detect-goal-complete"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let plan_id = params["plan-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'plan-id'"))?;

            debug!("detect-goal-complete: plan-id={}", plan_id);

            let works = ctx.stores.read_works()?;
            let mut done: u32 = 0;
            let mut abandoned: u32 = 0;
            let mut superseded: u32 = 0;
            let mut total: u32 = 0;

            // Walk hierarchy: works whose ancestry traces to this plan
            for work in works.values() {
                if !is_descendant_of_plan(ctx, work.parent_id.as_str(), plan_id)? {
                    continue;
                }
                total += 1;
                match work.status() {
                    WorkStatus::Done => done += 1,
                    WorkStatus::Superseded => superseded += 1,
                    WorkStatus::Abandoned => abandoned += 1,
                    _ => {}
                }
            }

            // GoalComplete: all non-Superseded works must be Done. Abandoned blocks completion.
            let complete = total > 0 && (done + superseded) == total && done > 0;

            let mut values = HashMap::new();
            values.insert("complete".to_string(), serde_json::json!(complete));
            values.insert("done-count".to_string(), serde_json::json!(done));
            values.insert("total-count".to_string(), serde_json::json!(total));
            values.insert("superseded-count".to_string(), serde_json::json!(superseded));
            values.insert("abandoned-count".to_string(), serde_json::json!(abandoned));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "plan {}: {}/{} done, {} superseded, {} abandoned, complete={}",
                    plan_id, done, total, superseded, abandoned, complete
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "complete".to_string(),
                field_type: OutputType::Bool,
            },
            OutputField {
                name: "done-count".to_string(),
                field_type: OutputType::U32,
            },
            OutputField {
                name: "total-count".to_string(),
                field_type: OutputType::U32,
            },
            OutputField {
                name: "superseded-count".to_string(),
                field_type: OutputType::U32,
            },
            OutputField {
                name: "abandoned-count".to_string(),
                field_type: OutputType::U32,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "plan-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Checks a numeric field against a maximum value. Pure query.
pub struct CheckThreshold;

impl Primitive for CheckThreshold {
    fn name(&self) -> &'static str {
        "check-threshold"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let collection = params["collection"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'collection'"))?;
            let id = params["id"].as_str().ok_or_else(|| eyre::eyre!("missing 'id'"))?;
            let field = params["field"].as_str().ok_or_else(|| eyre::eyre!("missing 'field'"))?;
            let max = params["max"].as_u64().ok_or_else(|| eyre::eyre!("missing 'max'"))? as u32;

            debug!("check-threshold: {}/{} field={} max={}", collection, id, field, max);

            let current = read_numeric_field(ctx, collection, id, field)?;
            let exceeded = current > max;

            let mut values = HashMap::new();
            values.insert("exceeded".to_string(), serde_json::json!(exceeded));
            values.insert("current".to_string(), serde_json::json!(current));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "{}/{}.{} = {} (max={}, exceeded={})",
                    collection, id, field, current, max, exceeded
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "exceeded".to_string(),
                field_type: OutputType::Bool,
            },
            OutputField {
                name: "current".to_string(),
                field_type: OutputType::U32,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "collection".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "field".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "max".to_string(),
                field_type: OutputType::U32,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Checks a ratio (numerator/denominator) against a threshold. Pure query.
pub struct CheckRatio;

impl Primitive for CheckRatio {
    fn name(&self) -> &'static str {
        "check-ratio"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let scope_id = params["scope-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'scope-id'"))?;
            let threshold = params["threshold"]
                .as_f64()
                .ok_or_else(|| eyre::eyre!("missing 'threshold'"))?;
            let num_query = &params["numerator-query"];
            let den_query = &params["denominator-query"];

            debug!("check-ratio: scope={} threshold={}", scope_id, threshold);

            let numerator = count_matching(ctx, num_query, scope_id)?;
            let denominator = count_matching(ctx, den_query, scope_id)?;

            let ratio = if denominator == 0 { 0.0 } else { numerator as f64 / denominator as f64 };
            let exceeded = ratio > threshold;

            let mut values = HashMap::new();
            values.insert("exceeded".to_string(), serde_json::json!(exceeded));
            values.insert("ratio".to_string(), serde_json::json!(ratio));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "ratio {}/{} = {:.2} (threshold={}, exceeded={})",
                    numerator, denominator, ratio, threshold, exceeded
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "exceeded".to_string(),
                field_type: OutputType::Bool,
            },
            OutputField {
                name: "ratio".to_string(),
                field_type: OutputType::F64,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "numerator-query".to_string(),
                field_type: OutputType::Json,
                required: true,
            },
            InputField {
                name: "denominator-query".to_string(),
                field_type: OutputType::Json,
                required: true,
            },
            InputField {
                name: "scope-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "threshold".to_string(),
                field_type: OutputType::F64,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Check if a parent_id traces back to a plan_id through the hierarchy.
fn is_descendant_of_plan(ctx: &PrimitiveContext<'_>, parent_id: &str, plan_id: &str) -> eyre::Result<bool> {
    // Direct child of plan
    if parent_id == plan_id {
        return Ok(true);
    }
    // Check phases -> spec -> plan
    if let Some(phase) = ctx.stores.read_phases()?.get(parent_id) {
        return is_descendant_of_plan(ctx, &phase.parent_id, plan_id);
    }
    // Check specs -> plan
    if let Some(spec) = ctx.stores.read_specs()?.get(parent_id) {
        return is_descendant_of_plan(ctx, &spec.parent_id, plan_id);
    }
    Ok(false)
}

/// Read a numeric (u32) field from a Work record by field name.
fn read_numeric_field(ctx: &PrimitiveContext<'_>, collection: &str, id: &str, field: &str) -> eyre::Result<u32> {
    match collection {
        "work" => {
            let works = ctx.stores.read_works()?;
            let work = works.get(id).ok_or_else(|| eyre::eyre!("work '{}' not found", id))?;
            match field {
                "session-failure-count" => Ok(work.session_failure_count),
                "attempt-count" => Ok(work.attempt_count),
                other => eyre::bail!("unknown numeric field '{}' on work", other),
            }
        }
        other => eyre::bail!("check-threshold not supported for collection '{}'", other),
    }
}

/// Count records matching a query filter within a scope.
/// Query format: {"collection": "work", "status": "Abandoned"}
fn count_matching(ctx: &PrimitiveContext<'_>, query: &serde_json::Value, scope_id: &str) -> eyre::Result<u32> {
    let collection = query["collection"].as_str().unwrap_or("work");
    let status_filter = query["status"].as_str();

    match collection {
        "work" => {
            let works = ctx.stores.read_works()?;
            let count = works
                .values()
                .filter(|w| is_descendant_of_plan(ctx, &w.parent_id, scope_id).unwrap_or(false))
                .filter(|w| {
                    status_filter
                        .map(|s| format!("{:?}", w.status()) == s || format!("{}", w.status()) == s)
                        .unwrap_or(true)
                })
                .count() as u32;
            Ok(count)
        }
        other => eyre::bail!("check-ratio not supported for collection '{}'", other),
    }
}
