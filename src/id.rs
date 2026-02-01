//! ID generation utilities for Loopr
//!
//! Provides functions for generating unique identifiers for loops, signals, and jobs.

use rand::Rng;

/// Get current timestamp in milliseconds since Unix epoch
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Generate a unique loop ID
///
/// Format: `{timestamp_ms}-{random_hex}`
/// Example: `1738300800123-a1b2`
pub fn generate_loop_id() -> String {
    let timestamp = now_ms();
    let random: u16 = rand::rng().random();
    format!("{}-{:04x}", timestamp, random)
}

/// Generate a child ID given parent and index
///
/// Format: `{parent_suffix}-{index:03}`
/// Example: For parent "001" and index 2: "001-002"
pub fn generate_child_id(parent_id: &str, index: u32) -> String {
    // Extract the last segment of the parent ID for hierarchy
    let parent_suffix = parent_id
        .split('-')
        .last()
        .unwrap_or(parent_id);
    format!("{}-{:03}", parent_suffix, index)
}

/// Generate a signal ID
///
/// Format: `sig-{timestamp_ms}-{random_hex}`
pub fn generate_signal_id() -> String {
    let timestamp = now_ms();
    let random: u16 = rand::rng().random();
    format!("sig-{}-{:04x}", timestamp, random)
}

/// Generate a job ID for a tool execution
///
/// Format: `job-{loop_id}-{iteration}-{random_hex}`
pub fn generate_job_id(loop_id: &str, iteration: u32) -> String {
    let random: u16 = rand::rng().random();
    format!("job-{}-{}-{:04x}", loop_id, iteration, random)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_ms_returns_reasonable_timestamp() {
        let ts = now_ms();
        // Should be after 2020-01-01 and before 2100-01-01
        assert!(ts > 1577836800000); // 2020-01-01
        assert!(ts < 4102444800000); // 2100-01-01
    }

    #[test]
    fn test_generate_loop_id_format() {
        let id = generate_loop_id();
        // Should contain a hyphen
        assert!(id.contains('-'));
        // Should have timestamp part (digits) before hyphen
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].chars().all(|c| c.is_ascii_digit()));
        // Should have 4-char hex suffix
        assert_eq!(parts[1].len(), 4);
        assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_loop_id_uniqueness() {
        let id1 = generate_loop_id();
        let id2 = generate_loop_id();
        // With random component, should be different
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_generate_child_id_format() {
        let child = generate_child_id("001", 2);
        assert_eq!(child, "001-002");
    }

    #[test]
    fn test_generate_child_id_with_complex_parent() {
        let child = generate_child_id("1738300800123-a1b2", 5);
        // Should use last segment of parent
        assert_eq!(child, "a1b2-005");
    }

    #[test]
    fn test_generate_child_id_index_padding() {
        let child = generate_child_id("parent", 1);
        assert!(child.ends_with("-001"));
    }

    #[test]
    fn test_generate_signal_id_format() {
        let id = generate_signal_id();
        assert!(id.starts_with("sig-"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "sig");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(parts[2].len(), 4);
    }

    #[test]
    fn test_generate_job_id_format() {
        let id = generate_job_id("loop-001", 3);
        assert!(id.starts_with("job-"));
        assert!(id.contains("loop-001"));
        assert!(id.contains("-3-"));
    }

    #[test]
    fn test_generate_job_id_includes_iteration() {
        let id = generate_job_id("myloop", 42);
        assert!(id.contains("-42-"));
    }
}
