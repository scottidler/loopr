use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Starts a new agent session for a given role and target.
pub struct SpawnAgent;

impl Primitive for SpawnAgent {
    fn name(&self) -> &'static str {
        "spawn-agent"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            debug!("spawn-agent: role={} target-id={}", params["role"], params["target-id"]);

            let resp = ctx.bridge.request("agent.start", params);
            if let Some(err) = &resp.error {
                eyre::bail!("spawn-agent failed: {}", err.message);
            }

            let session_id = resp
                .result
                .as_ref()
                .and_then(|r| r["session_id"].as_str())
                .unwrap_or("")
                .to_string();

            let mut values = HashMap::new();
            values.insert("session-id".to_string(), serde_json::json!(session_id));

            Ok(PrimitiveOutput {
                values,
                summary: format!("spawned agent session '{}'", session_id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "session-id".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "role".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "target-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "model".to_string(),
                field_type: OutputType::String,
                required: false,
            },
            InputField {
                name: "context-from".to_string(),
                field_type: OutputType::String,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}

/// Cancels a running agent session.
pub struct StopAgent;

impl Primitive for StopAgent {
    fn name(&self) -> &'static str {
        "stop-agent"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = params["session-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'session-id'"))?;

            debug!("stop-agent: session-id={}", session_id);

            let resp = ctx
                .bridge
                .request("agent.stop", serde_json::json!({"session_id": session_id}));
            if let Some(err) = &resp.error {
                eyre::bail!("stop-agent failed: {}", err.message);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("stopped agent '{}'", session_id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "session-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Pauses a running agent session.
pub struct PauseAgent;

impl Primitive for PauseAgent {
    fn name(&self) -> &'static str {
        "pause-agent"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = params["session-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'session-id'"))?;

            debug!("pause-agent: session-id={}", session_id);

            let resp = ctx
                .bridge
                .request("agent.pause", serde_json::json!({"session_id": session_id}));
            if let Some(err) = &resp.error {
                eyre::bail!("pause-agent failed: {}", err.message);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("paused agent '{}'", session_id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "session-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Resumes a paused agent session.
pub struct ResumeAgent;

impl Primitive for ResumeAgent {
    fn name(&self) -> &'static str {
        "resume-agent"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = params["session-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'session-id'"))?;

            debug!("resume-agent: session-id={}", session_id);

            let resp = ctx
                .bridge
                .request("agent.resume", serde_json::json!({"session_id": session_id}));
            if let Some(err) = &resp.error {
                eyre::bail!("resume-agent failed: {}", err.message);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("resumed agent '{}'", session_id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "session-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Injects additional context into a running agent's session.
pub struct InjectContext;

impl Primitive for InjectContext {
    fn name(&self) -> &'static str {
        "inject-context"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let session_id = params["session-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'session-id'"))?
                .to_string();
            let content_len = params["content"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'content'"))?
                .len();

            debug!("inject-context: session-id={} len={}", session_id, content_len);

            let resp = ctx.bridge.request("agent.inject_context", params);
            if let Some(err) = &resp.error {
                eyre::bail!("inject-context failed: {}", err.message);
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("injected {} chars into session '{}'", content_len, session_id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "session-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "content".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "source".to_string(),
                field_type: OutputType::String,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }
}
