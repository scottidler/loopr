use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Combines multiple Works that touched overlapping files into a single replacement.
pub struct CombineConflictingWorks;

impl Primitive for CombineConflictingWorks {
    fn name(&self) -> &'static str {
        "combine-conflicting-works"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let work_ids: Vec<String> = params["work-ids"]
                .as_array()
                .ok_or_else(|| eyre::eyre!("missing 'work-ids'"))?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            let conflicting_files: Vec<String> = params["conflicting-files"]
                .as_array()
                .ok_or_else(|| eyre::eyre!("missing 'conflicting-files'"))?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();

            if work_ids.len() < 2 {
                eyre::bail!("combine-conflicting-works requires at least 2 work IDs");
            }

            debug!(
                "combine-conflicting-works: {} works, {} conflicting files",
                work_ids.len(),
                conflicting_files.len()
            );

            let resp = ctx.bridge.request(
                "work.combine_conflicting",
                serde_json::json!({
                    "work-ids": work_ids,
                    "conflicting_files": conflicting_files,
                }),
            );
            if let Some(err) = &resp.error {
                eyre::bail!("combine-conflicting-works failed: {}", err.message);
            }

            let combined_id = resp
                .result
                .as_ref()
                .and_then(|r| r["combined_work_id"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("combined-work-id".to_string(), serde_json::json!(combined_id));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "combined {} works into '{}' over {} conflicting files",
                    work_ids.len(),
                    combined_id,
                    conflicting_files.len()
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "combined-work-id".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "work-ids".to_string(),
                field_type: OutputType::StringArray,
                required: true,
            },
            InputField {
                name: "conflicting-files".to_string(),
                field_type: OutputType::StringArray,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        // Combines N works into one superseding Work record. Re-invocation creates
        // a second superseding Work; the Superseded -> * transitions on the first
        // set would reject, but the primitive still mutates state unconditionally
        // (new combined work, new event emission). Genuinely non-idempotent.
        // Resolve-structural-conflict uses it as the only action step, so "must be
        // last" is satisfied.
        Idempotency::NonIdempotent
    }
}
