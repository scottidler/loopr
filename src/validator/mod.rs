pub mod client;
pub mod prompts;

use eyre::{Context, Result};
use log::info;

use crate::config::ValidatorConfig;
use crate::domain::validation::{IssueSeverity, ValidationIssue, ValidationReport, ValidationVerdict};

use client::LlmClient;

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
pub struct DocValidator {
    llm_client: LlmClient,
    model: String,
}

impl DocValidator {
    /// Create a DocValidator with a real ureq HTTP client.
    pub fn new(config: ValidatorConfig) -> Self {
        let model = config.model.clone();
        Self {
            llm_client: LlmClient::with_ureq(config),
            model,
        }
    }

    /// Validate a Plan document.
    pub fn validate_plan(
        &self,
        target_id: &str,
        title: &str,
        description: &str,
        acceptance_criteria: &str,
    ) -> Result<ValidationReport> {
        let prompt = prompts::plan_prompt(title, description, acceptance_criteria);
        self.run_validation("plans", target_id, &prompt)
    }

    /// Validate a Spec document.
    pub fn validate_spec(
        &self,
        target_id: &str,
        title: &str,
        description: &str,
        plan_title: &str,
    ) -> Result<ValidationReport> {
        let prompt = prompts::spec_prompt(title, description, plan_title);
        self.run_validation("specs", target_id, &prompt)
    }

    /// Validate a Phase document.
    pub fn validate_phase(
        &self,
        target_id: &str,
        title: &str,
        description: &str,
        order: u32,
        spec_title: &str,
    ) -> Result<ValidationReport> {
        let prompt = prompts::phase_prompt(title, description, order, spec_title);
        self.run_validation("phases", target_id, &prompt)
    }

    /// Internal: send prompt to LLM, parse response, build ValidationReport.
    fn run_validation(&self, collection: &str, target_id: &str, prompt: &str) -> Result<ValidationReport> {
        info!("Running validation for {}/{}", collection, target_id);

        let raw = self
            .llm_client
            .call_with_retry(prompt)
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

#[cfg(test)]
impl DocValidator {
    /// Create a DocValidator with a mock HTTP client (for tests).
    pub fn with_http_client(config: ValidatorConfig, http_client: Box<dyn client::HttpClient>) -> Self {
        let model = config.model.clone();
        Self {
            llm_client: LlmClient::new(config, http_client),
            model,
        }
    }
}

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
        fn post(&self, _url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    fn mock_config() -> ValidatorConfig {
        ValidatorConfig {
            enabled: true,
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "TEST_VALIDATOR_API_KEY".to_string(),
            max_tokens: 4096,
            temperature: 0.0,
        }
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

    #[test]
    fn test_validate_plan_pass() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator
            .validate_plan("plan-1", "Test Plan", "A plan", "Must work")
            .unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Pass);
        assert_eq!(report.target_collection, "plans");
        assert_eq!(report.target_id, "plan-1");
        assert!(report.issues.is_empty());

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_validate_plan_fail() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let mock = MockHttpClient::new(&mock_anthropic_response(&failing_llm_response()));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator.validate_plan("plan-2", "Bad Plan", "Vague", "").unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Fail);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].severity, IssueSeverity::Error);
        assert_eq!(report.issues[0].category, "completeness");

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_validate_spec_warn() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let mock = MockHttpClient::new(&mock_anthropic_response(&warning_llm_response()));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator
            .validate_spec("spec-1", "My Spec", "Desc", "Parent Plan")
            .unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Warn);
        assert_eq!(report.target_collection, "specs");
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].severity, IssueSeverity::Warning);

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_validate_phase() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator
            .validate_phase("phase-1", "My Phase", "Desc", 1, "Parent Spec")
            .unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Pass);
        assert_eq!(report.target_collection, "phases");
        assert_eq!(report.target_id, "phase-1");

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_parse_response_with_code_fences() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let fenced = format!("```json\n{}\n```", r#"{"verdict":"pass","issues":[],"summary":"ok"}"#);
        let mock = MockHttpClient::new(&mock_anthropic_response(&fenced));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator.validate_plan("p1", "T", "D", "C").unwrap();
        assert_eq!(report.verdict, ValidationVerdict::Pass);

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_parse_response_fallback_on_invalid_json() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let mock = MockHttpClient::new(&mock_anthropic_response("This is not JSON at all"));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator.validate_plan("p1", "T", "D", "C").unwrap();
        // Should fall back to Fail verdict
        assert_eq!(report.verdict, ValidationVerdict::Fail);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].category, "parse_error");
        assert!(report.summary.contains("Raw output"));

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_report_has_model_field() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator.validate_plan("p1", "T", "D", "C").unwrap();
        assert_eq!(report.model_used, "claude-sonnet-4-6");

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_report_has_valid_id_and_timestamp() {
        unsafe { std::env::set_var("TEST_VALIDATOR_API_KEY", "test-key") };

        let mock = MockHttpClient::new(&mock_anthropic_response(&passing_llm_response()));
        let validator = DocValidator::with_http_client(mock_config(), Box::new(mock));

        let report = validator.validate_plan("p1", "T", "D", "C").unwrap();
        assert!(!report.id.is_empty());
        assert!(report.created_at > 0);

        unsafe { std::env::remove_var("TEST_VALIDATOR_API_KEY") };
    }

    #[test]
    fn test_parse_response_direct() {
        let validator = DocValidator {
            llm_client: LlmClient::new(mock_config(), Box::new(MockHttpClient::new(""))),
            model: "test".to_string(),
        };

        let raw = r#"{"verdict":"warn","issues":[{"severity":"info","category":"clarity","message":"ok","suggestion":null}],"summary":"fine"}"#;
        let parsed = validator.parse_response(raw).unwrap();
        assert_eq!(parsed.verdict, ValidationVerdict::Warn);
        assert_eq!(parsed.issues.len(), 1);
    }
}
