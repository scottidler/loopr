//! Integration tests for `AnthropicClient::complete_free` via
//! wiremock. Exercises the text-block extraction path and the
//! request-body shape (no `tools`, no `tool_choice`).

#![allow(clippy::unwrap_used)]

use llm::{AnthropicClient, FatalReason, LlmClient, LlmConfig, LlmError, Message};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_config(base_url: String) -> LlmConfig {
    LlmConfig {
        model: "claude-sonnet-4-6".into(),
        max_tokens: 8192,
        temperature: Some(0.3),
        api_key_env: "ANTHROPIC_API_KEY".into(),
        api_base_url: base_url,
    }
}

fn text_response(text: &str) -> Value {
    json!({
        "id": "msg_01ABC",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20260115",
        "content": [{
            "type": "text",
            "text": text,
        }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 100, "output_tokens": 42 }
    })
}

#[tokio::test]
async fn free_happy_path_returns_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("hello world")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let messages = vec![Message::user("say hi")];
    let (result, _usage) = client.complete_free("sys", &messages, None).await.unwrap();

    assert_eq!(result, "hello world");
}

#[tokio::test]
async fn free_request_body_has_no_tools_or_tool_choice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("ok")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let messages = vec![
        Message::user("first"),
        Message::assistant("intermediate"),
        Message::user("last"),
    ];
    client.complete_free("S", &messages, None).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    // Phase 4: `system` is now an array of content blocks carrying
    // `cache_control`, not a bare string. Assert the new shape.
    let system = body["system"].as_array().expect("system must be an array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "S");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert!(body.get("tools").is_none(), "free request must not carry tools");
    assert!(
        body.get("tool_choice").is_none(),
        "free request must not carry tool_choice"
    );
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "first");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "intermediate");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"], "last");
}

#[tokio::test]
async fn free_picks_first_text_block_skipping_thinking() {
    // The design-doc invariant: thinking blocks are discarded; the
    // first `{"type": "text"}` block wins.
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            { "type": "thinking", "thinking": "reasoning text" },
            { "type": "text", "text": "the answer" },
            { "type": "text", "text": "second text block" },
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 20 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let msgs = vec![Message::user("q")];
    let (out, _usage) = client.complete_free("s", &msgs, None).await.unwrap();
    assert_eq!(out, "the answer", "first text block wins, thinking skipped");
}

#[tokio::test]
async fn free_no_text_block_maps_to_schema_validation() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            { "type": "thinking", "thinking": "only thinking" },
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client
        .complete_free("s", &[Message::user("q")], None)
        .await
        .unwrap_err();
    match err {
        LlmError::Fatal {
            reason: FatalReason::SchemaValidation { message: msg, .. },
        } => assert!(msg.contains("no text content block"), "got: {msg}"),
        other => panic!("expected SchemaValidation, got {other:?}"),
    }
}

#[tokio::test]
async fn free_max_tokens_maps_to_context_exhausted() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{ "type": "text", "text": "partial..." }],
        "stop_reason": "max_tokens",
        "usage": { "input_tokens": 100, "output_tokens": 8192 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client
        .complete_free("s", &[Message::user("q")], None)
        .await
        .unwrap_err();
    match err {
        LlmError::Fatal {
            reason: FatalReason::ContextExhausted { used, limit, usage },
        } => {
            assert_eq!(used, 8192);
            assert_eq!(limit, 8192);
            // Phase 1 remediation: the billed usage rides on the error so
            // the metered client can charge this max_tokens failure.
            assert_eq!(usage.output_tokens, 8192);
            assert_eq!(usage.input_tokens, 100);
        }
        other => panic!("expected ContextExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn free_http_500_maps_to_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream dead"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client
        .complete_free("s", &[Message::user("q")], None)
        .await
        .unwrap_err();
    match err {
        LlmError::Retryable {
            reason: llm::RetryableReason::ServerError { status },
        } => assert_eq!(status, 500),
        other => panic!("expected Retryable::ServerError(500), got {other:?}"),
    }
}

#[tokio::test]
async fn free_usage_carries_concrete_response_model() {
    // Phase 6 finding 1 (model pinning): the response's top-level `model`
    // (the concrete dated id the provider actually ran) must land on
    // `Usage.model`, even when the request asked for the floating tag.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("hi")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let (_out, usage) = client
        .complete_free("s", &[Message::user("q")], None)
        .await
        .unwrap();
    assert_eq!(
        usage.model.as_deref(),
        Some("claude-sonnet-4-6-20260115"),
        "the concrete dated model id must ride on Usage.model"
    );
}

#[tokio::test]
async fn free_request_body_carries_model_override_when_some() {
    // Phase 0 of director-phase-1: complete_free's `model: Option<&str>`
    // parameter must override the configured default model when `Some`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("ok")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let messages = vec![Message::user("q")];
    client
        .complete_free("s", &messages, Some("claude-opus-4-7"))
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        body["model"], "claude-opus-4-7",
        "complete_free with Some(model) must send that model in the request body, not the configured default"
    );
}

#[tokio::test]
async fn free_request_body_uses_config_model_when_none() {
    // Counterpart to free_request_body_carries_model_override_when_some:
    // confirm None still uses the configured default (Sonnet here).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("ok")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let messages = vec![Message::user("q")];
    client.complete_free("s", &messages, None).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], "claude-sonnet-4-6");
}

#[tokio::test]
async fn free_http_429_maps_to_rate_limited_with_retry_after() {
    // Phase 6 finding 5: 429 becomes a typed RateLimited carrying the
    // parsed retry-after header (seconds).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "12")
                .set_body_string("slow down"),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client
        .complete_free("s", &[Message::user("q")], None)
        .await
        .unwrap_err();
    match err {
        LlmError::Retryable {
            reason: llm::RetryableReason::RateLimited { retry_after },
        } => assert_eq!(retry_after, Some(12)),
        other => panic!("expected Retryable::RateLimited(Some(12)), got {other:?}"),
    }
}

#[tokio::test]
async fn free_http_529_maps_to_overloaded() {
    // Phase 6 finding 5: 529 (Anthropic overloaded_error) is its own
    // typed reason, distinct from a generic 5xx.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client
        .complete_free("s", &[Message::user("q")], None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            LlmError::Retryable {
                reason: llm::RetryableReason::Overloaded
            }
        ),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn free_http_401_maps_to_fatal_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid key"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client
        .complete_free("s", &[Message::user("q")], None)
        .await
        .unwrap_err();
    match err {
        LlmError::Fatal {
            reason: FatalReason::Auth(_),
        } => {}
        other => panic!("expected Fatal::Auth, got {other:?}"),
    }
}
