use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Creates an advisory lock on a resource (file path).
pub struct AcquireLock;

impl Primitive for AcquireLock {
    fn name(&self) -> &'static str {
        "acquire-lock"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("acquire-lock: resource={}", params["resource"]);

            let resp = ctx.bridge.request("lock.create", params);
            if let Some(err) = &resp.error {
                eyre::bail!("acquire-lock failed: {}", err.message);
            }

            let lock_id = resp
                .result
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .unwrap_or("")
                .to_string();
            let acquired = resp
                .result
                .as_ref()
                .and_then(|r| r["acquired"].as_bool())
                .unwrap_or(true);

            let mut values = HashMap::new();
            values.insert("lock-id".to_string(), serde_json::json!(lock_id));
            values.insert("acquired".to_string(), serde_json::json!(acquired));

            Ok(PrimitiveOutput {
                values,
                summary: format!("lock '{}' acquired={}", lock_id, acquired),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "lock-id".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "acquired".to_string(),
                field_type: OutputType::Bool,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "resource".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "holder-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "ttl-secs".to_string(),
                field_type: OutputType::U32,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }
}

/// Releases an advisory lock.
pub struct ReleaseLock;

impl Primitive for ReleaseLock {
    fn name(&self) -> &'static str {
        "release-lock"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("release-lock: {:?}", params);

            let resp = ctx.bridge.request("lock.release", params);
            if let Some(err) = &resp.error {
                eyre::bail!("release-lock failed: {}", err.message);
            }

            let count = resp.result.as_ref().and_then(|r| r["count"].as_u64()).unwrap_or(1) as u32;

            let mut values = HashMap::new();
            values.insert("count".to_string(), serde_json::json!(count));

            Ok(PrimitiveOutput {
                values,
                summary: format!("released {} lock(s)", count),
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
        vec![
            InputField {
                name: "lock-id".to_string(),
                field_type: OutputType::String,
                required: false,
            },
            InputField {
                name: "holder-id".to_string(),
                field_type: OutputType::String,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}
