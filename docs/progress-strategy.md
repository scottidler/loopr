# Progress Strategy: Cross-Iteration State for Ralph Loops

**Author:** Scott A. Idler
**Date:** 2026-01-15 (updated 2026-01-25)
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

This document specifies how Loopr accumulates and presents progress information across loop iterations. Since each iteration starts with a fresh LLM context window (the core Ralph Wiggum pattern), we must explicitly tell the LLM what happened in previous iterations. The `ProgressStrategy` trait abstracts this, with `SystemCapturedProgress` as the default implementation.

---

## Problem

Each Ralph loop iteration:
1. Starts a **new API conversation** (fresh context, no memory)
2. Reads state from **files and git** (not from previous LLM responses)
3. Attempts to complete the task
4. Runs validation
5. If validation fails, iterates again

**The problem:** Iteration 5 has no idea what iterations 1-4 tried. Without progress tracking:
- LLM repeats the same failing approach
- No learning from previous errors
- Loops spin indefinitely on the same bug

**The solution:** Capture validation output from each iteration, inject it into the next iteration's prompt.

---

## Design Decisions

### Where to Store Progress

| Option | Location | Verdict |
|--------|----------|---------|
| A. In LoopExecution record | `LoopExecution.progress` field | **CHOSEN** |
| B. Separate file per loop | `.taskstore/loops/{id}.progress.md` | Rejected |
| C. In worktree | `{worktree}/.progress.md` | Rejected |

**Rationale for A:**
- Single source of truth (TaskStore)
- Automatically persisted and recoverable
- No extra file management
- Truncation handles unbounded growth

### What to Store in Progress

| Option | Content | Verdict |
|--------|---------|---------|
| A. LLM-generated summary | "I tried X, failed because Y" | Rejected (unreliable) |
| B. System-captured output | Validation stdout/stderr | **CHOSEN (default)** |
| C. Hybrid | System capture + LLM summary | Future option |
| D. Structured log | JSON entries | Future option |

**Rationale for B:**
- Objective ground truth (LLM can't lie about exit codes)
- No extra API calls
- Sufficient for debugging
- Truncation keeps it manageable

### Abstraction: Trait-Based Strategy

To allow experimentation without code changes, progress accumulation is abstracted via a trait. Ship with one implementation (`SystemCapturedProgress`), add others later.

---

## Specification

### Trait Definition

**File:** `src/progress.rs`

```rust
use std::collections::VecDeque;

/// Context passed to progress strategy after each iteration
#[derive(Debug, Clone)]
pub struct IterationContext {
    /// Current iteration number (1-indexed)
    pub iteration: u32,
    /// The validation command that was run
    pub validation_command: String,
    /// Exit code from validation (0 = success)
    pub exit_code: i32,
    /// Stdout from validation command
    pub stdout: String,
    /// Stderr from validation command
    pub stderr: String,
    /// How long validation took
    pub duration_ms: u64,
    /// Files modified during this iteration (from git status)
    pub files_changed: Vec<String>,
}

/// Strategy for accumulating progress across loop iterations
///
/// Implementors define how iteration outcomes are recorded and formatted
/// for injection into subsequent prompts.
pub trait ProgressStrategy: Send + Sync {
    /// Record the outcome of an iteration
    ///
    /// Called after each iteration completes (pass or fail).
    /// Returns the formatted entry for this iteration.
    fn record(&mut self, ctx: &IterationContext) -> String;

    /// Get accumulated progress text for prompt injection
    ///
    /// This text is injected into the `{{progress}}` template variable.
    /// Should return empty string if no progress recorded yet.
    fn get_progress(&self) -> String;

    /// Reset all accumulated progress
    ///
    /// Called when loop is restarted or manually cleared.
    fn clear(&mut self);

    /// Number of iterations currently recorded
    fn len(&self) -> usize;

    /// Whether any progress has been recorded
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
```

### Default Implementation: SystemCapturedProgress

**File:** `src/progress.rs` (continued)

```rust
/// Default progress strategy: capture validation output verbatim
///
/// Records the raw stdout/stderr from each validation run, truncated
/// to prevent context window explosion. Keeps only the N most recent
/// iterations (older history is less relevant for debugging).
///
/// # Configuration
/// - `max_entries`: Maximum iterations to keep (default: 5)
/// - `max_output_chars`: Truncate output per iteration (default: 500)
///
/// # Example Output
/// ```markdown
/// ## Iteration 3
/// **Command:** `cargo test`
/// **Exit code:** 1
/// **Duration:** 2341ms
/// **Output:**
/// ```
/// running 8 tests
/// test auth::test_validate ... FAILED
///
/// failures:
///     auth::test_validate - assertion failed: expected Ok, got Err
/// ```
/// ```
#[derive(Debug, Clone)]
pub struct SystemCapturedProgress {
    entries: VecDeque<String>,
    max_entries: usize,
    max_output_chars: usize,
}

impl SystemCapturedProgress {
    /// Create with default settings (5 entries, 500 chars each)
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom limits
    pub fn with_limits(max_entries: usize, max_output_chars: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries),
            max_entries,
            max_output_chars,
        }
    }
}

impl Default for SystemCapturedProgress {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(5),
            max_entries: 5,
            max_output_chars: 500,
        }
    }
}

impl ProgressStrategy for SystemCapturedProgress {
    fn record(&mut self, ctx: &IterationContext) -> String {
        // Combine stdout and stderr, prefer stdout
        let output = if !ctx.stdout.is_empty() {
            &ctx.stdout
        } else {
            &ctx.stderr
        };

        // Truncate output, keeping the END (most relevant for errors)
        let truncated = if output.len() > self.max_output_chars {
            let start = output.len() - self.max_output_chars;
            format!("...[truncated]...\n{}", &output[start..])
        } else {
            output.clone()
        };

        // Format entry
        let entry = format!(
            "## Iteration {}\n\
             **Command:** `{}`\n\
             **Exit code:** {}\n\
             **Duration:** {}ms\n\
             **Files changed:** {}\n\
             **Output:**\n```\n{}\n```\n\n",
            ctx.iteration,
            ctx.validation_command,
            ctx.exit_code,
            ctx.duration_ms,
            if ctx.files_changed.is_empty() {
                "none".to_string()
            } else {
                ctx.files_changed.join(", ")
            },
            truncated.trim(),
        );

        // Add to queue, evict oldest if at capacity
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry.clone());

        entry
    }

    fn get_progress(&self) -> String {
        self.entries.iter().cloned().collect::<Vec<_>>().join("")
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}
```

---

## Integration

### In Loop Runner

**File:** `src/loop_runner.rs`

```rust
use crate::progress::{ProgressStrategy, SystemCapturedProgress, IterationContext};

pub struct LoopRunner {
    exec_id: String,
    config: LoopConfig,
    progress: Box<dyn ProgressStrategy>,
    store: Store,
    // ... other fields
}

impl LoopRunner {
    pub fn new(exec_id: String, config: LoopConfig, store: Store) -> Self {
        // Create progress strategy based on config (default: SystemCaptured)
        let progress: Box<dyn ProgressStrategy> = match config.progress_strategy.as_deref() {
            Some("system-captured") | None => {
                Box::new(SystemCapturedProgress::with_limits(
                    config.progress_max_entries.unwrap_or(5),
                    config.progress_max_chars.unwrap_or(500),
                ))
            }
            Some(other) => {
                tracing::warn!("Unknown progress strategy '{}', using default", other);
                Box::new(SystemCapturedProgress::default())
            }
        };

        Self {
            exec_id,
            config,
            progress,
            store,
        }
    }

    /// Restore progress from persisted LoopExecution record
    ///
    /// Called during crash recovery when resuming an interrupted loop.
    pub fn restore_progress(&mut self, persisted: &str) {
        // For SystemCapturedProgress, we can parse the markdown back
        // For now, just clear and let it rebuild
        // TODO: Implement proper restoration if needed
        self.progress.clear();
    }

    async fn run_iteration(&mut self, iteration: u32) -> Result<bool> {
        // ... execute LLM, apply changes ...

        // Run validation
        let start = std::time::Instant::now();
        let validation_result = self.run_validation().await?;
        let duration_ms = start.elapsed().as_millis() as u64;

        // Get changed files
        let files_changed = self.get_git_status().await?;

        // Record progress
        let ctx = IterationContext {
            iteration,
            validation_command: self.config.validation_command.clone(),
            exit_code: validation_result.exit_code,
            stdout: validation_result.stdout,
            stderr: validation_result.stderr,
            duration_ms,
            files_changed,
        };

        self.progress.record(&ctx);

        // Persist to TaskStore
        self.persist_progress().await?;

        // Return whether validation passed
        Ok(validation_result.exit_code == 0)
    }

    async fn persist_progress(&mut self) -> Result<()> {
        let mut exec: LoopExecution = self.store.get(&self.exec_id)?.unwrap();
        exec.progress = self.progress.get_progress();
        exec.updated_at = now_ms();
        self.store.update(exec)?;
        Ok(())
    }

    /// Build the prompt for this iteration
    fn build_prompt(&self, template: &str, context: &TemplateContext) -> Result<String> {
        let mut ctx = context.clone();

        // Inject progress into template context
        ctx.insert("progress".to_string(), self.progress.get_progress());

        // Render template
        render_handlebars(template, &ctx)
    }
}
```

### In Prompt Template

The `{{progress}}` variable is injected by the loop runner:

```yaml
# loopr.yml - phase loop example
phase:
  prompt-template: |
    You are implementing Phase {{phase-number}}: {{phase-name}}

    ## Spec Content
    {{spec-content}}

    {{#if progress}}
    ## Previous Iterations

    The following shows what happened in previous iterations of this loop.
    IMPORTANT: Do NOT repeat approaches that already failed.

    {{progress}}
    {{/if}}

    ## Instructions
    Implement the phase. Run validation when ready.
```

### Configuration Options

**File:** `loopr.yml`

```yaml
# Global defaults
validation:
  progress-strategy: system-captured  # Only option for now
  progress-max-entries: 5             # Keep last 5 iterations
  progress-max-chars: 500             # Truncate output to 500 chars

# Per-loop-type override
phase:
  progress-max-entries: 10            # More history for complex phases
  progress-max-chars: 1000            # More output for test failures
```

---

## Future Strategies

These are NOT implemented now. Documented for future extension.

### LlmSummarizedProgress (Option A)

```rust
/// Uses a cheap LLM call to summarize each iteration
///
/// Pros: More semantic, less noise
/// Cons: Extra API cost, latency, LLM can hallucinate
pub struct LlmSummarizedProgress {
    llm: Arc<dyn LlmClient>,
    summaries: VecDeque<String>,
    max_entries: usize,
}

impl ProgressStrategy for LlmSummarizedProgress {
    fn record(&mut self, ctx: &IterationContext) -> String {
        // Call LLM to summarize
        let prompt = format!(
            "Summarize this iteration result in 1-2 sentences:\n\
             Exit code: {}\nOutput:\n{}",
            ctx.exit_code, ctx.stdout
        );

        let summary = self.llm.complete_sync(&prompt)?;  // Blocking for simplicity
        self.summaries.push_back(summary.clone());
        // ... truncation logic
        summary
    }
    // ...
}
```

### HybridProgress (Option C)

```rust
/// Captures raw output AND generates LLM summary
///
/// Best of both: objective data + semantic understanding
pub struct HybridProgress {
    raw: SystemCapturedProgress,
    summaries: LlmSummarizedProgress,
}

impl ProgressStrategy for HybridProgress {
    fn get_progress(&self) -> String {
        format!(
            "## Summary\n{}\n\n## Raw Output\n{}",
            self.summaries.get_progress(),
            self.raw.get_progress()
        )
    }
    // ...
}
```

### StructuredLogProgress (Option D)

```rust
/// JSON-structured progress for machine parsing
///
/// Useful if prompt template wants to process progress programmatically
pub struct StructuredLogProgress {
    entries: VecDeque<serde_json::Value>,
    max_entries: usize,
}

impl ProgressStrategy for StructuredLogProgress {
    fn record(&mut self, ctx: &IterationContext) -> String {
        let entry = serde_json::json!({
            "iteration": ctx.iteration,
            "exit_code": ctx.exit_code,
            "duration_ms": ctx.duration_ms,
            "files_changed": ctx.files_changed,
            "error_summary": extract_first_error(&ctx.stdout),
        });

        self.entries.push_back(entry.clone());
        // ... truncation
        serde_json::to_string_pretty(&entry).unwrap()
    }

    fn get_progress(&self) -> String {
        serde_json::to_string_pretty(&self.entries).unwrap()
    }
    // ...
}
```

---

## Testing

### Unit Tests

**File:** `src/progress.rs` (tests module)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(iteration: u32, exit_code: i32, output: &str) -> IterationContext {
        IterationContext {
            iteration,
            validation_command: "cargo test".to_string(),
            exit_code,
            stdout: output.to_string(),
            stderr: String::new(),
            duration_ms: 1000,
            files_changed: vec!["src/lib.rs".to_string()],
        }
    }

    #[test]
    fn test_system_captured_records_iteration() {
        let mut progress = SystemCapturedProgress::default();

        let entry = progress.record(&make_ctx(1, 1, "test failed"));

        assert!(entry.contains("Iteration 1"));
        assert!(entry.contains("Exit code: 1"));
        assert!(entry.contains("test failed"));
        assert_eq!(progress.len(), 1);
    }

    #[test]
    fn test_system_captured_truncates_entries() {
        let mut progress = SystemCapturedProgress::with_limits(3, 100);

        for i in 1..=5 {
            progress.record(&make_ctx(i, 1, &format!("error {}", i)));
        }

        // Should only keep last 3
        assert_eq!(progress.len(), 3);
        let text = progress.get_progress();
        assert!(!text.contains("Iteration 1"));
        assert!(!text.contains("Iteration 2"));
        assert!(text.contains("Iteration 3"));
        assert!(text.contains("Iteration 4"));
        assert!(text.contains("Iteration 5"));
    }

    #[test]
    fn test_system_captured_truncates_output() {
        let mut progress = SystemCapturedProgress::with_limits(5, 50);

        let long_output = "x".repeat(200);
        progress.record(&make_ctx(1, 1, &long_output));

        let text = progress.get_progress();
        assert!(text.contains("[truncated]"));
        assert!(text.len() < 500); // Much smaller than 200 char output
    }

    #[test]
    fn test_system_captured_clear() {
        let mut progress = SystemCapturedProgress::default();

        progress.record(&make_ctx(1, 0, "ok"));
        progress.record(&make_ctx(2, 0, "ok"));
        assert_eq!(progress.len(), 2);

        progress.clear();
        assert_eq!(progress.len(), 0);
        assert!(progress.get_progress().is_empty());
    }

    #[test]
    fn test_empty_progress_returns_empty_string() {
        let progress = SystemCapturedProgress::default();
        assert!(progress.get_progress().is_empty());
        assert!(progress.is_empty());
    }
}
```

### Integration Test

**File:** `tests/progress_integration.rs`

```rust
use loopr::progress::{ProgressStrategy, SystemCapturedProgress, IterationContext};

#[test]
fn test_progress_survives_serialization() {
    let mut progress = SystemCapturedProgress::default();

    progress.record(&IterationContext {
        iteration: 1,
        validation_command: "make test".to_string(),
        exit_code: 1,
        stdout: "FAILED: test_foo".to_string(),
        stderr: String::new(),
        duration_ms: 500,
        files_changed: vec![],
    });

    // Simulate persisting to TaskStore and restoring
    let persisted = progress.get_progress();

    // In real code, this would be stored in LoopExecution.progress
    // and restored on crash recovery
    assert!(persisted.contains("Iteration 1"));
    assert!(persisted.contains("FAILED: test_foo"));
}
```

---

## Checklist for Implementer

- [ ] Create `src/progress.rs` with trait and default impl
- [ ] Add `progress` field to `LoopExecution` domain type (already exists)
- [ ] Integrate `ProgressStrategy` into `LoopRunner`
- [ ] Add `{{progress}}` to all loop type prompt templates
- [ ] Add progress config options to `LoopConfig`
- [ ] Write unit tests
- [ ] Write integration test with real loop execution

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and storage
- [loop-validation.md](loop-validation.md) - Validation and feedback
- [loop-config.md](loop-config.md) - Configuration options
- [rule-of-five.md](rule-of-five.md) - Plan refinement methodology
