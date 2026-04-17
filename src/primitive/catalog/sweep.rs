use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Promotes a Pending record to Active/Ready when deps are satisfied.
pub struct PromoteRecord;

impl Primitive for PromoteRecord {
    fn name(&self) -> &'static str {
        "promote-record"
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

            debug!("promote-record: {}/{}", collection, id);

            let resp = ctx
                .bridge
                .request(&format!("{}.promote", collection), serde_json::json!({"id": id}));
            let promoted = resp.error.is_none();

            let mut values = HashMap::new();
            values.insert("promoted".to_string(), serde_json::json!(promoted));

            Ok(PrimitiveOutput {
                values,
                summary: format!("{} '{}': promoted={}", collection, id, promoted),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "promoted".to_string(),
            field_type: OutputType::Bool,
        }]
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
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Marks a record as Complete when all children are terminal.
pub struct CompleteRecord;

impl Primitive for CompleteRecord {
    fn name(&self) -> &'static str {
        "complete-record"
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

            debug!("complete-record: {}/{}", collection, id);

            let resp = ctx.bridge.request(
                &format!("{}.transition", collection),
                serde_json::json!({"id": id, "target-status": "Complete", "role": "Coordinator"}),
            );
            let completed = resp.error.is_none();

            let mut values = HashMap::new();
            values.insert("completed".to_string(), serde_json::json!(completed));

            Ok(PrimitiveOutput {
                values,
                summary: format!("{} '{}': completed={}", collection, id, completed),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "completed".to_string(),
            field_type: OutputType::Bool,
        }]
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
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Deterministic sweep: transitions all Integrated works to Done.
pub struct SweepToDone;

impl Primitive for SweepToDone {
    fn name(&self) -> &'static str {
        "sweep-to-done"
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

            debug!("sweep-to-done: plan-id={}", plan_id);

            let resp = ctx
                .bridge
                .request("coordinator.sweep_to_done", serde_json::json!({"plan-id": plan_id}));
            if let Some(err) = &resp.error {
                eyre::bail!("sweep-to-done failed: {}", err.message);
            }

            let count = resp.result.as_ref().and_then(|r| r["count"].as_u64()).unwrap_or(0) as u32;

            let mut values = HashMap::new();
            values.insert("count".to_string(), serde_json::json!(count));

            Ok(PrimitiveOutput {
                values,
                summary: format!("swept {} works to Done", count),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "count".to_string(),
            field_type: OutputType::U32,
        }]
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

/// Safety net: advances InReview works whose bundles are all terminal.
pub struct SweepStuckInreview;

impl Primitive for SweepStuckInreview {
    fn name(&self) -> &'static str {
        "sweep-stuck-inreview"
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

            debug!("sweep-stuck-inreview: plan-id={}", plan_id);

            let resp = ctx.bridge.request(
                "coordinator.sweep_stuck_inreview",
                serde_json::json!({"plan-id": plan_id}),
            );
            if let Some(err) = &resp.error {
                eyre::bail!("sweep-stuck-inreview failed: {}", err.message);
            }

            let count = resp.result.as_ref().and_then(|r| r["count"].as_u64()).unwrap_or(0) as u32;

            let mut values = HashMap::new();
            values.insert("count".to_string(), serde_json::json!(count));

            Ok(PrimitiveOutput {
                values,
                summary: format!("swept {} stuck InReview works", count),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "count".to_string(),
            field_type: OutputType::U32,
        }]
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

/// Surfaces a need-help condition.
pub struct Escalate;

impl Primitive for Escalate {
    fn name(&self) -> &'static str {
        "escalate"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let reason = params["reason"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'reason'"))?
                .to_string();

            debug!("escalate: reason={}", reason);

            let resp = ctx.bridge.request("coordinator.escalate", params);
            if let Some(err) = &resp.error {
                eyre::bail!("escalate failed: {}", err.message);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("escalated: {}", reason),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "reason".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "scope-id".to_string(),
                field_type: OutputType::String,
                required: false,
            },
            InputField {
                name: "details".to_string(),
                field_type: OutputType::Json,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}
