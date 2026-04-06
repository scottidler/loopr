use eyre::{Result, eyre};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

use crate::config::ClarityGateConfig;

const HTTP_TIMEOUT_SECS: u64 = 10;

const EVALUATION_PROMPT: &str = r#"You are a goal clarity evaluator for an autonomous software engineering system.
The system will decompose this goal into a multi-level plan and execute it
without human intervention. Vague goals waste significant resources.

Evaluate this goal for autonomous execution readiness:

<goal>
{goal}
</goal>

Rate each dimension 1-5:
- specificity: Does it describe a concrete, bounded change?
- acceptance: Can completion be objectively verified?
- scope: Is it a single coherent concern (not a wishlist)?

Respond with JSON only:
{
  "specificity": { "score": 1-5, "reason": "one sentence" },
  "acceptance": { "score": 1-5, "reason": "one sentence" },
  "scope": { "score": 1-5, "reason": "one sentence" },
  "improved_goal": "If any dimension < 3, suggest a concrete improved version of the goal."
}"#;

/// Maximum goal length sent to the gate (controls token costs).
const MAX_GOAL_LENGTH: usize = 2000;

/// A single dimension score from the clarity evaluation.
#[derive(Debug, Deserialize)]
pub struct DimensionScore {
    pub score: u8,
    pub reason: String,
}

/// The full clarity verdict returned by the LLM, parsed client-side.
#[derive(Debug, Deserialize)]
pub struct ClarityVerdict {
    pub specificity: DimensionScore,
    pub acceptance: DimensionScore,
    pub scope: DimensionScore,
    #[serde(default)]
    pub improved_goal: Option<String>,
}

impl ClarityVerdict {
    /// Pass if all dimensions meet the threshold (computed client-side).
    pub fn passes(&self, min_score: u8) -> bool {
        self.specificity.score >= min_score && self.acceptance.score >= min_score && self.scope.score >= min_score
    }

    /// Clamp all scores to the 1-5 range.
    pub fn clamp_scores(&mut self) {
        self.specificity.score = self.specificity.score.clamp(1, 5);
        self.acceptance.score = self.acceptance.score.clamp(1, 5);
        self.scope.score = self.scope.score.clamp(1, 5);
    }
}

/// CLI-side goal clarity gate. Makes a single Sonnet call to evaluate
/// whether a goal is specific enough for autonomous execution.
pub struct ClarityGate {
    client: Client,
    config: ClarityGateConfig,
    api_key: String,
}

impl ClarityGate {
    /// Create a new ClarityGate. Returns Err if the API key env var is missing.
    pub fn new(config: ClarityGateConfig) -> Result<Self> {
        let api_key = std::env::var(&config.api_key_env)
            .map_err(|_| eyre!("API key not found in env var: {}", config.api_key_env))?;
        Ok(Self {
            client: Client::new(),
            config,
            api_key,
        })
    }

    /// Evaluate a goal for clarity. Returns the parsed verdict.
    pub async fn evaluate(&self, goal: &str) -> Result<ClarityVerdict> {
        let truncated = if goal.len() > MAX_GOAL_LENGTH {
            warn!(
                "Goal truncated from {} to {} chars for clarity gate",
                goal.len(),
                MAX_GOAL_LENGTH
            );
            &goal[..MAX_GOAL_LENGTH]
        } else {
            goal
        };

        let prompt = EVALUATION_PROMPT.replace("{goal}", truncated);

        let body = serde_json::json!({
            "model": self.config.model,
            "max_tokens": 512,
            "temperature": 0.0,
            "messages": [{
                "role": "user",
                "content": prompt,
            }],
        });

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("Clarity gate API call failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre!("Clarity gate API error {}: {}", status, error_body));
        }

        let resp_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| eyre!("Failed to parse API response: {}", e))?;

        // Extract text content from the Anthropic Messages API response
        let text = resp_body
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|block| block.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| eyre!("No text content in API response"))?;

        let mut verdict: ClarityVerdict =
            serde_json::from_str(text).map_err(|e| eyre!("Failed to parse clarity verdict JSON: {}", e))?;

        verdict.clamp_scores();
        Ok(verdict)
    }
}

/// Format a failed clarity verdict for CLI output.
pub fn format_failure(goal: &str, verdict: &ClarityVerdict) -> String {
    let mut out = String::new();
    out.push_str("Goal rejected: too vague for autonomous execution.\n\n");
    out.push_str(&format!(
        "  Specificity: {}/5 - {}\n",
        verdict.specificity.score, verdict.specificity.reason
    ));
    out.push_str(&format!(
        "  Acceptance:  {}/5 - {}\n",
        verdict.acceptance.score, verdict.acceptance.reason
    ));
    out.push_str(&format!(
        "  Scope:       {}/5 - {}\n",
        verdict.scope.score, verdict.scope.reason
    ));

    if let Some(ref improved) = verdict.improved_goal {
        out.push_str(&format!("\nSuggested goal: \"{improved}\"\n"));
    }

    out.push_str("\nOptions:\n");
    out.push_str("  1. Be specific:    loopr run \"<a more specific goal>\"\n");
    out.push_str("  2. Provide a plan: loopr run --plan docs/my-feature.md\n");
    out.push_str("  3. Use the TUI:    loopr  (refine the goal interactively)\n");
    out.push_str(&format!(
        "  4. Bypass:         loopr run --skip-clarity-gate \"{goal}\"\n"
    ));

    out
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verdict_passes_all_above_threshold() {
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 4,
                reason: "good".to_string(),
            },
            acceptance: DimensionScore {
                score: 3,
                reason: "ok".to_string(),
            },
            scope: DimensionScore {
                score: 5,
                reason: "great".to_string(),
            },
            improved_goal: None,
        };
        assert!(verdict.passes(3));
    }

    #[test]
    fn test_verdict_fails_one_below_threshold() {
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 2,
                reason: "vague".to_string(),
            },
            acceptance: DimensionScore {
                score: 4,
                reason: "good".to_string(),
            },
            scope: DimensionScore {
                score: 4,
                reason: "good".to_string(),
            },
            improved_goal: Some("A better goal".to_string()),
        };
        assert!(!verdict.passes(3));
    }

    #[test]
    fn test_verdict_fails_all_below_threshold() {
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 1,
                reason: "no".to_string(),
            },
            acceptance: DimensionScore {
                score: 1,
                reason: "no".to_string(),
            },
            scope: DimensionScore {
                score: 2,
                reason: "bad".to_string(),
            },
            improved_goal: Some("Do something specific".to_string()),
        };
        assert!(!verdict.passes(3));
    }

    #[test]
    fn test_verdict_exact_threshold() {
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 3,
                reason: "ok".to_string(),
            },
            acceptance: DimensionScore {
                score: 3,
                reason: "ok".to_string(),
            },
            scope: DimensionScore {
                score: 3,
                reason: "ok".to_string(),
            },
            improved_goal: None,
        };
        assert!(verdict.passes(3));
    }

    #[test]
    fn test_clamp_scores_out_of_range() {
        let mut verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 0,
                reason: "zero".to_string(),
            },
            acceptance: DimensionScore {
                score: 10,
                reason: "ten".to_string(),
            },
            scope: DimensionScore {
                score: 3,
                reason: "ok".to_string(),
            },
            improved_goal: None,
        };
        verdict.clamp_scores();
        assert_eq!(verdict.specificity.score, 1);
        assert_eq!(verdict.acceptance.score, 5);
        assert_eq!(verdict.scope.score, 3);
    }

    #[test]
    fn test_parse_verdict_from_json() {
        let json = r#"{
            "specificity": { "score": 4, "reason": "Concrete change identified" },
            "acceptance": { "score": 5, "reason": "Testable completion criteria" },
            "scope": { "score": 4, "reason": "Single coherent concern" },
            "improved_goal": null
        }"#;
        let verdict: ClarityVerdict = serde_json::from_str(json).expect("should parse");
        assert!(verdict.passes(3));
        assert!(verdict.improved_goal.is_none());
    }

    #[test]
    fn test_parse_verdict_with_improved_goal() {
        let json = r#"{
            "specificity": { "score": 1, "reason": "No deliverable" },
            "acceptance": { "score": 1, "reason": "Not verifiable" },
            "scope": { "score": 2, "reason": "Unbounded" },
            "improved_goal": "Add a /version command that prints the crate version"
        }"#;
        let verdict: ClarityVerdict = serde_json::from_str(json).expect("should parse");
        assert!(!verdict.passes(3));
        assert_eq!(
            verdict.improved_goal.as_deref(),
            Some("Add a /version command that prints the crate version")
        );
    }

    #[test]
    fn test_parse_verdict_missing_improved_goal() {
        let json = r#"{
            "specificity": { "score": 4, "reason": "Good" },
            "acceptance": { "score": 4, "reason": "Good" },
            "scope": { "score": 4, "reason": "Good" }
        }"#;
        let verdict: ClarityVerdict = serde_json::from_str(json).expect("should parse");
        assert!(verdict.passes(3));
        assert!(verdict.improved_goal.is_none());
    }

    #[test]
    fn test_format_failure_output() {
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 1,
                reason: "No concrete deliverable identified".to_string(),
            },
            acceptance: DimensionScore {
                score: 1,
                reason: "No way to verify completion".to_string(),
            },
            scope: DimensionScore {
                score: 2,
                reason: "Unbounded improvement request".to_string(),
            },
            improved_goal: Some("Add a /health endpoint that returns JSON with uptime and version".to_string()),
        };
        let output = format_failure("make things better", &verdict);
        assert!(output.contains("Goal rejected"));
        assert!(output.contains("Specificity: 1/5"));
        assert!(output.contains("Acceptance:  1/5"));
        assert!(output.contains("Scope:       2/5"));
        assert!(output.contains("Suggested goal:"));
        assert!(output.contains("--skip-clarity-gate"));
    }

    #[test]
    fn test_format_failure_no_suggestion() {
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 2,
                reason: "Somewhat vague".to_string(),
            },
            acceptance: DimensionScore {
                score: 2,
                reason: "Hard to verify".to_string(),
            },
            scope: DimensionScore {
                score: 2,
                reason: "A bit broad".to_string(),
            },
            improved_goal: None,
        };
        let output = format_failure("do stuff", &verdict);
        assert!(output.contains("Goal rejected"));
        assert!(!output.contains("Suggested goal:"));
    }

    // --- Phase 3: Edge case tests ---

    #[test]
    fn test_parse_malformed_json_missing_field() {
        // Missing 'scope' field entirely - serde should fail
        let json = r#"{
            "specificity": { "score": 4, "reason": "Good" },
            "acceptance": { "score": 4, "reason": "Good" }
        }"#;
        let result: Result<ClarityVerdict, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_malformed_json_garbage() {
        let json = "this is not json at all";
        let result: Result<ClarityVerdict, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_malformed_json_empty_object() {
        let json = "{}";
        let result: Result<ClarityVerdict, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_clamp_scores_already_in_range() {
        let mut verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 1,
                reason: "low".to_string(),
            },
            acceptance: DimensionScore {
                score: 5,
                reason: "high".to_string(),
            },
            scope: DimensionScore {
                score: 3,
                reason: "mid".to_string(),
            },
            improved_goal: None,
        };
        verdict.clamp_scores();
        assert_eq!(verdict.specificity.score, 1);
        assert_eq!(verdict.acceptance.score, 5);
        assert_eq!(verdict.scope.score, 3);
    }

    #[test]
    fn test_new_gate_fails_without_api_key() {
        // Ensure the env var is not set for this test
        let config = ClarityGateConfig {
            enabled: true,
            model: "claude-sonnet-4-6".to_string(),
            min_score: 3,
            api_key_env: "LOOPR_TEST_NONEXISTENT_KEY_12345".to_string(),
        };
        let result = ClarityGate::new(config);
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("LOOPR_TEST_NONEXISTENT_KEY_12345"));
    }

    #[test]
    fn test_verdict_passes_threshold_1() {
        // With threshold 1, everything passes
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 1,
                reason: "minimal".to_string(),
            },
            acceptance: DimensionScore {
                score: 1,
                reason: "minimal".to_string(),
            },
            scope: DimensionScore {
                score: 1,
                reason: "minimal".to_string(),
            },
            improved_goal: None,
        };
        assert!(verdict.passes(1));
    }

    #[test]
    fn test_verdict_fails_threshold_5() {
        // With threshold 5, only perfect scores pass
        let verdict = ClarityVerdict {
            specificity: DimensionScore {
                score: 5,
                reason: "perfect".to_string(),
            },
            acceptance: DimensionScore {
                score: 4,
                reason: "almost".to_string(),
            },
            scope: DimensionScore {
                score: 5,
                reason: "perfect".to_string(),
            },
            improved_goal: None,
        };
        assert!(!verdict.passes(5));
    }
}
