use std::time::{SystemTime, UNIX_EPOCH};

/// Generate a new unique ID with a short prefix format: `{prefix}-{5-char base36}`.
/// Example: `generate_id("wk")` → `"wk-k7m2p"`.
pub fn generate_id(prefix: &str) -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let code: String = (0..5)
        .map(|_| {
            let idx = rng.random_range(0..36u8);
            if idx < 10 { (b'0' + idx) as char } else { (b'a' + idx - 10) as char }
        })
        .collect();
    format!("{prefix}-{code}")
}

/// Return the current Unix timestamp in milliseconds.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as i64
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_id_format() {
        let id = generate_id("wk");
        // Format: "wk-xxxxx" = 8 chars
        assert_eq!(id.len(), 8);
        assert!(id.starts_with("wk-"));
    }

    #[test]
    fn test_generate_id_prefix_passthrough() {
        let id = generate_id("pl");
        assert!(id.starts_with("pl-"));
        let id = generate_id("bd");
        assert!(id.starts_with("bd-"));
    }

    #[test]
    fn test_generate_id_base36_chars() {
        let id = generate_id("xx");
        let code = &id[3..]; // after "xx-"
        assert!(code.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn test_generate_id_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| generate_id("wk")).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len());
    }

    #[test]
    fn test_now_millis_positive() {
        let ts = now_millis();
        assert!(ts > 0);
    }

    #[test]
    fn test_now_millis_reasonable() {
        let ts = now_millis();
        // Should be after 2020-01-01 and before 2100-01-01
        let year_2020_ms: i64 = 1_577_836_800_000;
        let year_2100_ms: i64 = 4_102_444_800_000;
        assert!(ts > year_2020_ms, "timestamp should be after 2020");
        assert!(ts < year_2100_ms, "timestamp should be before 2100");
    }
}
