use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::fsm::status::FsmStatus;
use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Rejects a bundle and cascades: resets the parent Work to Ready.
pub struct RejectBundle;

impl Primitive for RejectBundle {
    fn name(&self) -> &'static str {
        "reject-bundle"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let bundle_id = params["bundle-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'bundle-id'"))?;
            let reason = params["reason"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'reason'"))?;

            debug!("reject-bundle: bundle-id={} reason={}", bundle_id, reason);

            // Transition bundle to Rejected
            let resp = ctx.bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target-status": "Rejected",
                    "role": "Coordinator",
                    "reason": reason,
                }),
            );
            if let Some(err) = &resp.error {
                eyre::bail!("reject-bundle transition failed: {}", err.message);
            }

            // Get the work_id from the bundle to reset the parent work
            let bundles = ctx.stores.read_bundles()?;
            if let Some(bundle) = bundles.get(bundle_id) {
                let work_id = bundle.work_id.clone();
                drop(bundles);
                // Reset parent work to Ready
                let resp = ctx.bridge.request(
                    "work.transition",
                    serde_json::json!({
                        "id": work_id,
                        "target-status": "Ready",
                        "role": "Coordinator",
                        "override": true,
                        "reason": format!("bundle {} rejected: {}", bundle_id, reason),
                    }),
                );
                if let Some(err) = &resp.error {
                    eyre::bail!("reject-bundle work reset failed: {}", err.message);
                }
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("rejected bundle '{}': {}", bundle_id, reason),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "bundle-id".to_string(),
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

/// Marks all non-terminal bundles for a work as Superseded.
pub struct SupersedeBundles;

impl Primitive for SupersedeBundles {
    fn name(&self) -> &'static str {
        "supersede-bundles"
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
            let except_id = params["except-bundle-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'except-bundle-id'"))?;

            debug!("supersede-bundles: work-id={} except={}", work_id, except_id);

            // Find non-terminal bundles for this work
            let to_supersede: Vec<String> = {
                let bundles = ctx.stores.read_bundles()?;
                bundles
                    .values()
                    .filter(|b| b.work_id == work_id && b.id != except_id && !b.status().is_terminal(&ctx.stores.fsm))
                    .map(|b| b.id.clone())
                    .collect()
            };

            let count = to_supersede.len() as u32;
            for bid in &to_supersede {
                let resp = ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": bid,
                        "target-status": "Superseded",
                        "role": "Coordinator",
                    }),
                );
                if let Some(err) = &resp.error {
                    eyre::bail!("supersede-bundles failed for {}: {}", bid, err.message);
                }
            }

            let mut values = HashMap::new();
            values.insert("count".to_string(), serde_json::json!(count));

            Ok(PrimitiveOutput {
                values,
                summary: format!("superseded {} bundles for work '{}'", count, work_id),
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
                name: "work-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "except-bundle-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}
