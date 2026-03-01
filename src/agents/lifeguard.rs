use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::agents::AgentAction;

/// Decision from the lifeguard after checking an action or error.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Continue normally.
    Continue,
    /// Escalate to NeedHelp — the agent is stuck.
    Escalate(String),
}

/// Per-session loop detector. Tracks repeated actions and errors
/// to detect futile loops before the hard iteration cap.
pub struct Lifeguard {
    /// Consecutive identical action hashes.
    last_action_hash: Option<u64>,
    consecutive_action_count: u32,
    action_threshold: u32,

    /// Recent error hashes (sliding window).
    recent_errors: VecDeque<u64>,
    error_window_size: usize,
    error_threshold: u32,

    /// Consecutive parse failures.
    consecutive_parse_failures: u32,
    max_parse_retries: u32,
}

impl Default for Lifeguard {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifeguard {
    pub fn new() -> Self {
        Self {
            last_action_hash: None,
            consecutive_action_count: 0,
            action_threshold: 5,
            recent_errors: VecDeque::new(),
            error_window_size: 10,
            error_threshold: 3,
            consecutive_parse_failures: 0,
            max_parse_retries: 3,
        }
    }

    /// Check whether a sequence of actions indicates a loop.
    /// Call once per action before execution.
    pub fn check_action(&mut self, action_hash: u64) -> Verdict {
        if self.last_action_hash == Some(action_hash) {
            self.consecutive_action_count += 1;
        } else {
            self.last_action_hash = Some(action_hash);
            self.consecutive_action_count = 1;
        }

        if self.consecutive_action_count >= self.action_threshold {
            return Verdict::Escalate(format!(
                "repeated identical action {} consecutive times",
                self.consecutive_action_count
            ));
        }

        Verdict::Continue
    }

    /// Record an action error and check for repeated identical errors.
    pub fn record_error(&mut self, error: &str) -> Verdict {
        let hash = hash_string(error);

        self.recent_errors.push_back(hash);
        if self.recent_errors.len() > self.error_window_size {
            self.recent_errors.pop_front();
        }

        let same_count = self.recent_errors.iter().filter(|h| **h == hash).count() as u32;
        if same_count >= self.error_threshold {
            return Verdict::Escalate(format!(
                "same error repeated {} times: {}",
                same_count,
                truncate(error, 200),
            ));
        }

        Verdict::Continue
    }

    /// Record a parse failure. Returns Escalate if max retries exceeded.
    pub fn record_parse_failure(&mut self) -> Verdict {
        self.consecutive_parse_failures += 1;
        if self.consecutive_parse_failures > self.max_parse_retries {
            return Verdict::Escalate(format!(
                "failed to produce valid output after {} parse retries",
                self.max_parse_retries
            ));
        }
        Verdict::Continue
    }

    /// Reset parse failure counter after a successful parse.
    pub fn reset_parse_failures(&mut self) {
        self.consecutive_parse_failures = 0;
    }
}

/// Hash an AgentAction by serializing to JSON and hashing the result.
pub fn hash_action(action: &AgentAction) -> u64 {
    let json = serde_json::to_string(action).unwrap_or_default();
    hash_string(&json)
}

fn hash_string(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_below_action_threshold_continues() {
        let mut lg = Lifeguard::new();
        for _ in 0..4 {
            assert_eq!(lg.check_action(42), Verdict::Continue);
        }
    }

    #[test]
    fn test_at_action_threshold_escalates() {
        let mut lg = Lifeguard::new();
        for i in 0..5 {
            let verdict = lg.check_action(42);
            if i < 4 {
                assert_eq!(verdict, Verdict::Continue);
            } else {
                assert!(matches!(verdict, Verdict::Escalate(_)));
            }
        }
    }

    #[test]
    fn test_different_actions_reset_counter() {
        let mut lg = Lifeguard::new();
        for _ in 0..4 {
            assert_eq!(lg.check_action(42), Verdict::Continue);
        }
        // Different action resets
        assert_eq!(lg.check_action(99), Verdict::Continue);
        // Now 42 again starts from 1
        for _ in 0..4 {
            assert_eq!(lg.check_action(42), Verdict::Continue);
        }
        // 5th consecutive should escalate
        assert!(matches!(lg.check_action(42), Verdict::Escalate(_)));
    }

    #[test]
    fn test_error_below_threshold_continues() {
        let mut lg = Lifeguard::new();
        assert_eq!(lg.record_error("path escapes sandbox"), Verdict::Continue);
        assert_eq!(lg.record_error("path escapes sandbox"), Verdict::Continue);
    }

    #[test]
    fn test_error_at_threshold_escalates() {
        let mut lg = Lifeguard::new();
        assert_eq!(lg.record_error("path escapes sandbox"), Verdict::Continue);
        assert_eq!(lg.record_error("path escapes sandbox"), Verdict::Continue);
        let verdict = lg.record_error("path escapes sandbox");
        assert!(matches!(verdict, Verdict::Escalate(_)));
    }

    #[test]
    fn test_different_errors_dont_trigger() {
        let mut lg = Lifeguard::new();
        assert_eq!(lg.record_error("error A"), Verdict::Continue);
        assert_eq!(lg.record_error("error B"), Verdict::Continue);
        assert_eq!(lg.record_error("error C"), Verdict::Continue);
        assert_eq!(lg.record_error("error D"), Verdict::Continue);
    }

    #[test]
    fn test_error_window_eviction() {
        let mut lg = Lifeguard::new();
        // Fill window with 2 of the same error
        assert_eq!(lg.record_error("target"), Verdict::Continue);
        assert_eq!(lg.record_error("target"), Verdict::Continue);
        // Fill rest of window with different errors to push "target" out
        for i in 0..8 {
            assert_eq!(lg.record_error(&format!("other-{}", i)), Verdict::Continue);
        }
        // "target" should have been evicted — adding it again should be fine
        assert_eq!(lg.record_error("target"), Verdict::Continue);
    }

    #[test]
    fn test_parse_failure_below_threshold_continues() {
        let mut lg = Lifeguard::new();
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
    }

    #[test]
    fn test_parse_failure_above_threshold_escalates() {
        let mut lg = Lifeguard::new();
        for _ in 0..3 {
            assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        }
        let verdict = lg.record_parse_failure();
        assert!(matches!(verdict, Verdict::Escalate(_)));
    }

    #[test]
    fn test_parse_failure_reset() {
        let mut lg = Lifeguard::new();
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        lg.reset_parse_failures();
        // Counter reset — should take 3 more before escalating
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        assert_eq!(lg.record_parse_failure(), Verdict::Continue);
        assert!(matches!(lg.record_parse_failure(), Verdict::Escalate(_)));
    }

    #[test]
    fn test_hash_action_deterministic() {
        let action = AgentAction::WriteFile {
            path: "src/main.rs".into(),
            content: "fn main() {}".into(),
        };
        let h1 = hash_action(&action);
        let h2 = hash_action(&action);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_action_different_for_different_actions() {
        let a1 = AgentAction::WriteFile {
            path: "a.rs".into(),
            content: "x".into(),
        };
        let a2 = AgentAction::WriteFile {
            path: "b.rs".into(),
            content: "y".into(),
        };
        assert_ne!(hash_action(&a1), hash_action(&a2));
    }
}
