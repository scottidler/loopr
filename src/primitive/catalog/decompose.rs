use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Binary classifier: determines brief vs full decomposition path.
/// Pure classification, no side effects.
pub struct ClassifyTier;

impl Primitive for ClassifyTier {
    fn name(&self) -> &'static str {
        "classify-tier"
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

            debug!("classify-tier: plan-id={}", plan_id);

            let plans = ctx.stores.read_plans()?;
            let plan = plans
                .get(plan_id)
                .ok_or_else(|| eyre::eyre!("plan '{}' not found", plan_id))?;

            let tier = format!("{:?}", plan.tier).to_lowercase();

            let mut values = HashMap::new();
            values.insert("tier".to_string(), serde_json::json!(tier));

            Ok(PrimitiveOutput {
                values,
                summary: format!("plan {} tier: {}", plan_id, tier),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "tier".to_string(),
            field_type: OutputType::String,
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
