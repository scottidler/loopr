use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ReadCacheKey {
    path: PathBuf,
    offset: Option<u64>,
    limit: Option<u64>,
}

#[derive(Debug, Clone)]
struct ReadCacheEntry {
    mtime: SystemTime,
    total_lines: usize,
}

/// Tracks file reads within a single agent session to detect unchanged re-reads.
///
/// Two-phase API: call `check_hit` before reading the file. On a hit, skip the
/// read entirely. On a miss, read the file, then call `record` to populate the
/// cache for next time.
#[derive(Debug, Default)]
pub struct ReadCache {
    entries: HashMap<ReadCacheKey, ReadCacheEntry>,
}

impl ReadCache {
    /// Check whether a read is a dedup hit (same path + offset + limit, mtime
    /// unchanged). Returns Some(total_lines) on hit, None on miss.
    /// Does NOT insert on miss - call `record` after reading the file.
    pub fn check_hit(
        &self,
        path: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
        current_mtime: SystemTime,
    ) -> Option<usize> {
        let key = ReadCacheKey {
            path: path.to_path_buf(),
            offset,
            limit,
        };
        match self.entries.get(&key) {
            Some(entry) if entry.mtime == current_mtime => Some(entry.total_lines),
            _ => None,
        }
    }

    /// Record a completed read so future identical reads can be deduped.
    pub fn record(
        &mut self,
        path: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
        mtime: SystemTime,
        total_lines: usize,
    ) {
        let key = ReadCacheKey {
            path: path.to_path_buf(),
            offset,
            limit,
        };
        self.entries.insert(key, ReadCacheEntry { mtime, total_lines });
    }

    /// Invalidate all cache entries for a path (any offset/limit).
    /// Called after write_file or edit_file actions.
    pub fn invalidate(&mut self, path: &Path) {
        self.entries.retain(|k, _| k.path != *path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn mtime(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn check_hit_returns_none_before_record() {
        let cache = ReadCache::default();
        let path = Path::new("/tmp/test.rs");
        assert!(cache.check_hit(path, None, None, mtime(100)).is_none());
    }

    #[test]
    fn check_hit_returns_some_after_record_same_mtime() {
        let mut cache = ReadCache::default();
        let path = Path::new("/tmp/test.rs");
        cache.record(path, None, None, mtime(100), 42);
        assert_eq!(cache.check_hit(path, None, None, mtime(100)), Some(42));
    }

    #[test]
    fn check_hit_returns_none_when_mtime_differs() {
        let mut cache = ReadCache::default();
        let path = Path::new("/tmp/test.rs");
        cache.record(path, None, None, mtime(100), 42);
        assert!(cache.check_hit(path, None, None, mtime(200)).is_none());
    }

    #[test]
    fn different_offset_limit_are_different_keys() {
        let mut cache = ReadCache::default();
        let path = Path::new("/tmp/test.rs");
        cache.record(path, Some(1), Some(500), mtime(100), 42);
        // Same path, different offset/limit -> miss
        assert!(cache.check_hit(path, Some(1), Some(100), mtime(100)).is_none());
        assert!(cache.check_hit(path, None, None, mtime(100)).is_none());
        // Same params -> hit
        assert_eq!(cache.check_hit(path, Some(1), Some(500), mtime(100)), Some(42));
    }

    #[test]
    fn invalidate_clears_all_entries_for_path() {
        let mut cache = ReadCache::default();
        let path = Path::new("/tmp/test.rs");
        cache.record(path, None, None, mtime(100), 42);
        cache.record(path, Some(500), Some(500), mtime(100), 42);
        cache.invalidate(path);
        assert!(cache.check_hit(path, None, None, mtime(100)).is_none());
        assert!(cache.check_hit(path, Some(500), Some(500), mtime(100)).is_none());
    }

    #[test]
    fn invalidate_does_not_affect_other_paths() {
        let mut cache = ReadCache::default();
        let path_a = Path::new("/tmp/a.rs");
        let path_b = Path::new("/tmp/b.rs");
        cache.record(path_a, None, None, mtime(100), 10);
        cache.record(path_b, None, None, mtime(100), 20);
        cache.invalidate(path_a);
        assert!(cache.check_hit(path_a, None, None, mtime(100)).is_none());
        assert_eq!(cache.check_hit(path_b, None, None, mtime(100)), Some(20));
    }

    #[test]
    fn record_then_invalidate_then_check_is_miss() {
        let mut cache = ReadCache::default();
        let path = Path::new("/tmp/test.rs");
        cache.record(path, None, None, mtime(100), 42);
        cache.invalidate(path);
        assert!(cache.check_hit(path, None, None, mtime(100)).is_none());
    }

    #[test]
    fn record_updates_existing_entry() {
        let mut cache = ReadCache::default();
        let path = Path::new("/tmp/test.rs");
        cache.record(path, None, None, mtime(100), 42);
        cache.record(path, None, None, mtime(200), 99);
        // Old mtime misses
        assert!(cache.check_hit(path, None, None, mtime(100)).is_none());
        // New mtime hits with updated line count
        assert_eq!(cache.check_hit(path, None, None, mtime(200)), Some(99));
    }
}
