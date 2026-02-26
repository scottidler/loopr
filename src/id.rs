use std::time::{SystemTime, UNIX_EPOCH};
use ulid::Ulid;

/// Generate a new unique ID using ULID (Universally Unique Lexicographically Sortable Identifier).
/// ULIDs are time-ordered, so IDs sort chronologically.
pub fn generate_id() -> String {
    Ulid::new().to_string()
}

/// Return the current Unix timestamp in milliseconds.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_id_is_26_chars() {
        let id = generate_id();
        // ULIDs are 26 characters in Crockford's Base32
        assert_eq!(id.len(), 26);
    }

    #[test]
    fn test_generate_id_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| generate_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len());
    }

    #[test]
    fn test_generate_id_lexicographic_order() {
        // IDs generated later should sort after earlier ones
        let id1 = generate_id();
        // Small delay to ensure different timestamp component
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = generate_id();
        assert!(id2 > id1, "later ID should sort after earlier ID");
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
