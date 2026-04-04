pub mod prompts;

use eyre::{Context, Result};
use log::{debug, info};

use crate::config::ValidatorConfig;
use crate::domain::coverage::{
    CoverageGap, CoverageReport, CoverageReportParams, CoverageVerdict, GapSeverity, OutOfScopeItem,
};
use crate::validator::client::LlmClient;

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

/// Parameters for Phase → Works coverage evaluation.
pub struct PhaseWorksParams {
    pub id: String,
    pub title: String,
    pub description: String,
    pub order: u32,
    pub spec_title: String,
}

/// Coverage Evaluator — checks parent→children semantic completeness.
///
/// Architecturally parallel to the Doc Validator:
/// - Doc Validator: validates **one document** against quality criteria
/// - Coverage Evaluator: validates **parent + children set** against coverage criteria
///
/// Both use synchronous LLM calls (ureq via `LlmClient`), both produce persisted reports.
pub struct CoverageEvaluator {
    llm_client: LlmClient,
    model: String,
}

impl CoverageEvaluator {
    /// Create a CoverageEvaluator with a real ureq HTTP client.
    pub fn new(config: ValidatorConfig) -> Self {
        debug!("CoverageEvaluator::new(model={})", config.model);
        let model = config.model.clone();
        Self {
            llm_client: LlmClient::with_ureq(config),
            model,
        }
    }

    /// Evaluate coverage of Plan → Specs.
    pub fn evaluate_plan_specs(
        &self,
        plan_id: &str,
        plan_title: &str,
        plan_description: &str,
        plan_acceptance_criteria: &str,
        specs_list: &str,
        children_ids: Vec<String>,
    ) -> Result<CoverageReport> {
        debug!("CoverageEvaluator::evaluate_plan_specs(plan_id={})", plan_id);
        let prompt = prompts::plan_specs_prompt(plan_title, plan_description, plan_acceptance_criteria, specs_list);
        self.run_evaluation("plans", plan_id, "specs", children_ids, &prompt)
    }

    /// Evaluate coverage of Spec → Phases.
    pub fn evaluate_spec_phases(
        &self,
        spec_id: &str,
        spec_title: &str,
        spec_description: &str,
        plan_title: &str,
        phases_list: &str,
        children_ids: Vec<String>,
    ) -> Result<CoverageReport> {
        debug!("CoverageEvaluator::evaluate_spec_phases(spec_id={})", spec_id);
        let prompt = prompts::spec_phases_prompt(spec_title, spec_description, plan_title, phases_list);
        self.run_evaluation("specs", spec_id, "phases", children_ids, &prompt)
    }

    /// Evaluate coverage of Phase → Works.
    pub fn evaluate_phase_works(
        &self,
        phase: &PhaseWorksParams,
        works_list: &str,
        children_ids: Vec<String>,
    ) -> Result<CoverageReport> {
        debug!("CoverageEvaluator::evaluate_phase_works(phase_id={})", phase.id);
        let prompt = prompts::phase_works_prompt(
            &phase.title,
            &phase.description,
            phase.order,
            &phase.spec_title,
            works_list,
        );
        self.run_evaluation("phases", &phase.id, "works", children_ids, &prompt)
    }

    /// Internal: send prompt to LLM, parse response, build CoverageReport.
    fn run_evaluation(
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
impl CoverageEvaluator {
    /// Create a CoverageEvaluator with a mock HTTP client (for tests).
    pub fn with_http_client(
        config: ValidatorConfig,
        http_client: Box<dyn crate::validator::client::HttpClient>,
    ) -> Self {
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
        fn post(&self, _url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    fn mock_config_with_key() -> (ValidatorConfig, String) {
        let env_var = format!("TEST_COVERAGE_KEY_{}", crate::id::generate_id("xx"));
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

    fn complete_llm_response() -> String {
        r#"{"verdict":"complete","gaps":[],"out_of_scope":[],"summary":"Full coverage achieved."}"#.to_string()
    }

    fn incomplete_llm_response() -> String {
        r#"{"verdict":"incomplete","gaps":[{"parent_criterion":"audit logging","description":"No spec covers audit logging","severity":"critical"}],"out_of_scope":[{"child_id":"sp-abc12","description":"Includes email notifications"}],"summary":"Missing audit logging coverage."}"#.to_string()
    }

    #[test]
    fn test_evaluate_plan_specs_complete() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let report = evaluator
            .evaluate_plan_specs(
                "pl-123",
                "Test Plan",
                "A plan",
                "Must work",
                "- Spec 1: covers auth",
                vec!["sp-1".to_string()],
            )
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);
        assert_eq!(report.parent_collection, "plans");
        assert_eq!(report.parent_id, "pl-123");
        assert_eq!(report.children_collection, "specs");
        assert!(report.gaps.is_empty());
        assert!(report.out_of_scope.is_empty());

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_evaluate_plan_specs_incomplete() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&incomplete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let report = evaluator
            .evaluate_plan_specs(
                "pl-456",
                "Bad Plan",
                "Desc",
                "Auth + logging",
                "- Spec 1: only auth",
                vec!["sp-1".to_string()],
            )
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Incomplete);
        assert_eq!(report.gaps.len(), 1);
        assert_eq!(report.gaps[0].parent_criterion, "audit logging");
        assert_eq!(report.gaps[0].severity, GapSeverity::Critical);
        assert_eq!(report.out_of_scope.len(), 1);
        assert_eq!(report.out_of_scope[0].child_id, "sp-abc12");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_evaluate_spec_phases() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let report = evaluator
            .evaluate_spec_phases(
                "sp-123",
                "My Spec",
                "Spec desc",
                "Plan Title",
                "- Phase 1",
                vec!["ph-1".to_string()],
            )
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);
        assert_eq!(report.parent_collection, "specs");
        assert_eq!(report.children_collection, "phases");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_evaluate_phase_works() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let params = PhaseWorksParams {
            id: "ph-123".into(),
            title: "My Phase".into(),
            description: "Phase desc".into(),
            order: 1,
            spec_title: "Spec Title".into(),
        };
        let report = evaluator
            .evaluate_phase_works(&params, "- Work 1", vec!["wk-1".to_string()])
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);
        assert_eq!(report.parent_collection, "phases");
        assert_eq!(report.children_collection, "works");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_parse_response_with_code_fences() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let fenced = format!("```json\n{}\n```", &complete_llm_response());
        let mock = MockHttpClient::new(&mock_anthropic_response(&fenced));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let report = evaluator
            .evaluate_plan_specs("pl-1", "T", "D", "C", "specs", vec!["sp-1".to_string()])
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Complete);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_parse_response_fallback_on_invalid_json() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response("This is not JSON at all"));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let report = evaluator
            .evaluate_plan_specs("pl-1", "T", "D", "C", "specs", vec!["sp-1".to_string()])
            .unwrap();
        assert_eq!(report.verdict, CoverageVerdict::Incomplete);
        assert_eq!(report.gaps.len(), 1);
        assert_eq!(report.gaps[0].parent_criterion, "parse_error");
        assert!(report.summary.contains("Raw output"));

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_report_has_model_field() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let report = evaluator
            .evaluate_plan_specs("pl-1", "T", "D", "C", "specs", vec!["sp-1".to_string()])
            .unwrap();
        assert_eq!(report.model_used, "claude-sonnet-4-6");

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_report_has_valid_id_and_timestamp() {
        crate::prompts::init_defaults();
        let (config, env_var) = mock_config_with_key();
        let mock = MockHttpClient::new(&mock_anthropic_response(&complete_llm_response()));
        let evaluator = CoverageEvaluator::with_http_client(config, Box::new(mock));

        let report = evaluator
            .evaluate_plan_specs("pl-1", "T", "D", "C", "specs", vec!["sp-1".to_string()])
            .unwrap();
        assert!(report.id.starts_with("cr-"));
        assert!(report.created_at > 0);

        unsafe { std::env::remove_var(&env_var) };
    }

    #[test]
    fn test_parse_response_direct() {
        let (config, _env_var) = mock_config_with_key();
        let evaluator = CoverageEvaluator {
            llm_client: LlmClient::new(config, Box::new(MockHttpClient::new(""))),
            model: "test".to_string(),
        };

        let raw = r#"{"verdict":"incomplete","gaps":[{"parent_criterion":"tests","description":"missing","severity":"minor"}],"out_of_scope":[],"summary":"needs tests"}"#;
        let parsed = evaluator.parse_response(raw).unwrap();
        assert_eq!(parsed.verdict, CoverageVerdict::Incomplete);
        assert_eq!(parsed.gaps.len(), 1);
        assert_eq!(parsed.gaps[0].severity, GapSeverity::Minor);
    }
}
