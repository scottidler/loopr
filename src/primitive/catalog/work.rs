use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Priority-scored atomic claim of the next Ready work from the queue.
pub struct ClaimNextWork;

impl Primitive for ClaimNextWork {
    fn name(&self) -> &'static str {
        "claim-next-work"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("claim-next-work: scanning queue");

            let resp = ctx.bridge.request("work.claim_next", serde_json::json!({}));
            if let Some(err) = &resp.error {
                eyre::bail!("claim-next-work failed: {}", err.message);
            }

            let work_id = resp
                .result
                .as_ref()
                .and_then(|r| r["work_id"].as_str())
                .map(|s| s.to_string());

            let mut values = HashMap::new();
            values.insert(
                "work-id".to_string(),
                work_id
                    .as_deref()
                    .map(|s| serde_json::json!(s))
                    .unwrap_or(serde_json::Value::Null),
            );

            Ok(PrimitiveOutput {
                values,
                summary: match &work_id {
                    Some(id) => format!("claimed work '{}'", id),
                    None => "no work available".to_string(),
                },
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "work-id".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}

/// Resets a Work to Ready after bundle rejection or failure.
pub struct ResetWork;

impl Primitive for ResetWork {
    fn name(&self) -> &'static str {
        "reset-work"
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
            let reason = params["reason"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'reason'"))?;

            debug!("reset-work: work-id={} reason={}", work_id, reason);

            let resp = ctx.bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": work_id,
                    "target_status": "Ready",
                    "role": "Coordinator",
                    "override": true,
                    "reason": reason,
                }),
            );
            if let Some(err) = &resp.error {
                eyre::bail!("reset-work failed: {}", err.message);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("reset work '{}' to Ready: {}", work_id, reason),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "work-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "reason".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Increments a Work's session failure counter.
pub struct IncrementFailureCount;

impl Primitive for IncrementFailureCount {
    fn name(&self) -> &'static str {
        "increment-failure-count"
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

            debug!("increment-failure-count: work-id={}", work_id);

            let mut works = ctx.stores.write_works()?;
            let work = works
                .get_mut(work_id)
                .ok_or_else(|| eyre::eyre!("work '{}' not found", work_id))?;
            work.session_failure_count += 1;
            let count = work.session_failure_count;
            work.updated_at = crate::id::now_millis();

            let mut values = HashMap::new();
            values.insert("count".to_string(), serde_json::json!(count));

            Ok(PrimitiveOutput {
                values,
                summary: format!("work '{}' failure count: {}", work_id, count),
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
            name: "work-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}

/// Increments a Work's attempt counter.
pub struct IncrementAttemptCount;

impl Primitive for IncrementAttemptCount {
    fn name(&self) -> &'static str {
        "increment-attempt-count"
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

            debug!("increment-attempt-count: work-id={}", work_id);

            let mut works = ctx.stores.write_works()?;
            let work = works
                .get_mut(work_id)
                .ok_or_else(|| eyre::eyre!("work '{}' not found", work_id))?;
            work.attempt_count += 1;
            let count = work.attempt_count;
            work.updated_at = crate::id::now_millis();

            let mut values = HashMap::new();
            values.insert("count".to_string(), serde_json::json!(count));

            Ok(PrimitiveOutput {
                values,
                summary: format!("work '{}' attempt count: {}", work_id, count),
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
            name: "work-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}
