use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};
use crate::scorer;

/// Computes a composite quality score for a completed plan. Pure computation.
pub struct ComputeScore;

impl Primitive for ComputeScore {
    fn name(&self) -> &'static str {
        "compute-score"
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let store_path = params["store-path"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'store-path'"))?;
            let duration_secs = params["duration-secs"]
                .as_u64()
                .ok_or_else(|| eyre::eyre!("missing 'duration-secs'"))?;

            debug!(
                "compute-score: store-path={} duration-secs={}",
                store_path, duration_secs
            );

            let score = scorer::compute(&PathBuf::from(store_path), duration_secs)?;

            let score_json = serde_json::to_value(&score)?;

            let mut values = HashMap::new();
            values.insert("score".to_string(), score_json);

            Ok(PrimitiveOutput {
                values,
                summary: format!("composite score: {:.2}", score.composite_score),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "score".to_string(),
            field_type: OutputType::Json,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "store-path".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "duration-secs".to_string(),
                field_type: OutputType::U64,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}
