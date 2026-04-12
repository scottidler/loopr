use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::agents::context::{estimate_tokens, select_learnings, truncate_from_head, truncate_prose};
use crate::domain::learning::LearningScope;
use crate::domain::role::Role;
use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Filters and ranks learnings for a given scope and role. Pure query.
pub struct SelectLearnings;

impl Primitive for SelectLearnings {
    fn name(&self) -> &'static str {
        "select-learnings"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let role_str = params["role"].as_str().ok_or_else(|| eyre::eyre!("missing 'role'"))?;
            let role: Role = serde_json::from_value(serde_json::json!(role_str))
                .map_err(|e| eyre::eyre!("invalid role '{}': {}", role_str, e))?;

            let min_confidence = params.get("min-confidence").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let max_count = params.get("max-count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

            let scope_ids_raw = params["scope-ids"]
                .as_array()
                .ok_or_else(|| eyre::eyre!("missing 'scope-ids'"))?;

            debug!("select-learnings: role={} scopes={}", role_str, scope_ids_raw.len());

            let scope_pairs: Vec<(String, LearningScope)> = scope_ids_raw
                .iter()
                .filter_map(|entry| {
                    let id = entry["id"].as_str()?.to_string();
                    let scope: LearningScope = serde_json::from_value(entry["scope"].clone()).ok()?;
                    Some((id, scope))
                })
                .collect();

            let scope_refs: Vec<(&str, LearningScope)> =
                scope_pairs.iter().map(|(id, scope)| (id.as_str(), *scope)).collect();

            let learnings_map = ctx.stores.read_learnings()?;
            let selected = select_learnings(&learnings_map, &scope_refs, role, min_confidence, max_count);

            let learnings_json: Vec<serde_json::Value> =
                selected.iter().filter_map(|l| serde_json::to_value(l).ok()).collect();

            let count = learnings_json.len();
            let mut values = HashMap::new();
            values.insert("learnings".to_string(), serde_json::Value::Array(learnings_json));

            Ok(PrimitiveOutput {
                values,
                summary: format!("selected {} learnings for {}", count, role_str),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "learnings".to_string(),
            field_type: OutputType::Json,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "scope-ids".to_string(),
                field_type: OutputType::Json,
                required: true,
            },
            InputField {
                name: "role".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "min-confidence".to_string(),
                field_type: OutputType::F64,
                required: false,
            },
            InputField {
                name: "max-count".to_string(),
                field_type: OutputType::U64,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Truncates context sections to fit within token budget. Pure transform.
pub struct CompactContext;

impl Primitive for CompactContext {
    fn name(&self) -> &'static str {
        "compact-context"
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let text = params["text"].as_str().ok_or_else(|| eyre::eyre!("missing 'text'"))?;
            let max_tokens = params["max-tokens"]
                .as_u64()
                .ok_or_else(|| eyre::eyre!("missing 'max-tokens'"))? as usize;
            let strategy = params.get("strategy").and_then(|v| v.as_str()).unwrap_or("prose");

            debug!(
                "compact-context: strategy={} max-tokens={} input-tokens={}",
                strategy,
                max_tokens,
                estimate_tokens(text)
            );

            let original_tokens = estimate_tokens(text);
            let truncated = match strategy {
                "head" => truncate_from_head(text, max_tokens),
                "tail" | "prose" => truncate_prose(text, max_tokens),
                other => {
                    eyre::bail!("unknown strategy: '{}'", other)
                }
            };
            let was_truncated = truncated.len() < text.len();

            let mut values = HashMap::new();
            values.insert("truncated".to_string(), serde_json::json!(truncated));
            values.insert("was-truncated".to_string(), serde_json::json!(was_truncated));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "compact: {} -> {} tokens (truncated={})",
                    original_tokens,
                    estimate_tokens(&truncated),
                    was_truncated
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "truncated".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "was-truncated".to_string(),
                field_type: OutputType::Bool,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "text".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "max-tokens".to_string(),
                field_type: OutputType::U64,
                required: true,
            },
            InputField {
                name: "strategy".to_string(),
                field_type: OutputType::String,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}
