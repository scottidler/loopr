use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Creates a new record of any domain type via bridge dispatch.
pub struct CreateRecord;

impl Primitive for CreateRecord {
    fn name(&self) -> &'static str {
        "create-record"
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
            let fields = params.get("fields").cloned().unwrap_or(serde_json::json!({}));

            debug!("create-record: collection={}", collection);

            let method = format!("{}.create", collection);
            let resp = ctx.bridge.request(&method, fields);

            if let Some(err) = &resp.error {
                eyre::bail!("create-record failed: {}", err.message);
            }

            let id = resp
                .result
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("id".to_string(), serde_json::json!(id));

            Ok(PrimitiveOutput {
                values,
                summary: format!("created {} '{}'", collection, id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "id".to_string(),
            field_type: OutputType::String,
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
                name: "fields".to_string(),
                field_type: OutputType::Json,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }
}

/// Updates fields on an existing record via bridge dispatch.
pub struct UpdateRecord;

impl Primitive for UpdateRecord {
    fn name(&self) -> &'static str {
        "update-record"
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
            let fields = params.get("fields").cloned().unwrap_or(serde_json::json!({}));

            debug!("update-record: collection={} id={}", collection, id);

            let method = format!("{}.update", collection);
            let mut update_params = fields;
            if let Some(obj) = update_params.as_object_mut() {
                obj.insert("id".to_string(), serde_json::json!(id));
            }
            let resp = ctx.bridge.request(&method, update_params);

            if let Some(err) = &resp.error {
                eyre::bail!("update-record failed: {}", err.message);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("updated {} '{}'", collection, id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
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
                name: "fields".to_string(),
                field_type: OutputType::Json,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Generic FSM transition for any domain type via bridge dispatch.
pub struct TransitionRecord;

impl Primitive for TransitionRecord {
    fn name(&self) -> &'static str {
        "transition-record"
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
            let target_status = params["target-status"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'target-status'"))?;
            let role = params["role"].as_str().ok_or_else(|| eyre::eyre!("missing 'role'"))?;

            debug!(
                "transition-record: {}/{} -> {} (role={})",
                collection, id, target_status, role
            );

            let method = format!("{}.transition", collection);
            let resp = ctx.bridge.request(
                &method,
                serde_json::json!({
                    "id": id,
                    "target_status": target_status,
                    "role": role,
                }),
            );

            if let Some(err) = &resp.error {
                eyre::bail!("transition-record failed: {}", err.message);
            }

            let from_status = resp
                .result
                .as_ref()
                .and_then(|r| r["from_status"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("from-status".to_string(), serde_json::json!(from_status));
            values.insert("to-status".to_string(), serde_json::json!(target_status));

            Ok(PrimitiveOutput {
                values,
                summary: format!("{} '{}': {} -> {}", collection, id, from_status, target_status),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "from-status".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "to-status".to_string(),
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
            InputField {
                name: "target-status".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "role".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Creates a new Work record. Domain-specific wrapper around create-record.
pub struct CreateWork;

impl Primitive for CreateWork {
    fn name(&self) -> &'static str {
        "create-work"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("create-work: parent-id={}", params["parent-id"]);

            let resp = ctx.bridge.request("work.create", params);

            if let Some(err) = &resp.error {
                eyre::bail!("create-work failed: {}", err.message);
            }

            let id = resp
                .result
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("work-id".to_string(), serde_json::json!(id));

            Ok(PrimitiveOutput {
                values,
                summary: format!("created work '{}'", id),
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
        vec![
            InputField {
                name: "parent-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "title".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "files".to_string(),
                field_type: OutputType::StringArray,
                required: false,
            },
            InputField {
                name: "acceptance-criteria".to_string(),
                field_type: OutputType::StringArray,
                required: false,
            },
            InputField {
                name: "dependencies".to_string(),
                field_type: OutputType::StringArray,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }
}

/// Work-specific FSM transition with dependency checks.
pub struct TransitionWork;

impl Primitive for TransitionWork {
    fn name(&self) -> &'static str {
        "transition-work"
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
            let target_status = params["target-status"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'target-status'"))?;
            let role = params["role"].as_str().ok_or_else(|| eyre::eyre!("missing 'role'"))?;

            debug!("transition-work: {} -> {} (role={})", work_id, target_status, role);

            let resp = ctx.bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": work_id,
                    "target_status": target_status,
                    "role": role,
                }),
            );

            if let Some(err) = &resp.error {
                eyre::bail!("transition-work failed: {}", err.message);
            }

            let from_status = resp
                .result
                .as_ref()
                .and_then(|r| r["from_status"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("from-status".to_string(), serde_json::json!(from_status));
            values.insert("to-status".to_string(), serde_json::json!(target_status));

            Ok(PrimitiveOutput {
                values,
                summary: format!("work '{}': {} -> {}", work_id, from_status, target_status),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "from-status".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "to-status".to_string(),
                field_type: OutputType::String,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "work-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "target-status".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "role".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Force-transition a Work with audit trail, bypassing normal FSM guards.
pub struct OverrideWork;

impl Primitive for OverrideWork {
    fn name(&self) -> &'static str {
        "override-work"
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
            let target_status = params["target-status"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'target-status'"))?;
            let reason = params["reason"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'reason'"))?;

            debug!("override-work: {} -> {} reason={}", work_id, target_status, reason);

            let resp = ctx.bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": work_id,
                    "target_status": target_status,
                    "role": "Coordinator",
                    "override": true,
                    "reason": reason,
                }),
            );

            if let Some(err) = &resp.error {
                eyre::bail!("override-work failed: {}", err.message);
            }

            let from_status = resp
                .result
                .as_ref()
                .and_then(|r| r["from_status"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("from-status".to_string(), serde_json::json!(from_status));
            values.insert("to-status".to_string(), serde_json::json!(target_status));

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "override work '{}': {} -> {} ({})",
                    work_id, from_status, target_status, reason
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "from-status".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "to-status".to_string(),
                field_type: OutputType::String,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "work-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "target-status".to_string(),
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

/// Creates a Bundle from an implementer's work output.
pub struct CreateBundle;

impl Primitive for CreateBundle {
    fn name(&self) -> &'static str {
        "create-bundle"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("create-bundle: work-id={}", params["work-id"]);

            let resp = ctx.bridge.request("bundle.create", params);

            if let Some(err) = &resp.error {
                eyre::bail!("create-bundle failed: {}", err.message);
            }

            let id = resp
                .result
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("bundle-id".to_string(), serde_json::json!(id));

            Ok(PrimitiveOutput {
                values,
                summary: format!("created bundle '{}'", id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "bundle-id".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "work-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "branch-name".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "description".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "claims".to_string(),
                field_type: OutputType::StringArray,
                required: true,
            },
            InputField {
                name: "head-commit".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "paths".to_string(),
                field_type: OutputType::StringArray,
                required: true,
            },
            InputField {
                name: "is-noop".to_string(),
                field_type: OutputType::Bool,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }
}

/// Creates a new Tick for bundling accepted work.
pub struct CreateTick;

impl Primitive for CreateTick {
    fn name(&self) -> &'static str {
        "create-tick"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("create-tick: plan-id={}", params["plan-id"]);

            let resp = ctx.bridge.request("tick.create", params);

            if let Some(err) = &resp.error {
                eyre::bail!("create-tick failed: {}", err.message);
            }

            let id = resp
                .result
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("tick-id".to_string(), serde_json::json!(id));

            Ok(PrimitiveOutput {
                values,
                summary: format!("created tick '{}'", id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "tick-id".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "plan-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "number".to_string(),
                field_type: OutputType::U32,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }
}

/// Records a learning for future context.
pub struct CreateLearning;

impl Primitive for CreateLearning {
    fn name(&self) -> &'static str {
        "create-learning"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("create-learning: source-id={}", params["source-id"]);

            let resp = ctx.bridge.request("learning.create", params);

            if let Some(err) = &resp.error {
                eyre::bail!("create-learning failed: {}", err.message);
            }

            let id = resp
                .result
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("learning-id".to_string(), serde_json::json!(id));

            Ok(PrimitiveOutput {
                values,
                summary: format!("created learning '{}'", id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "learning-id".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "content".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "scope".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "source-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }
}

/// Increments bubble_up_count on the Plan that owns the given Spec.
/// Used by the revise-parent-on-impossible-spec strategy to track how many
/// times children have bubbled up failures, preventing infinite re-decomposition.
pub struct IncrementBubbleUp;

impl Primitive for IncrementBubbleUp {
    fn name(&self) -> &'static str {
        "increment-bubble-up"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let spec_id = params["spec-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'spec-id'"))?;

            let plan_id = {
                let specs = ctx
                    .stores
                    .specs
                    .read()
                    .map_err(|_| eyre::eyre!("specs lock poisoned"))?;
                specs
                    .get(spec_id)
                    .map(|s| s.parent_id.clone())
                    .ok_or_else(|| eyre::eyre!("spec not found: {}", spec_id))?
            };

            let new_count = {
                let mut plans = ctx
                    .stores
                    .plans
                    .write()
                    .map_err(|_| eyre::eyre!("plans lock poisoned"))?;
                let plan = plans
                    .get_mut(&plan_id)
                    .ok_or_else(|| eyre::eyre!("plan not found: {}", plan_id))?;
                plan.bubble_up_count += 1;
                plan.updated_at = crate::id::now_millis();
                let count = plan.bubble_up_count;
                if let Some(store) = &ctx.stores.store {
                    let _ = store
                        .lock()
                        .map_err(|_| eyre::eyre!("taskstore lock poisoned"))?
                        .update(plan.clone());
                }
                count
            };

            debug!(
                "increment-bubble-up: plan {} bubble_up_count now {}",
                plan_id, new_count
            );

            let mut values = HashMap::new();
            values.insert("plan-id".to_string(), serde_json::json!(plan_id));
            values.insert("bubble-up-count".to_string(), serde_json::json!(new_count));

            Ok(PrimitiveOutput {
                values,
                summary: format!("plan '{}' bubble_up_count incremented to {}", plan_id, new_count),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "plan-id".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "bubble-up-count".to_string(),
                field_type: OutputType::U32,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "spec-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}
