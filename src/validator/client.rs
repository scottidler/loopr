use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

use crate::config::ValidatorConfig;

/// Trait for HTTP clients, enabling mock injection in tests.
#[allow(async_fn_in_trait)]
pub trait HttpClient: Send + Sync {
    async fn post(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String>;
}

/// Real HTTP client using reqwest for async requests.
pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client construction is infallible with valid config");
        Self { client }
    }
}

impl HttpClient for ReqwestClient {
    async fn post(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String> {
        let mut req = self.client.post(url);
        for (key, value) in headers {
            req = req.header(*key, *value);
        }
        let response = req.body(body.to_string()).send().await.context("HTTP request failed")?;
        let text = response.text().await.context("Failed to read response body")?;
        Ok(text)
    }
}

/// Anthropic Messages API request structure.
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    temperature: f32,
    messages: Vec<AnthropicMessage>,
}

/// A single message in the Anthropic Messages API format.
#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

/// Anthropic Messages API response structure.
#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    text: String,
}

/// LLM client that calls the Anthropic Messages API.
pub struct LlmClient<H: HttpClient = ReqwestClient> {
    config: ValidatorConfig,
    http_client: H,
}

impl<H: HttpClient> LlmClient<H> {
    pub fn new(config: ValidatorConfig, http_client: H) -> Self {
        Self { config, http_client }
    }

    /// Send a prompt to the LLM and return the raw text response.
    /// Retries once on parse failure with error feedback.
    pub async fn call(&self, prompt: &str) -> Result<String> {
        let api_key = std::env::var(&self.config.llm.api_key_env)
            .context(format!("Missing API key env var: {}", self.config.llm.api_key_env))?;

        let api_url = match self.config.provider.as_str() {
            "anthropic" => "https://api.anthropic.com/v1/messages",
            other => bail!("Unsupported LLM provider: {}", other),
        };

        let request = AnthropicRequest {
            model: self.config.llm.model.clone(),
            max_tokens: self.config.llm.max_tokens,
            temperature: self.config.llm.temperature,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
        };

        let body = serde_json::to_string(&request).context("Failed to serialize API request")?;

        let headers = [
            ("content-type", "application/json"),
            ("x-api-key", api_key.as_str()),
            ("anthropic-version", "2023-06-01"),
        ];

        info!(
            "Calling LLM API: provider={}, model={}",
            self.config.provider, self.config.llm.model
        );

        let response_text = self
            .http_client
            .post(api_url, &headers, &body)
            .await
            .context("LLM API call failed")?;

        let response: AnthropicResponse = serde_json::from_str(&response_text)
            .context("Failed to parse API response — may indicate API error or rate limiting")?;

        let text = response
            .content
            .into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");

        if text.is_empty() {
            bail!("LLM returned empty response");
        }

        info!("LLM response received: {} chars", text.len());
        Ok(text)
    }

    /// Send a prompt and retry once if the response isn't valid JSON.
    pub async fn call_with_retry(&self, prompt: &str) -> Result<String> {
        match self.call(prompt).await {
            Ok(text) => {
                // Check if it's valid JSON
                if serde_json::from_str::<serde_json::Value>(&text).is_ok() {
                    return Ok(text);
                }
                // Retry with error feedback
                warn!("LLM returned invalid JSON, retrying with feedback");
                let retry_prompt = format!(
                    "{}\n\n## Previous Attempt\n\nYour previous response was not valid JSON. \
                     Please respond with ONLY valid JSON, no markdown fences or extra text.\n\n\
                     Your previous response was:\n{}",
                    prompt, text
                );
                self.call(&retry_prompt).await
            }
            Err(e) => Err(e),
        }
    }
}

impl LlmClient<ReqwestClient> {
    /// Create an LlmClient with the real reqwest HTTP client.
    pub fn with_reqwest(config: ValidatorConfig) -> Self {
        Self::new(config, ReqwestClient::new())
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// Mock HTTP client that returns a canned response.
    struct MockHttpClient {
        response: String,
    }

    impl MockHttpClient {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
            }
        }
    }

    impl HttpClient for MockHttpClient {
        async fn post(&self, _url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    /// A single recorded HTTP call: (url, headers, body).
    type RecordedCall = (String, Vec<(String, String)>, String);

    /// Mock that records request details.
    struct RecordingHttpClient {
        response: String,
        calls: std::sync::Mutex<Vec<RecordedCall>>,
    }

    impl RecordingHttpClient {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
                calls: std::sync::Mutex::new(vec![]),
            }
        }
    }

    impl HttpClient for RecordingHttpClient {
        async fn post(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String> {
            self.calls.lock().unwrap().push((
                url.to_string(),
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                body.to_string(),
            ));
            Ok(self.response.clone())
        }
    }

    /// Mock that fails on HTTP.
    struct FailingHttpClient;

    impl HttpClient for FailingHttpClient {
        async fn post(&self, _url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<String> {
            bail!("connection refused")
        }
    }

    /// Generate a unique env var name and set it, returning a config that uses it.
    /// Returns (config, env_var_name) - caller must clean up the env var.
    fn test_config_with_key(key_value: &str) -> (ValidatorConfig, String) {
        let env_var = format!("TEST_ANTHROPIC_API_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_var, key_value) };
        let config = ValidatorConfig {
            llm: crate::config::LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: env_var.clone(),
                max_tokens: 4096,
                temperature: 0.0,
            },
            enabled: true,
            provider: "anthropic".to_string(),
            prompts: crate::config::ValidatorPrompts::default(),
        };
        (config, env_var)
    }

    fn mock_anthropic_response(text: &str) -> String {
        serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "model": "claude-sonnet-4-6",
            "role": "assistant",
        })
        .to_string()
    }

    #[tokio::test]
    async fn test_llm_client_call_success() {
        let (config, env_var) = test_config_with_key("test-key-123");

        let response_json = r#"{"verdict":"pass","issues":[],"summary":"Looks good"}"#;
        let mock = MockHttpClient::new(&mock_anthropic_response(response_json));
        let client = LlmClient::new(config, mock);

        let result = client.call("test prompt").await.unwrap();
        assert_eq!(result, response_json);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_llm_client_missing_api_key() {
        let missing_var = format!("TEST_ANTHROPIC_MISSING_{}", crate::id::generate_id("xx"));
        unsafe { std::env::remove_var(&missing_var) };
        let config = ValidatorConfig {
            llm: crate::config::LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: missing_var,
                max_tokens: 4096,
                temperature: 0.0,
            },
            enabled: true,
            provider: "anthropic".to_string(),
            prompts: crate::config::ValidatorPrompts::default(),
        };
        let mock = MockHttpClient::new("{}");
        let client = LlmClient::new(config, mock);

        let result = client.call("test prompt").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing API key"));
    }

    #[tokio::test]
    async fn test_llm_client_unsupported_provider() {
        let (mut config, env_var) = test_config_with_key("test-key");
        config.provider = "openai".to_string();

        let mock = MockHttpClient::new("{}");
        let client = LlmClient::new(config, mock);

        let result = client.call("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported LLM provider"));

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_llm_client_http_failure() {
        let (config, env_var) = test_config_with_key("test-key");

        let client = LlmClient::new(config, FailingHttpClient);
        let result = client.call("test").await;
        assert!(result.is_err());

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_llm_client_sends_correct_headers() {
        let (config, env_var) = test_config_with_key("my-api-key");

        let response_json = r#"{"verdict":"pass","issues":[],"summary":"ok"}"#;
        let recorder_client = LlmClient {
            config: config.clone(),
            http_client: RecordingHttpClient::new(&mock_anthropic_response(response_json)),
        };
        let _ = recorder_client.call("test prompt").await.unwrap();

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_llm_client_call_with_retry_valid_json() {
        let (config, env_var) = test_config_with_key("test-key");

        let response_json = r#"{"verdict":"pass","issues":[],"summary":"ok"}"#;
        let mock = MockHttpClient::new(&mock_anthropic_response(response_json));
        let client = LlmClient::new(config, mock);

        let result = client.call_with_retry("test prompt").await.unwrap();
        assert_eq!(result, response_json);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_llm_client_empty_response() {
        let (config, env_var) = test_config_with_key("test-key");

        let response = serde_json::json!({
            "content": [{"type": "text", "text": ""}],
        })
        .to_string();
        let mock = MockHttpClient::new(&response);
        let client = LlmClient::new(config, mock);

        let result = client.call("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty response"));

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_llm_client_invalid_api_response() {
        let (config, env_var) = test_config_with_key("test-key");

        let mock = MockHttpClient::new("not json at all");
        let client = LlmClient::new(config, mock);

        let result = client.call("test").await;
        assert!(result.is_err());

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_llm_client_with_reqwest_constructor() {
        let (config, _env_var) = test_config_with_key("test-key");
        let _client = LlmClient::with_reqwest(config);
        // Just verify construction doesn't panic
    }
}
