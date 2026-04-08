pub mod client;
pub mod prompts;

use eyre::{Context, Result};
use tracing::{debug, info};

use crate::config::ValidatorConfig;
use crate::domain::validation::{IssueSeverity, ValidationIssue, ValidationReport, ValidationVerdict};

use client::{HttpClient, LlmClient, ReqwestClient};

/// Parsed LLM response before it becomes a full ValidationReport.
#[derive(Debug, serde::Deserialize)]
struct LlmValidationResponse {
    verdict: ValidationVerdict,
    issues: Vec<LlmIssue>,
    summary: String,
}

#[derive(Debug, serde::Deserialize)]
struct LlmIssue {
    severity: IssueSeverity,
    category: String,
    message: String,
    suggestion: Option<String>,
}

/// Doc Validator — runs LLM validation on Plan, Spec, and Phase documents.
///
/// Uses a trait-based HTTP client for testability. When `validator.enabled = false`
/// in config, the daemon should not construct a DocValidator at all.
pub struct DocValidator<H: HttpClient = ReqwestClient> {
    llm_client: LlmClient<H>,
    model: String,
}

impl DocValidator<ReqwestClient> {
    /// Create a DocValidator with a real reqwest HTTP client.
    pub fn new(config: ValidatorConfig) -> Self {
        debug!("DocValidator::new(model={})", config.model);
        let model = config.model.clone();
        Self {
            llm_client: LlmClient::with_reqwest(config),
            model,
        }
    }
}

impl<H: HttpClient> DocValidator<H> {
    /// Validate a Plan document.
    ///
    /// `markdown_content` is the full `docs/loopr/<id>.md` file including frontmatter.
    pub async fn validate_plan(&self, target_id: &str, markdown_content: &str) -> Result<ValidationReport> {
        debug!("DocValidator::validate_plan(target_id={})", target_id);
        let prompt = prompts::plan_prompt(markdown_content);
        self.run_validation("plans", target_id, &prompt).await
    }

    /// Validate a Spec document.
    ///
    /// `markdown_content` is the spec's full `.md` file.
    /// `parent_markdown_content` is the parent Plan's full `.md` file.
    pub async fn validate_spec(
        &self,
        target_id: &str,
        markdown_content: &str,
        parent_markdown_content: &str,
    ) -> Result<ValidationReport> {
        debug!("DocValidator::validate_spec(target_id={})", target_id);
        let prompt = prompts::spec_prompt(markdown_content, parent_markdown_content);
        self.run_validation("specs", target_id, &prompt).await
    }

    /// Validate a Phase document.
    ///
    /// `markdown_content` is the phase's full `.md` file.
    /// `parent_markdown_content` is the parent Spec's full `.md` file.
    pub async fn validate_phase(
        &self,
        target_id: &str,
        markdown_content: &str,
        parent_markdown_content: &str,
    ) -> Result<ValidationReport> {
        debug!("DocValidator::validate_phase(target_id={})", target_id);
        let prompt = prompts::phase_prompt(markdown_content, parent_markdown_content);
        self.run_validation("phases", target_id, &prompt).await
    }

    /// Internal: send prompt to LLM, parse response, build ValidationReport.
    async fn run_validation(&self, collection: &str, target_id: &str, prompt: &str) -> Result<ValidationReport> {
        debug!(
            "DocValidator::run_validation(collection={}, target_id={})",
            collection, target_id
        );
        info!("Running validation for {}/{}", collection, target_id);

        let raw = self
            .llm_client
            .call_with_retry(prompt)
            .await
            .context("LLM validation call failed")?;

        let parsed = self.parse_response(&raw).unwrap_or_else(|e| {
            // Fallback: if parsing fails, return a Fail verdict with the raw output
            info!("Failed to parse LLM response, falling back to Fail verdict: {}", e);
            LlmValidationResponse {
                verdict: ValidationVerdict::Fail,
                issues: vec![LlmIssue {
                    severity: IssueSeverity::Error,
                    category: "parse_error".to_string(),
                    message: format!("Failed to parse LLM response: {}", e),
                    suggestion: None,
                }],
                summary: format!("LLM response could not be parsed. Raw output: {}", raw),
            }
        });

        let issues = parsed
            .issues
            .into_iter()
            .map(|i| ValidationIssue {
                severity: i.severity,
                category: i.category,
                message: i.message,
                suggestion: i.suggestion,
            })
            .collect();

        Ok(ValidationReport::new(
            collection.to_string(),
            target_id.to_string(),
            parsed.verdict,
            issues,
            parsed.summary,
            self.model.clone(),
        ))
    }

    /// Parse the raw LLM text response into a structured response.
    fn parse_response(&self, raw: &str) -> Result<LlmValidationResponse> {
        // Strip markdown code fences if present
        let cleaned = raw
            .trim()
            .strip_prefix("```json")
            .or_else(|| raw.trim().strip_prefix("```"))
            .unwrap_or(raw.trim());
        let cleaned = cleaned.strip_suffix("```").unwrap_or(cleaned).trim();

        serde_json::from_str(cleaned).context("Failed to parse LLM response as JSON")
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
impl<H: HttpClient> DocValidator<H> {
    /// Create a DocValidator with a mock HTTP client (for tests).
    pub fn with_http_client(config: ValidatorConfig, http_client: H) -> Self {
        let model = config.model.clone();
        Self {
            llm_client: LlmClient::new(config, http_client),
            model,
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use client::HttpClient;

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

    /// Generate a unique env var name, set it, and return (config, env_var_name).
    /// Caller must clean up the env var after use.
    fn mock_config_with_key() -> (ValidatorConfig, String) {
        let env_var = format!("TEST_VALIDATOR_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_var, "test-key") };
        let config = ValidatorConfig {
            enabled: true,
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: env_var.clone(),
            max_tokens: 4096,
            temperature: 0.0,
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

    fn passing_llm_response() -> String {
        r#"{"verdict":"pass","issues":[],"summary":"Document meets all criteria."}"#.to_string()
    }

    fn failing_llm_response() -> String {
        r#"{"verdict":"fail","issues":[{"severity":"error","category":"completeness","message":"Missing acceptance criteria","suggestion":"Add measurable criteria"}],"summary":"Document is incomplete."}"#.to_string()
    }

    fn warning_llm_response() -> String {
        r#"{"verdict":"warn","issues":[{"severity":"warning","category":"scope","message":"Scope may be too broad","suggestion":null}],"summary":"Passes with warnings."}"#.to_string()
    }

    #[tokio::test]
    async fn test_validate_plan_pass() {
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator
            .validate_plan(
                "plan-1",
                "---\ntitle: Test Plan\n---\n\nA plan\n\n## Acceptance Criteria\n\n- Must work",
            )
            .await
            .unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Pass);
        assert_eq!(report.target_collection, "plans");
        assert_eq!(report.target_id, "plan-1");
        assert!(report.issues.is_empty());

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_validate_plan_fail() {
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&failing_llm_response()));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator
            .validate_plan("plan-2", "---\ntitle: Bad Plan\n---\n\nVague")
            .await
            .unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Fail);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].severity, IssueSeverity::Error);
        assert_eq!(report.issues[0].category, "completeness");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_validate_spec_warn() {
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&warning_llm_response()));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator
            .validate_spec(
                "spec-1",
                "---\ntitle: My Spec\nparent-id: pl-123\n---\n\nDesc",
                "---\ntitle: Parent Plan\n---\n\nPlan body",
            )
            .await
            .unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Warn);
        assert_eq!(report.target_collection, "specs");
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].severity, IssueSeverity::Warning);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_validate_phase() {
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator
            .validate_phase(
                "phase-1",
                "---\ntitle: My Phase\norder: 1\nparent-id: sp-123\n---\n\nDesc",
                "---\ntitle: Parent Spec\n---\n\nSpec body",
            )
            .await
            .unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Pass);
        assert_eq!(report.target_collection, "phases");
        assert_eq!(report.target_id, "phase-1");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_parse_response_with_code_fences() {
        let (config, env_var) = mock_config_with_key();
        let fenced = format!("```json\n{}\n```", r#"{"verdict":"pass","issues":[],"summary":"ok"}"#);
        let mock = MockHttpClient::new(&mock_anthropic_response(&fenced));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator.validate_plan("p1", "---\ntitle: T\n---\n\nD").await.unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Pass);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_parse_response_fallback_on_invalid_json() {
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response("This is not JSON at all"));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator.validate_plan("p1", "---\ntitle: T\n---\n\nD").await.unwrap();
        // Should fall back to Fail verdict
        assert_eq!(report.verdict, ValidationVerdict::Fail);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].category, "parse_error");
        assert!(report.summary.contains("Raw output"));

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_report_has_model_field() {
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator.validate_plan("p1", "---\ntitle: T\n---\n\nD").await.unwrap();
        assert_eq!(report.model_used, "claude-sonnet-4-6");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_report_has_valid_id_and_timestamp() {
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(config, mock);

        let report = validator.validate_plan("p1", "---\ntitle: T\n---\n\nD").await.unwrap();
        assert!(!report.id.is_empty());
        assert!(report.created_at > 0);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_parse_response_direct() {
        let (config, _env_var) = mock_config_with_key();
        let validator = DocValidator {
            llm_client: LlmClient::new(config, MockHttpClient::new("")),
            model: "test".to_string(),
        };

        let raw = r#"{"verdict":"warn","issues":[{"severity":"info","category":"clarity","message":"ok","suggestion":null}],"summary":"fine"}"#;
        let parsed = validator.parse_response(raw).unwrap();
        assert_eq!(parsed.verdict, ValidationVerdict::Warn);
        assert_eq!(parsed.issues.len(), 1);
    }
}
