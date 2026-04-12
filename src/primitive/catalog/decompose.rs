use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Binary classifier: determines brief vs full decomposition path.
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

/// Breaks a parent document into children via LLM call.
pub struct Decompose;

impl Primitive for Decompose {
    fn name(&self) -> &'static str {
        "decompose"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("decompose: parent-id={}", params["parent-id"]);

            let resp = ctx.bridge.request("decomposer.decompose", params);
            if let Some(err) = &resp.error {
                eyre::bail!("decompose failed: {}", err.message);
            }

            let children = resp
                .result
                .as_ref()
                .and_then(|r| r["children"].clone().into())
                .unwrap_or(serde_json::json!([]));

            let mut values = HashMap::new();
            values.insert("children".to_string(), children);

            Ok(PrimitiveOutput {
                values,
                summary: "decomposition complete".to_string(),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "children".to_string(),
            field_type: OutputType::Json,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "parent-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "parent-collection".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "target-kind".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "prompt".to_string(),
                field_type: OutputType::String,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}

/// Validates a document against its type's schema/template via LLM.
pub struct ValidateDocument;

impl Primitive for ValidateDocument {
    fn name(&self) -> &'static str {
        "validate-document"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("validate-document: {}/{}", params["collection"], params["id"]);

            let resp = ctx.bridge.request("validator.validate", params);
            if let Some(err) = &resp.error {
                eyre::bail!("validate-document failed: {}", err.message);
            }

            let result = resp.result.unwrap_or(serde_json::json!({}));
            let verdict = result["verdict"].as_str().unwrap_or("unknown").to_string();

            let mut values = HashMap::new();
            values.insert("verdict".to_string(), serde_json::json!(verdict));
            values.insert("issues".to_string(), result["issues"].clone());
            values.insert("summary".to_string(), result["summary"].clone());

            Ok(PrimitiveOutput {
                values,
                summary: format!("validation: {}", verdict),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "verdict".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "issues".to_string(),
                field_type: OutputType::Json,
            },
            OutputField {
                name: "summary".to_string(),
                field_type: OutputType::String,
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
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Checks whether children adequately cover parent requirements via LLM.
pub struct EvaluateCoverage;

impl Primitive for EvaluateCoverage {
    fn name(&self) -> &'static str {
        "evaluate-coverage"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!(
                "evaluate-coverage: {}/{}",
                params["parent-collection"], params["parent-id"]
            );

            let resp = ctx.bridge.request("evaluator.evaluate_coverage", params);
            if let Some(err) = &resp.error {
                eyre::bail!("evaluate-coverage failed: {}", err.message);
            }

            let result = resp.result.unwrap_or(serde_json::json!({}));
            let verdict = result["verdict"].as_str().unwrap_or("unknown").to_string();

            let mut values = HashMap::new();
            values.insert("verdict".to_string(), serde_json::json!(verdict));
            values.insert("gaps".to_string(), result["gaps"].clone());
            values.insert("summary".to_string(), result["summary"].clone());

            Ok(PrimitiveOutput {
                values,
                summary: format!("coverage: {}", verdict),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "verdict".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "gaps".to_string(),
                field_type: OutputType::Json,
            },
            OutputField {
                name: "summary".to_string(),
                field_type: OutputType::String,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "parent-collection".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "parent-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Bottom-up semantic validation of parent-children relationships via LLM.
pub struct RatifyHierarchy;

impl Primitive for RatifyHierarchy {
    fn name(&self) -> &'static str {
        "ratify-hierarchy"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("ratify-hierarchy: plan-id={}", params["plan-id"]);

            let resp = ctx.bridge.request("decomposer.ratify", params);
            if let Some(err) = &resp.error {
                eyre::bail!("ratify-hierarchy failed: {}", err.message);
            }

            let result = resp.result.unwrap_or(serde_json::json!({}));
            let passed = result["passed"].as_bool().unwrap_or(false);

            let mut values = HashMap::new();
            values.insert("passed".to_string(), serde_json::json!(passed));
            values.insert("issues".to_string(), result["issues"].clone());

            Ok(PrimitiveOutput {
                values,
                summary: format!("ratification: passed={}", passed),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "passed".to_string(),
                field_type: OutputType::Bool,
            },
            OutputField {
                name: "issues".to_string(),
                field_type: OutputType::Json,
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

/// Abandons all non-terminal children of a parent.
pub struct AbandonChildren;

impl Primitive for AbandonChildren {
    fn name(&self) -> &'static str {
        "abandon-children"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("abandon-children: parent-id={}", params["parent-id"]);

            let resp = ctx.bridge.request("decomposer.abandon_children", params);
            if let Some(err) = &resp.error {
                eyre::bail!("abandon-children failed: {}", err.message);
            }

            let result = resp.result.unwrap_or(serde_json::json!({}));
            let abandoned = result["abandoned_count"].as_u64().unwrap_or(0) as u32;
            let preserved = result["preserved_count"].as_u64().unwrap_or(0) as u32;

            let mut values = HashMap::new();
            values.insert("abandoned-count".to_string(), serde_json::json!(abandoned));
            values.insert("preserved-count".to_string(), serde_json::json!(preserved));

            Ok(PrimitiveOutput {
                values,
                summary: format!("abandoned {} children, preserved {}", abandoned, preserved),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "abandoned-count".to_string(),
                field_type: OutputType::U32,
            },
            OutputField {
                name: "preserved-count".to_string(),
                field_type: OutputType::U32,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "parent-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "parent-collection".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "preserve-ids".to_string(),
                field_type: OutputType::StringArray,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Re-decomposes a parent after new knowledge invalidates existing decomposition.
pub struct ReDecompose;

impl Primitive for ReDecompose {
    fn name(&self) -> &'static str {
        "re-decompose"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("re-decompose: parent-id={}", params["parent-id"]);

            let resp = ctx.bridge.request("decomposer.re_decompose", params);
            if let Some(err) = &resp.error {
                eyre::bail!("re-decompose failed: {}", err.message);
            }

            let result = resp.result.unwrap_or(serde_json::json!({}));
            let children = result["children"].clone();
            let abandoned = result["abandoned_count"].as_u64().unwrap_or(0) as u32;

            let mut values = HashMap::new();
            values.insert("children".to_string(), children);
            values.insert("abandoned-count".to_string(), serde_json::json!(abandoned));

            Ok(PrimitiveOutput {
                values,
                summary: format!("re-decomposed, abandoned {} children", abandoned),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "children".to_string(),
                field_type: OutputType::Json,
            },
            OutputField {
                name: "abandoned-count".to_string(),
                field_type: OutputType::U32,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "parent-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "parent-collection".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "target-kind".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "reason".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "preserve-ids".to_string(),
                field_type: OutputType::StringArray,
                required: false,
            },
            InputField {
                name: "prompt".to_string(),
                field_type: OutputType::String,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}
