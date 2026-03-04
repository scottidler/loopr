use std::ops::Deref;
use std::path::{Path, PathBuf};

/// RAII guard for test temporary directories.
/// Creates the directory on construction, removes it on drop.
/// Implements `Deref<Target=Path>` so `&dir` auto-coerces to `&Path`,
/// and `dir.join(...)`, `dir.display()` etc. work directly.
pub struct TestDir(PathBuf);

impl TestDir {
    /// Create a new temp directory with the given prefix.
    /// Directory is created immediately; removed when this guard is dropped.
    pub fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{}-{}", prefix, crate::id::generate_id("xx")));
        std::fs::create_dir_all(&dir).expect("failed to create test dir");
        Self(dir)
    }
}

impl Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_testdir_creates_and_derefs() {
        let dir = TestDir::new("loopr-testutil-test");
        assert!(dir.exists(), "directory should exist after creation");
        assert!(dir.join("subdir").starts_with(&*dir));
    }

    #[test]
    fn test_testdir_cleans_up_on_drop() {
        let path = {
            let dir = TestDir::new("loopr-testutil-drop");
            let p = dir.to_path_buf();
            assert!(p.exists());
            p
        };
        assert!(!path.exists(), "directory should be removed after drop");
    }
}
