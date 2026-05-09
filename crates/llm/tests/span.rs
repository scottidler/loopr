//! Span-snapshot test for `llm.anthropic`. Pins the telemetry
//! payload rules locked by Architect round 1 + design doc Security
//! section: the span MUST emit model, prompt lengths, prompt
//! previews (truncated to 4 KiB), duration, outcome. It MUST NOT
//! emit the tool schema, `ToolCall.input`, the API key, or any
//! header values.
//!
//! Implementation: a minimal `tracing_subscriber::Layer` records
//! span metadata and field names into an `Arc<Mutex<_>>` the test
//! inspects after the call completes.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex, OnceLock};

use llm::{AnthropicClient, LlmClient, LlmConfig, ToolSchema};
use serde_json::json;
use tracing::{
    Subscriber,
    span::{Attributes, Id, Record},
};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Per-span capture: the span's name, its initial Attributes' field
/// names, and any field names recorded later via `span.record`.
#[derive(Default, Debug, Clone)]
struct Captured {
    id: u64,
    span_name: String,
    attr_field_names: Vec<String>,
    recorded_field_names: Vec<String>,
    /// Rendered string of each initial attribute field, to verify
    /// model/preview values (and to verify the api-key string never
    /// appears anywhere).
    attr_field_renders: Vec<(String, String)>,
}

#[derive(Default)]
struct CaptureLayer {
    spans: Arc<Mutex<Vec<Captured>>>,
}

impl<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>> Layer<S> for CaptureLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut cap = Captured {
            id: id.into_u64(),
            span_name: attrs.metadata().name().to_string(),
            attr_field_names: attrs.fields().iter().map(|f| f.name().to_string()).collect(),
            recorded_field_names: Vec::new(),
            attr_field_renders: Vec::new(),
        };
        struct Visitor<'a>(&'a mut Vec<(String, String)>);
        impl<'a> tracing::field::Visit for Visitor<'a> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push((field.name().to_string(), format!("{value:?}")));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push((field.name().to_string(), value.to_string()));
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.push((field.name().to_string(), value.to_string()));
            }
            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.0.push((field.name().to_string(), value.to_string()));
            }
        }
        let mut v = Visitor(&mut cap.attr_field_renders);
        attrs.record(&mut v);
        self.spans.lock().unwrap().push(cap);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let id_u64 = id.into_u64();
        let mut guard = self.spans.lock().unwrap();
        if let Some(entry) = guard.iter_mut().find(|c| c.id == id_u64) {
            struct NameVisitor<'a>(&'a mut Vec<String>);
            impl<'a> tracing::field::Visit for NameVisitor<'a> {
                fn record_debug(&mut self, field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {
                    self.0.push(field.name().to_string());
                }
                fn record_str(&mut self, field: &tracing::field::Field, _value: &str) {
                    self.0.push(field.name().to_string());
                }
                fn record_u64(&mut self, field: &tracing::field::Field, _value: u64) {
                    self.0.push(field.name().to_string());
                }
            }
            let mut v = NameVisitor(&mut entry.recorded_field_names);
            values.record(&mut v);
        }
    }
}

static CAPTURED: OnceLock<Arc<Mutex<Vec<Captured>>>> = OnceLock::new();

fn shared_captured() -> Arc<Mutex<Vec<Captured>>> {
    CAPTURED
        .get_or_init(|| {
            let buf: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
            let layer = CaptureLayer { spans: buf.clone() };
            tracing_subscriber::registry().with(layer).init();
            buf
        })
        .clone()
}

fn test_config(base_url: String) -> LlmConfig {
    LlmConfig {
        model: "claude-sonnet-4-6".into(),
        max_tokens: 8192,
        temperature: 0.3,
        api_key_env: "ANTHROPIC_API_KEY".into(),
        api_base_url: base_url,
    }
}

#[tokio::test]
async fn span_emits_required_fields_and_no_secrets() {
    let captured = shared_captured();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6-20260115",
            "content": [{
                "type": "tool_use",
                "id": "toolu_01",
                "name": "submit_decomposition",
                "input": { "works": [] }
            }],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 100, "output_tokens": 42 }
        })))
        .mount(&server)
        .await;

    let api_key = "super-secret-key-xyz123".to_string();
    let client = AnthropicClient::new(test_config(server.uri()), api_key.clone()).unwrap();

    // Unique marker user-prompt so we can distinguish this test's
    // span from the other span test's span in the shared buffer.
    let user_marker = "USR-MARKER-span-emits-required-fields";
    let tool = ToolSchema {
        name: "submit_decomposition".into(),
        description: "SCHEMA-DESCRIPTION-MARKER-NOT-IN-LOGS".into(),
        input_schema: json!({ "marker": "INPUT-SCHEMA-MARKER-NOT-IN-LOGS" }),
    };
    client
        .complete_with_tool("short-system", user_marker, tool, None)
        .await
        .unwrap();

    let guard = captured.lock().unwrap();
    let span = guard
        .iter()
        .find(|c| {
            c.span_name == "llm.anthropic"
                && c.attr_field_renders
                    .iter()
                    .any(|(k, v)| k == "user_preview" && v == user_marker)
        })
        .expect("expected a llm.anthropic span for this test's unique user_marker");

    // Required attribute fields declared at span creation:
    for field in [
        "model",
        "system_len",
        "user_len",
        "system_preview",
        "user_preview",
        "tool_name",
    ] {
        assert!(
            span.attr_field_names.iter().any(|n| n == field),
            "missing required attr field {field}: have {:?}",
            span.attr_field_names
        );
    }

    // Fields recorded after the call completes (duration + outcome).
    for field in ["duration_ms", "outcome"] {
        assert!(
            span.recorded_field_names.iter().any(|n| n == field),
            "missing required recorded field {field}: have {:?}",
            span.recorded_field_names
        );
    }

    // MUST NOT emit: the api key or the schema description or the
    // input_schema marker or the ToolCall.input response.
    let all_rendered = span
        .attr_field_renders
        .iter()
        .map(|(_, v)| v.as_str())
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        !all_rendered.contains(&api_key),
        "api key leaked into span field: {all_rendered}"
    );
    assert!(
        !all_rendered.contains("SCHEMA-DESCRIPTION-MARKER-NOT-IN-LOGS"),
        "tool schema description leaked into span field: {all_rendered}"
    );
    assert!(
        !all_rendered.contains("INPUT-SCHEMA-MARKER-NOT-IN-LOGS"),
        "tool input schema leaked into span field: {all_rendered}"
    );

    // Must not have an unexpected field like `api_key`, `x_api_key`,
    // `input_schema`, `response_input`, `tool_input`, `schema`.
    for forbidden in [
        "api_key",
        "x_api_key",
        "input_schema",
        "response_input",
        "tool_input",
        "schema",
        "authorization",
    ] {
        assert!(
            !span.attr_field_names.iter().any(|n| n == forbidden),
            "forbidden span field `{forbidden}` was declared: {:?}",
            span.attr_field_names
        );
    }
}

#[tokio::test]
async fn long_prompt_preview_is_truncated_with_marker() {
    // Shared Captured buffer across the two span tests; we scan for
    // a span whose user_len matches the long prompt we sent to pick
    // up this test's span specifically.
    let captured = shared_captured();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6-20260115",
            "content": [{
                "type": "tool_use",
                "id": "toolu_02",
                "name": "submit_decomposition",
                "input": { "works": [] }
            }],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 100, "output_tokens": 42 }
        })))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "k".into()).unwrap();
    let long_user = "u".repeat(10_000);
    client
        .complete_with_tool(
            "s",
            &long_user,
            ToolSchema {
                name: "submit_decomposition".into(),
                description: "x".into(),
                input_schema: json!({}),
            },
            None,
        )
        .await
        .unwrap();

    let guard = captured.lock().unwrap();
    let span = guard
        .iter()
        .rev()
        .find(|c| {
            c.span_name == "llm.anthropic"
                && c.attr_field_renders
                    .iter()
                    .any(|(k, v)| k == "user_len" && v == "10000")
        })
        .expect("expected a llm.anthropic span for the long-prompt call");

    let preview = span
        .attr_field_renders
        .iter()
        .find(|(k, _)| k == "user_preview")
        .map(|(_, v)| v.as_str())
        .unwrap();

    assert!(
        preview.contains("truncated"),
        "expected truncation marker in preview, got: {preview:.200}"
    );
    // The truncated preview should be bounded (< 4200 chars covers
    // the 4096-byte cap plus the marker suffix).
    assert!(
        preview.len() < 4200,
        "preview too long ({} chars); truncation didn't kick in",
        preview.len()
    );
}
