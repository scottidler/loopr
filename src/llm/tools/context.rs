//! Tool execution context - scoped to a single loop's worktree

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Execution context for tools - scoped to a single loop
#[derive(Clone)]
pub struct ToolContext {
    /// Git worktree path - all file ops constrained here
    pub worktree: PathBuf,

    /// Loop execution ID (for coordination)
    pub exec_id: String,

    /// Files read this iteration (for edit validation)
    read_files: Arc<Mutex<HashSet<PathBuf>>>,

    /// Whether sandbox mode is enabled (default: true)
    pub sandbox_enabled: bool,
}

impl ToolContext {
    pub fn new(worktree: PathBuf, exec_id: String) -> Self {
        Self {
            worktree,
            exec_id,
            read_files: Arc::new(Mutex::new(HashSet::new())),
            sandbox_enabled: true,
        }
    }

    /// Create a context with sandbox disabled (for testing)
    pub fn new_unsandboxed(worktree: PathBuf, exec_id: String) -> Self {
        Self {
            worktree,
            exec_id,
            read_files: Arc::new(Mutex::new(HashSet::new())),
            sandbox_enabled: false,
        }
    }

    /// Track that a file was read (enables edit validation)
    pub async fn track_read(&self, path: &Path) {
        let mut read_files = self.read_files.lock().await;
        read_files.insert(self.normalize_path(path));
    }

    /// Check if a file was read (required before edit)
    pub async fn was_read(&self, path: &Path) -> bool {
        let read_files = self.read_files.lock().await;
        read_files.contains(&self.normalize_path(path))
    }

    /// Clear read tracking (called at iteration start)
    pub async fn clear_reads(&self) {
        let mut read_files = self.read_files.lock().await;
        read_files.clear();
    }

    /// Normalize a path relative to worktree
    fn normalize_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() { path.to_path_buf() } else { self.worktree.join(path) }
    }

    /// Validate path is within worktree (sandbox enforcement)
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf, ToolError> {
        let normalized = self.normalize_path(path);

        // For new files that don't exist yet, we can't canonicalize
        // So we check if the normalized path starts with the worktree
        let canonical = normalized.canonicalize().unwrap_or_else(|_| normalized.clone());

        if !self.sandbox_enabled {
            return Ok(canonical);
        }

        let worktree_canonical = self.worktree.canonicalize().map_err(|e| ToolError::IoError {
            operation: "canonicalize worktree".to_string(),
            source: e,
        })?;

        if canonical.starts_with(&worktree_canonical) {
            Ok(canonical)
        } else {
            // For new files, check if the normalized (non-canonical) path is valid
            if normalized.starts_with(&worktree_canonical) {
                Ok(normalized)
            } else {
                Err(ToolError::SandboxViolation {
                    path: path.to_path_buf(),
                    worktree: self.worktree.clone(),
                })
            }
        }
    }

    /// Get the worktree path
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }
}

/// Errors that can occur during tool execution
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Path {path} escapes worktree {worktree}")]
    SandboxViolation { path: PathBuf, worktree: PathBuf },

    #[error("File not found: {path}")]
    FileNotFound { path: String, source: std::io::Error },

    #[error("Must read file before editing: {path}")]
    EditWithoutRead { path: String },

    #[error("Command timed out after {timeout_ms}ms")]
    CommandTimeout { timeout_ms: u64 },

    #[error("Tool not found: {name}")]
    UnknownTool { name: String },

    #[error("IO error during {operation}: {source}")]
    IoError {
        operation: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Invalid input: {message}")]
    InvalidInput { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_context_creation() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "exec_001".to_string());

        assert_eq!(ctx.exec_id, "exec_001");
        assert!(ctx.sandbox_enabled);
    }

    #[tokio::test]
    async fn test_read_tracking() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "exec_001".to_string());

        let test_file = Path::new("test.txt");

        assert!(!ctx.was_read(test_file).await);

        ctx.track_read(test_file).await;
        assert!(ctx.was_read(test_file).await);

        ctx.clear_reads().await;
        assert!(!ctx.was_read(test_file).await);
    }

    #[tokio::test]
    async fn test_path_validation_inside_worktree() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "exec_001".to_string());

        // Create a test file inside the worktree
        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "content").unwrap();

        let result = ctx.validate_path(Path::new("test.txt"));
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_path_validation_outside_worktree() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "exec_001".to_string());

        let result = ctx.validate_path(Path::new("/etc/passwd"));
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[tokio::test]
    async fn test_path_validation_with_sandbox_disabled() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new_unsandboxed(dir.path().to_path_buf(), "exec_001".to_string());

        // Even paths outside worktree should be allowed
        let result = ctx.validate_path(Path::new("/tmp"));
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_normalize_relative_path() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "exec_001".to_string());

        // Create the file so validation works
        let test_file = dir.path().join("subdir").join("test.txt");
        std::fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        std::fs::write(&test_file, "content").unwrap();

        let result = ctx.validate_path(Path::new("subdir/test.txt"));
        assert!(result.is_ok());
    }
}
