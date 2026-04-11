pub mod prompts;

use eyre::{Context, Result};
use tracing::{debug, info};

use crate::config::ValidatorConfig;
use crate::domain::coverage::{
    CoverageGap, CoverageReport, CoverageReportParams, CoverageVerdict, GapSeverity, OutOfScopeItem,
};
use crate::validator::client::{HttpClient, LlmClient, ReqwestClient};

/// Parsed LLM response before it becomes a full CoverageReport.
#[derive(Debug, serde::Deserialize)]
struct LlmCoverageResponse {
    verdict: CoverageVerdict,
    gaps: Vec<LlmCoverageGap>,
    out_of_scope: Vec<LlmOutOfScopeItem>,
    summary: String,
}

#[derive(Debug, serde::Deserialize)]
struct LlmCoverageGap {
    parent_criterion: String,
    description: String,
    severity: GapSeverity,
}

#[derive(Debug, serde::Deserialize)]
struct LlmOutOfScopeItem {
    child_id: String,
    description: String,
}

/// Coverage Evaluator - checks parent->children semantic completeness.
///
/// Architecturally parallel to the Doc Validator:
/// - Doc Validator: validates **one document** against quality criteria
/// - Coverage Evaluator: validates **parent + children set** against coverage criteria
///
/// Both use async LLM calls (reqwest via `LlmClient`), both produce persisted reports.
pub struct CoverageEvaluator<H: HttpClient = ReqwestClient> {
    llm_client: LlmClient<H>,
    model: String,
}

impl CoverageEvaluator<ReqwestClient> {
    /// Create a CoverageEvaluator with a real reqwest HTTP client.
    pub fn new(config: ValidatorConfig) -> Self {
        debug!("CoverageEvaluator::new(model={})", config.llm.model);
        let model = config.llm.model.clone();
        Self {
            llm_client: LlmClient::with_reqwest(config),
            model,
        }
    }
}

impl<H: HttpClient> CoverageEvaluator<H> {
    /// Evaluate coverage of Plan -> Specs.
    ///
    /// `parent_markdown_content` is the Plan's full `.md` file.
    /// `children_markdown_content` is the concatenated Spec `.md` files separated by `---`.
    pub async fn evaluate_plan_specs(
        &self,
        plan_id: &str,
        parent_markdown_content: &str,
        children_markdown_content: &str,
        children_ids: Vec<String>,
    ) -> Result<CoverageReport> {
        debug!("CoverageEvaluator::evaluate_plan_specs(plan_id={})", plan_id);
        let prompt = prompts::plan_specs_prompt(parent_markdown_content, children_markdown_content);
        self.run_evaluation("plans", plan_id, "specs", children_ids, &prompt)
            .await
    }

    /// Evaluate coverage of Spec -> Phases.
    ///
    /// `parent_markdown_content` is the Spec's full `.md` file.
    /// `children_markdown_content` is the concatenated Phase `.md` files separated by `---`.
    pub async fn evaluate_spec_phases(
        &self,
        spec_id: &str,
        parent_markdown_content: &str,
        children_markdown_content: &str,
        children_ids: Vec<String>,
    ) -> Result<CoverageReport> {
        debug!("CoverageEvaluator::evaluate_spec_phases(spec_id={})", spec_id);
        let prompt = prompts::spec_phases_prompt(parent_markdown_content, children_markdown_content);
        self.run_evaluation("specs", spec_id, "phases", children_ids, &prompt)
            .await
    }

    /// Evaluate coverage of Phase -> Works.
    ///
    /// `parent_markdown_content` is the Phase's full `.md` file.
    /// `children_markdown_content` is the concatenated Work `.md` files separated by `---`.
    pub async fn evaluate_phase_works(
        &self,
        phase_id: &str,
        parent_markdown_content: &str,
        children_markdown_content: &str,
        children_ids: Vec<String>,
    ) -> Result<CoverageReport> {
        debug!("CoverageEvaluator::evaluate_phase_works(phase_id={})", phase_id);
        let prompt = prompts::phase_works_prompt(parent_markdown_content, children_markdown_content);
        self.run_evaluation("phases", phase_id, "works", children_ids, &prompt)
            .await
    }

    /// Internal: send prompt to LLM, parse response, build CoverageReport.
    async fn run_evaluation(
        &self,
        parent_collection: &str,
        parent_id: &str,
        children_collection: &str,
        children_ids: Vec<String>,
        prompt: &str,
    ) -> Result<CoverageReport> {
        debug!(
            "CoverageEvaluator::run_evaluation(parent={}/{}, children={})",
            parent_collection, parent_id, children_collection
        );
        info!("Running coverage evaluation for {}/{}", parent_collection, parent_id);

        let raw = self
            .llm_client
            .call_with_retry(prompt)
            .await
            .context("LLM coverage evaluation call failed")?;

        let parsed = self.parse_response(&raw).unwrap_or_else(|e| {
            info!(
                "Failed to parse coverage LLM response, falling back to Incomplete: {}",
                e
            );
            LlmCoverageResponse {
                verdict: CoverageVerdict::Incomplete,
                gaps: vec![LlmCoverageGap {
                    parent_criterion: "parse_error".to_string(),
                    description: format!("Failed to parse LLM response: {}", e),
                    severity: GapSeverity::Critical,
                }],
                out_of_scope: vec![],
                summary: format!("LLM response could not be parsed. Raw output: {}", raw),
            }
        });

        let gaps = parsed
            .gaps
            .into_iter()
            .map(|g| CoverageGap {
                parent_criterion: g.parent_criterion,
                description: g.description,
                severity: g.severity,
            })
            .collect();

        let out_of_scope = parsed
            .out_of_scope
            .into_iter()
            .map(|o| OutOfScopeItem {
                child_id: o.child_id,
                description: o.description,
            })
            .collect();

        Ok(CoverageReport::new(CoverageReportParams {
            parent_collection: parent_collection.to_string(),
            parent_id: parent_id.to_string(),
            children_collection: children_collection.to_string(),
            children_ids,
            verdict: parsed.verdict,
            gaps,
            out_of_scope,
            summary: parsed.summary,
            model_used: self.model.clone(),
        }))
    }

    /// Parse the raw LLM text response into a structured response.
    fn parse_response(&self, raw: &str) -> Result<LlmCoverageResponse> {
        let cleaned = raw
            .trim()
            .strip_prefix("```json")
            .or_else(|| raw.trim().strip_prefix("```"))
            .unwrap_or(raw.trim());
        let cleaned = cleaned.strip_suffix("```").unwrap_or(cleaned).trim();

        serde_json::from_str(cleaned).context("Failed to parse coverage LLM response as JSON")
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
impl<H: HttpClient> CoverageEvaluator<H> {
    /// Create a CoverageEvaluator with a mock HTTP client (for tests).
    pub fn with_http_client(config: ValidatorConfig, http_client: H) -> Self {
        let model = config.llm.model.clone();
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
    use crate::validator::client::HttpClient;

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

    fn mock_config_with_key() -> (ValidatorConfig, String) {
        let env_var = format!("TEST_COVERAGE_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_var, "test-key") };
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

    fn complete_llm_response() -> String {
        r#"{"verdict":"complete","gaps":[],"out_of_scope":[],"summary":"Full coverage achieved."}"#.to_string()
    }

    fn incomplete_llm_response() -> String {
        r#"{"verdict":"incomplete","gaps":[{"parent_criterion":"audit logging","description":"No spec covers audit logging","severity":"critical"}],"out_of_scope":[{"child_id":"sp-abc12","description":"Includes email notifications"}],"summary":"Missing audit logging coverage."}"#.to_string()
    }

    #[tokio::test]
    async fn test_evaluate_plan_specs_complete() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let parent_md = "---\ntitle: Test Plan\nacceptance-criteria:\n  - Must work\n---\n\nA plan";
        let children_md = "---\ntitle: Spec 1\n---\n\ncovers auth";
        let report = evaluator
            .evaluate_plan_specs("pl-123", parent_md, children_md, vec!["sp-1".to_string()])
            .await
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);
        assert_eq!(report.parent_collection, "plans");
        assert_eq!(report.parent_id, "pl-123");
        assert_eq!(report.children_collection, "specs");
        assert!(report.gaps.is_empty());
        assert!(report.out_of_scope.is_empty());

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_evaluate_plan_specs_incomplete() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&incomplete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let parent_md = "---\ntitle: Bad Plan\nacceptance-criteria:\n  - Auth + logging\n---\n\nDesc";
        let children_md = "---\ntitle: Spec 1\n---\n\nonly auth";
        let report = evaluator
            .evaluate_plan_specs("pl-456", parent_md, children_md, vec!["sp-1".to_string()])
            .await
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Incomplete);
        assert_eq!(report.gaps.len(), 1);
        assert_eq!(report.gaps[0].parent_criterion, "audit logging");
        assert_eq!(report.gaps[0].severity, GapSeverity::Critical);
        assert_eq!(report.out_of_scope.len(), 1);
        assert_eq!(report.out_of_scope[0].child_id, "sp-abc12");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_evaluate_spec_phases() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let parent_md = "---\ntitle: My Spec\nparent-id: pl-123\n---\n\nSpec desc";
        let children_md = "---\ntitle: Phase 1\norder: 0\n---\n\nPhase body";
        let report = evaluator
            .evaluate_spec_phases("sp-123", parent_md, children_md, vec!["ph-1".to_string()])
            .await
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);
        assert_eq!(report.parent_collection, "specs");
        assert_eq!(report.children_collection, "phases");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_evaluate_phase_works() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let parent_md = "---\ntitle: My Phase\norder: 1\nparent-id: sp-456\n---\n\nPhase desc";
        let children_md = "---\ntitle: Work 1\n---\n\nWork body";
        let report = evaluator
            .evaluate_phase_works("ph-123", parent_md, children_md, vec!["wk-1".to_string()])
            .await
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);
        assert_eq!(report.parent_collection, "phases");
        assert_eq!(report.children_collection, "works");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_parse_response_with_code_fences() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let fenced = format!("```json\n{}\n```", &complete_llm_response());
        let mock = MockHttpClient::new(&mock_anthropic_response(&fenced));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let report = evaluator
            .evaluate_plan_specs(
                "pl-1",
                "---\ntitle: T\n---\n\nD",
                "---\ntitle: C\n---\n\nspecs",
                vec!["sp-1".to_string()],
            )
            .await
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_parse_response_fallback_on_invalid_json() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response("This is not JSON at all"));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let report = evaluator
            .evaluate_plan_specs(
                "pl-1",
                "---\ntitle: T\n---\n\nD",
                "---\ntitle: C\n---\n\nspecs",
                vec!["sp-1".to_string()],
            )
            .await
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Incomplete);
        assert_eq!(report.gaps.len(), 1);
        assert_eq!(report.gaps[0].parent_criterion, "parse_error");
        assert!(report.summary.contains("Raw output"));

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_report_has_model_field() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let report = evaluator
            .evaluate_plan_specs(
                "pl-1",
                "---\ntitle: T\n---\n\nD",
                "---\ntitle: C\n---\n\nspecs",
                vec!["sp-1".to_string()],
            )
            .await
            .unwrap();
        assert_eq!(report.model_used, "claude-sonnet-4-6");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[tokio::test]
    async fn test_report_has_valid_id_and_timestamp() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, mock);

        let report = evaluator
            .evaluate_plan_specs(
                "pl-1",
                "---\ntitle: T\n---\n\nD",
                "---\ntitle: C\n---\n\nspecs",
                vec!["sp-1".to_string()],
            )
            .await
            .unwrap();
        assert!(report.id.starts_with("cr-"));
        assert!(report.created_at > 0);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_parse_response_direct() {
        let (config, _env_var) = mock_config_with_key();
        let evaluator = CoverageEvaluator {
            llm_client: LlmClient::new(config, MockHttpClient::new("")),
            model: "test".to_string(),
        };

        let raw = r#"{"verdict":"incomplete","gaps":[{"parent_criterion":"tests","description":"missing","severity":"minor"}],"out_of_scope":[],"summary":"needs tests"}"#;
        let parsed = evaluator.parse_response(raw).unwrap();
        assert_eq!(parsed.verdict, CoverageVerdict::Incomplete);
        assert_eq!(parsed.gaps.len(), 1);
        assert_eq!(parsed.gaps[0].severity, GapSeverity::Minor);
    }
}
