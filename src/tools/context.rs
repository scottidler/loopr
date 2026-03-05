use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, eyre};
use tokio::sync::Mutex;

/// Execution context for tools — provides working directory, read tracking, and sandbox enforcement.
///
/// The `working_dir` could be a git worktree, repo root, or CWD — the tool doesn't care.
/// Callers construct the appropriate ToolContext for their use case.
pub struct ToolContext {
    /// The directory tools operate in. Could be a git worktree, repo root, or CWD.
    pub working_dir: PathBuf,
    pub exec_id: String,
    read_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub sandbox_enabled: bool,
    deny_patterns: Vec<String>,
}

impl ToolContext {
    pub fn new(working_dir: PathBuf, exec_id: String) -> Self {
        Self {
            working_dir,
            exec_id,
            read_files: Arc::new(Mutex::new(HashSet::new())),
            sandbox_enabled: true,
            deny_patterns: default_deny_patterns(),
        }
    }

    pub fn with_deny_patterns(mut self, patterns: Vec<String>) -> Self {
        self.deny_patterns = patterns;
        self
    }

    pub fn with_sandbox(mut self, enabled: bool) -> Self {
        self.sandbox_enabled = enabled;
        self
    }

    pub async fn track_read(&self, path: &Path) {
        let mut files = self.read_files.lock().await;
        files.insert(path.to_path_buf());
    }

    pub async fn was_read(&self, path: &Path) -> bool {
        let files = self.read_files.lock().await;
        files.contains(path)
    }

    /// Validate that a path is within the working directory (sandbox enforcement)
    /// and not in the deny list.
    pub fn validate_path(&self, path: &str) -> Result<PathBuf> {
        let resolved = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.working_dir.join(path)
        };

        // Canonicalize working_dir for comparison (resolve symlinks)
        let canon_working = self
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| self.working_dir.clone());

        if self.sandbox_enabled {
            // Check if the resolved path is within working_dir
            let canon_resolved = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
            if !canon_resolved.starts_with(&canon_working) {
                return Err(eyre!(
                    "path '{}' escapes sandbox (working_dir: {})",
                    path,
                    self.working_dir.display()
                ));
            }
        }

        // Check deny patterns
        let path_str = path.to_lowercase();
        for pattern in &self.deny_patterns {
            if path_str.contains(&pattern.to_lowercase()) {
                return Err(eyre!("path '{}' matches deny pattern '{}'", path, pattern));
            }
        }

        Ok(resolved)
    }
}

/// Default deny patterns for security-sensitive files.
fn default_deny_patterns() -> Vec<String> {
    vec![
        ".env".to_string(),
        ".key".to_string(),
        ".pem".to_string(),
        "credentials".to_string(),
        "secret".to_string(),
    ]
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_context_new() {
        let ctx = ToolContext::new(PathBuf::from("/tmp/test"), "test-1".to_string());
        assert_eq!(ctx.working_dir, PathBuf::from("/tmp/test"));
        assert_eq!(ctx.exec_id, "test-1");
        assert!(ctx.sandbox_enabled);
    }

    #[tokio::test]
    async fn test_read_tracking() {
        let ctx = ToolContext::new(PathBuf::from("/tmp/test"), "test-1".to_string());
        let path = PathBuf::from("/tmp/test/foo.rs");
        assert!(!ctx.was_read(&path).await);
        ctx.track_read(&path).await;
        assert!(ctx.was_read(&path).await);
    }

    #[test]
    fn test_validate_path_relative() {
        let dir = std::env::temp_dir();
        let ctx = ToolContext::new(dir.clone(), "test-1".to_string()).with_deny_patterns(vec![]);
        let result = ctx.validate_path("foo.rs");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.join("foo.rs"));
    }

    #[test]
    fn test_validate_path_deny_pattern() {
        let ctx = ToolContext::new(PathBuf::from("/tmp/test"), "test-1".to_string());
        let result = ctx.validate_path("config/.env");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("deny pattern"));
    }

    #[test]
    fn test_validate_path_deny_pem() {
        let ctx = ToolContext::new(PathBuf::from("/tmp/test"), "test-1".to_string());
        let result = ctx.validate_path("server.pem");
        assert!(result.is_err());
    }

    #[test]
    fn test_with_deny_patterns_override() {
        let ctx = ToolContext::new(std::env::temp_dir(), "test-1".to_string()).with_deny_patterns(vec![]);
        // .env is allowed when deny patterns are empty
        let result = ctx.validate_path(".env");
        assert!(result.is_ok());
    }

    #[test]
    fn test_with_sandbox_disabled() {
        let ctx = ToolContext::new(PathBuf::from("/tmp/test"), "test-1".to_string())
            .with_sandbox(false)
            .with_deny_patterns(vec![]);
        // Absolute path outside working_dir is allowed when sandbox is disabled
        let result = ctx.validate_path("/etc/hosts");
        assert!(result.is_ok());
    }
}
