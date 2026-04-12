use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::ipc::protocol::DaemonEvent;
use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Emits an arbitrary event to the daemon's event bus.
pub struct EmitEvent;

impl Primitive for EmitEvent {
    fn name(&self) -> &'static str {
        "emit-event"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let event_type = params["event-type"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'event-type'"))?;
            let payload = params.get("payload").cloned().unwrap_or(serde_json::json!({}));

            debug!("emit-event: type={}", event_type);

            let event = DaemonEvent::new(event_type.to_string(), payload);
            let _ = ctx.event_tx.send(event);

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("emitted event '{}'", event_type),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "event-type".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "payload".to_string(),
                field_type: OutputType::Json,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}
