//! Token usage reported by the Anthropic Messages API.
//!
//! Phase 4 of the Tier-1 cleanup widens `LlmClient` to surface
//! `Usage` alongside the existing payload so the daemon's per-process
//! digest can record cache-hit ratios without re-instrumenting every
//! call site. The cache fields default to zero so older
//! pre-prompt-caching responses deserialize cleanly.

use serde::Deserialize;

/// Token counts for one Messages-API response.
///
/// `cache_creation_input_tokens` increments the first time a system
/// prompt with `cache_control: ephemeral` lands at the API; subsequent
/// calls with the same byte-stable system prompt increment
/// `cache_read_input_tokens` instead. The two fields are
/// mutually-exclusive at the per-call granularity Anthropic exposes.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Cache-read tokens divided by total input-side tokens (read +
    /// create + uncached). Returns 0.0 when the denominator is zero
    /// (e.g. a stub response with all fields at zero).
    pub fn cache_hit_ratio(&self) -> f64 {
        let denom = self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens;
        if denom == 0 {
            0.0
        } else {
            (self.cache_read_input_tokens as f64) / (denom as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_ratio_handles_all_zero() {
        let u = Usage::default();
        assert_eq!(u.cache_hit_ratio(), 0.0);
    }

    #[test]
    fn cache_hit_ratio_pure_read() {
        let u = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 100,
        };
        assert_eq!(u.cache_hit_ratio(), 1.0);
    }

    #[test]
    fn cache_hit_ratio_mixed() {
        let u = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 100,
        };
        assert!((u.cache_hit_ratio() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn deserialize_old_response_without_cache_fields() {
        let json = r#"{"input_tokens": 50, "output_tokens": 25}"#;
        let u: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(u.input_tokens, 50);
        assert_eq!(u.output_tokens, 25);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn deserialize_new_response_with_cache_fields() {
        let json = r#"{
            "input_tokens": 50,
            "output_tokens": 25,
            "cache_creation_input_tokens": 1024,
            "cache_read_input_tokens": 2048
        }"#;
        let u: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(u.cache_creation_input_tokens, 1024);
        assert_eq!(u.cache_read_input_tokens, 2048);
    }
}
