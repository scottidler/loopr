use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::{debug, info};

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Registers validation commands for a phase as runtime tools.
///
/// Reads the phase's validation commands from the integrator config and
/// registers them via the tools.register IPC handler so implementers
/// in that phase can run validation.
pub struct RegisterValidationTools;

impl Primitive for RegisterValidationTools {
    fn name(&self) -> &'static str {
        "register-validation-tools"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let phase_id = params["phase-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'phase-id'"))?;

            debug!("register-validation-tools: phase_id={}", phase_id);

            // Read validation commands from integrator config
            let commands = &ctx.stores.config.integrator.validation_commands;
            if commands.is_empty() {
                info!(
                    "register-validation-tools: no validation commands configured for phase {}",
                    phase_id
                );
                return Ok(PrimitiveOutput {
                    values: HashMap::new(),
                    summary: format!("no validation commands to register for phase '{}'", phase_id),
                });
            }

            // Register each validation command as a runtime tool
            let mut registered = 0;
            for cmd in commands {
                let resp = ctx.bridge.request(
                    "tools.register",
                    serde_json::json!({
                        "name": format!("validate-{}", registered),
                        "command": cmd,
                        "phase-id": phase_id,
                    }),
                );
                if resp.is_error() {
                    debug!(
                        "register-validation-tools: failed to register '{}': {:?}",
                        cmd, resp.error
                    );
                } else {
                    registered += 1;
                }
            }

            info!(
                "register-validation-tools: registered {}/{} tools for phase {}",
                registered,
                commands.len(),
                phase_id
            );

            let mut values = HashMap::new();
            values.insert("registered-count".to_string(), serde_json::json!(registered));

            Ok(PrimitiveOutput {
                values,
                summary: format!("registered {} validation tools for phase '{}'", registered, phase_id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "registered-count".to_string(),
            field_type: OutputType::U32,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "phase-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}
