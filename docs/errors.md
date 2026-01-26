# Error Handling Strategy

**Version:** 1.0
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

Loopr uses a layered error handling strategy: propagate with `?` by default, use `eyre!` for context-rich messages at boundaries, and provide user-friendly messages in the TUI. All errors are recoverable through TaskStore persistence.

---

## Error Handling Philosophy

### Prefer `?` Propagation

Use the `?` operator for error propagation in most code:

```rust
// PREFERRED: Clean propagation
fn load_artifact(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path)?;
    Ok(content)
}
```

### Use `eyre!` at Boundaries

Use `eyre!` when adding context at module boundaries or when the error message needs domain-specific information:

```rust
// Use eyre! when context is essential
fn load_loop_record(id: &str) -> Result<LoopRecord> {
    let record = store.get(id)?
        .ok_or_else(|| eyre!("Loop {} not found in TaskStore", id))?;
    Ok(record)
}

// Use .context() for adding context to existing errors
fn create_worktree(loop_id: &str) -> Result<PathBuf> {
    Command::new("git")
        .args(["worktree", "add", ...])
        .status()
        .context(format!("Failed to create worktree for loop {}", loop_id))?;
    Ok(path)
}
```

### Error Categories

```rust
/// Top-level error categories
#[derive(Debug, thiserror::Error)]
pub enum LooprError {
    /// Storage errors (TaskStore, filesystem)
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    /// LLM API errors (rate limits, network, invalid response)
    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    /// Tool execution errors (file ops, commands)
    #[error("Tool error: {0}")]
    Tool(#[from] ToolError),

    /// Loop lifecycle errors (invalid state transitions)
    #[error("Loop error: {0}")]
    Loop(#[from] LoopError),

    /// Configuration errors (missing fields, invalid values)
    #[error("Config error: {0}")]
    Config(#[from] ConfigError),

    /// Git errors (worktree, merge, branch)
    #[error("Git error: {0}")]
    Git(#[from] GitError),
}
```

---

## Error Types by Component

### StorageError

```rust
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("Record not found: {id}")]
    NotFound { id: String },

    #[error("Record already exists: {id}")]
    AlreadyExists { id: String },

    #[error("JSONL parse error at line {line}: {message}")]
    ParseError { line: usize, message: String },

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl StorageError {
    /// Whether this error is recoverable by retry
    pub fn is_retryable(&self) -> bool {
        matches!(self, StorageError::Io(_))
    }
}
```

### LlmError

```rust
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("Rate limited, retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },

    #[error("API error {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Context limit exceeded: {used} tokens > {limit} max")]
    ContextOverflow { used: usize, limit: usize },

    #[error("Timeout after {timeout:?}")]
    Timeout { timeout: Duration },
}

impl LlmError {
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, LlmError::RateLimited { .. })
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::RateLimited { .. } => true,
            LlmError::ApiError { status, .. } => *status >= 500,
            LlmError::Network(_) => true,
            LlmError::Timeout { .. } => true,
            _ => false,
        }
    }

    pub fn retry_delay(&self) -> Option<Duration> {
        match self {
            LlmError::RateLimited { retry_after } => Some(*retry_after),
            LlmError::ApiError { status, .. } if *status >= 500 => {
                Some(Duration::from_secs(5))
            }
            LlmError::Network(_) => Some(Duration::from_secs(2)),
            LlmError::Timeout { .. } => Some(Duration::from_secs(1)),
            _ => None,
        }
    }
}
```

### ToolError

```rust
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Path {path:?} escapes worktree sandbox {worktree:?}")]
    SandboxViolation { path: PathBuf, worktree: PathBuf },

    #[error("File not found: {path}")]
    FileNotFound { path: String },

    #[error("Must read file before editing: {path}")]
    EditWithoutRead { path: String },

    #[error("Command timed out after {timeout_ms}ms")]
    CommandTimeout { timeout_ms: u64 },

    #[error("Command failed with exit code {exit_code}: {stderr}")]
    CommandFailed { exit_code: i32, stderr: String },

    #[error("Tool not found: {name}")]
    UnknownTool { name: String },

    #[error("Invalid tool input: {message}")]
    InvalidInput { message: String },
}
```

### LoopError

```rust
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("Invalid state transition: {from:?} -> {to:?}")]
    InvalidTransition { from: LoopStatus, to: LoopStatus },

    #[error("Max iterations ({max}) reached without validation passing")]
    MaxIterations { max: u32 },

    #[error("Parent loop {parent_id} not found")]
    ParentNotFound { parent_id: String },

    #[error("Triggering artifact not found: {path}")]
    ArtifactNotFound { path: String },

    #[error("Loop {id} was invalidated: {reason}")]
    Invalidated { id: String, reason: String },

    #[error("Validation failed: {message}")]
    ValidationFailed { message: String },
}
```

### GitError

```rust
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("Failed to create worktree: {message}")]
    WorktreeCreation { message: String },

    #[error("Failed to remove worktree: {message}")]
    WorktreeRemoval { message: String },

    #[error("Merge conflict in files: {files:?}")]
    MergeConflict { files: Vec<String> },

    #[error("Branch {branch} not found")]
    BranchNotFound { branch: String },

    #[error("Git command failed: {command} - {stderr}")]
    CommandFailed { command: String, stderr: String },
}
```

### ConfigError

```rust
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Config file not found: {path:?}")]
    FileNotFound { path: PathBuf },

    #[error("Invalid config: {message}")]
    Invalid { message: String },

    #[error("Missing required field: {field}")]
    MissingField { field: String },

    #[error("Invalid value for {field}: {value} (expected {expected})")]
    InvalidValue { field: String, value: String, expected: String },

    #[error("Environment variable {var} not set")]
    EnvNotSet { var: String },
}
```

---

## Recovery Strategies

### Automatic Retry

For transient errors, implement automatic retry with exponential backoff:

```rust
async fn with_retry<T, F, Fut>(
    operation: F,
    max_attempts: u32,
    base_delay: Duration,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempts = 0;
    loop {
        attempts += 1;
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) if is_retryable(&e) && attempts < max_attempts => {
                let delay = base_delay * 2u32.pow(attempts - 1);
                tracing::warn!(
                    error = %e,
                    attempt = attempts,
                    delay_ms = delay.as_millis(),
                    "Retrying after transient error"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}
```

### Crash Recovery

Loops persist state to TaskStore after each significant action:

```rust
// State saved after each iteration
async fn run_iteration(&mut self) -> Result<()> {
    // ... execute iteration ...

    // Persist state before validation (survives crash during validation)
    self.persist_progress().await?;

    // Run validation
    let result = self.validate().await;

    // Persist result (survives crash after validation)
    self.persist_validation_result(result).await?;

    Ok(())
}
```

On restart, the LoopManager recovers interrupted loops:

```rust
async fn recover_loops(&self) -> Result<()> {
    let running = self.store.query::<LoopRecord>(&[
        Filter::eq("status", "running"),
    ])?;

    for record in running {
        if self.worktree_exists(&record.id) {
            // Resume from last persisted state
            self.spawn_loop(record).await?;
        } else {
            // Worktree lost - mark as failed
            self.mark_failed(&record.id, "Worktree lost during crash").await?;
        }
    }

    Ok(())
}
```

### Graceful Degradation

When non-critical components fail, continue with reduced functionality:

```rust
// Example: SQLite cache rebuild
fn ensure_cache(&self) -> Result<()> {
    match self.rebuild_sqlite_cache() {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, "SQLite cache rebuild failed, using JSONL directly");
            self.use_jsonl_fallback = true;
            Ok(()) // Continue without cache
        }
    }
}
```

---

## User-Facing Messages

### TUI Error Display

Format errors for user consumption in the TUI:

```rust
impl LooprError {
    /// Human-readable message for TUI display
    pub fn user_message(&self) -> String {
        match self {
            LooprError::Llm(LlmError::RateLimited { retry_after }) => {
                format!("API rate limited. Retrying in {}s...", retry_after.as_secs())
            }
            LooprError::Llm(LlmError::Network(_)) => {
                "Network error. Check your internet connection.".to_string()
            }
            LooprError::Tool(ToolError::SandboxViolation { path, .. }) => {
                format!("Security: Cannot access {} (outside worktree)", path.display())
            }
            LooprError::Loop(LoopError::MaxIterations { max }) => {
                format!("Loop failed after {} attempts. Consider adjusting the task.", max)
            }
            LooprError::Config(ConfigError::EnvNotSet { var }) => {
                format!("Missing environment variable: {}. Set it and restart.", var)
            }
            _ => self.to_string(),
        }
    }

    /// Whether to show detailed error (debug info)
    pub fn show_details(&self) -> bool {
        match self {
            LooprError::Llm(LlmError::ApiError { .. }) => true,
            LooprError::Tool(ToolError::CommandFailed { .. }) => true,
            LooprError::Git(GitError::MergeConflict { .. }) => true,
            _ => false,
        }
    }
}
```

### Logging Levels

```rust
// Error: Requires user attention
tracing::error!(loop_id = %id, error = %e, "Loop failed permanently");

// Warn: Automatic recovery in progress
tracing::warn!(loop_id = %id, error = %e, attempt = n, "Retrying after error");

// Info: Normal operation, notable events
tracing::info!(loop_id = %id, iteration = n, "Validation passed");

// Debug: Detailed execution trace
tracing::debug!(loop_id = %id, tool = %name, "Executing tool");

// Trace: Very verbose, performance-sensitive paths
tracing::trace!(loop_id = %id, "Polling TaskStore");
```

---

## Error Handling Checklist

When adding new error-prone code:

- [ ] Define specific error variants (not generic strings)
- [ ] Implement `is_retryable()` for transient errors
- [ ] Add context with `.context()` at module boundaries
- [ ] Persist state before risky operations (for crash recovery)
- [ ] Provide user-friendly message for TUI display
- [ ] Log with appropriate level and structured fields
- [ ] Test error paths, not just happy paths

---

## Dependencies

```toml
[dependencies]
thiserror = "2"
eyre = "0.6"
tracing = "0.1"
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop lifecycle
- [execution-model.md](execution-model.md) - Crash recovery
- [llm-client.md](llm-client.md) - LLM error handling
- [tools.md](tools.md) - Tool error handling
