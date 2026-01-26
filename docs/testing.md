# Testing Strategy

**Version:** 1.0
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

Loopr uses a lib.rs + thin CLI architecture to maximize testability. Most logic lives in the library crate, with main.rs providing only argument parsing and process setup. Target: 70%+ unit test coverage.

---

## Architecture for Testability

### Lib.rs Contains Everything

```
loopr/
├── Cargo.toml
├── src/
│   ├── main.rs           # THIN: arg parsing, terminal setup, run lib
│   ├── lib.rs            # Public API, re-exports
│   │
│   ├── store/            # All testable
│   │   ├── mod.rs
│   │   ├── task_store.rs
│   │   └── records.rs
│   │
│   ├── llm/              # All testable (with mocks)
│   │   ├── mod.rs
│   │   ├── client.rs
│   │   ├── anthropic.rs
│   │   └── tools/
│   │
│   ├── loops/            # All testable
│   │   ├── mod.rs
│   │   ├── ralph.rs
│   │   ├── phase.rs
│   │   ├── spec.rs
│   │   ├── plan.rs
│   │   └── manager.rs
│   │
│   ├── scheduler/        # All testable
│   ├── validation/       # All testable
│   ├── tui/              # Testable (state, not rendering)
│   └── config/           # All testable
│
└── tests/
    ├── store_tests.rs
    ├── llm_tests.rs
    ├── loop_tests.rs
    └── integration/
        └── full_plan_test.rs
```

### Thin main.rs

```rust
// src/main.rs - KEEP THIS MINIMAL
use clap::Parser;
use loopr::{run, Config, Result};

#[derive(Parser)]
#[command(name = "loopr", version, about)]
struct Cli {
    /// Config file path
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Setup logging
    tracing_subscriber::init();

    // Load config (testable in lib)
    let config = Config::load(cli.config)?;

    // Run the app (testable in lib)
    run(config).await
}
```

### Public lib.rs API

```rust
// src/lib.rs
pub mod config;
pub mod errors;
pub mod llm;
pub mod loops;
pub mod scheduler;
pub mod store;
pub mod tui;
pub mod validation;

pub use config::Config;
pub use errors::{LooprError, Result};

/// Main entry point - fully testable
pub async fn run(config: Config) -> Result<()> {
    let store = TaskStore::open(&config.storage)?;
    let llm = AnthropicClient::from_config(&config.llm)?;
    let manager = LoopManager::new(store, llm, config);

    // Start TUI or headless mode
    if config.headless {
        manager.run_headless().await
    } else {
        tui::run(manager).await
    }
}
```

---

## Coverage Target: 70%+

### Measuring Coverage

```bash
# Install coverage tool
cargo install cargo-tarpaulin

# Run with coverage
cargo tarpaulin --out Html --output-dir coverage/

# View report
open coverage/tarpaulin-report.html
```

### Coverage by Component

| Component | Target | Rationale |
|-----------|--------|-----------|
| `store/` | 90%+ | Core data layer, must be rock-solid |
| `loops/` | 80%+ | Business logic, many edge cases |
| `scheduler/` | 85%+ | Priority logic, dependency resolution |
| `validation/` | 80%+ | Critical for correctness |
| `llm/` | 70%+ | API boundaries, mocks needed |
| `tui/` | 50%+ | State logic testable, rendering less so |
| `config/` | 90%+ | Parse/validate logic |

### What to Test

**Always test:**
- Public API functions
- State transitions
- Error handling paths
- Edge cases (empty input, max values, invalid data)
- Serialization/deserialization

**Don't bother testing:**
- Simple getters/setters
- Trivial constructors
- Framework-generated code (derives)
- TUI rendering (visual inspection instead)

---

## Test Organization

### Unit Tests (in-file)

Co-locate unit tests with implementation:

```rust
// src/scheduler/priority.rs

pub fn calculate_priority(record: &LoopRecord) -> i32 {
    // ... implementation ...
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ralph_has_highest_base_priority() {
        let ralph = LoopRecord::new_ralph("test");
        let plan = LoopRecord::new_plan("test");

        assert!(calculate_priority(&ralph) > calculate_priority(&plan));
    }

    #[test]
    fn test_age_boost_increases_priority() {
        let mut record = LoopRecord::new_ralph("test");
        let initial = calculate_priority(&record);

        record.created_at -= 60_000; // 1 minute older
        let aged = calculate_priority(&record);

        assert!(aged > initial);
    }

    #[test]
    fn test_retry_penalty_decreases_priority() {
        let mut record = LoopRecord::new_ralph("test");
        let initial = calculate_priority(&record);

        record.iteration = 5;
        let retried = calculate_priority(&record);

        assert!(retried < initial);
    }
}
```

### Integration Tests (tests/ directory)

Test component interactions:

```rust
// tests/loop_lifecycle_test.rs

use loopr::{LoopManager, LoopRecord, TaskStore, MockLlmClient};

#[tokio::test]
async fn test_loop_runs_to_completion() {
    // Setup
    let store = TaskStore::in_memory();
    let llm = MockLlmClient::new(vec![
        mock_response("I'll implement this..."),
        mock_tool_call("write_file", json!({"path": "test.rs", "content": "fn main() {}"})),
        mock_tool_call("complete_task", json!({"summary": "Done"})),
    ]);
    let config = Config::default();

    // Create loop
    let record = LoopRecord::new_ralph("Write a hello world");
    store.create(&record)?;

    // Run manager
    let manager = LoopManager::new(store.clone(), llm, config);
    manager.tick().await?;

    // Verify completion
    let updated: LoopRecord = store.get(&record.id)?.unwrap();
    assert_eq!(updated.status, LoopStatus::Complete);
}

#[tokio::test]
async fn test_validation_failure_triggers_reiteration() {
    let store = TaskStore::in_memory();
    let llm = MockLlmClient::new(vec![
        // First iteration - fails validation
        mock_response("Attempt 1"),
        // Second iteration - passes
        mock_response("Attempt 2"),
    ]);

    // Configure validation to fail first, pass second
    let config = Config {
        validation_command: "test $ITERATION -gt 1".to_string(),
        ..Default::default()
    };

    let record = LoopRecord::new_ralph("Test task");
    store.create(&record)?;

    let manager = LoopManager::new(store.clone(), llm, config);

    // First tick - validation fails
    manager.tick().await?;
    let r1: LoopRecord = store.get(&record.id)?.unwrap();
    assert_eq!(r1.iteration, 1);
    assert_eq!(r1.status, LoopStatus::Running);

    // Second tick - validation passes
    manager.tick().await?;
    let r2: LoopRecord = store.get(&record.id)?.unwrap();
    assert_eq!(r2.iteration, 2);
    assert_eq!(r2.status, LoopStatus::Complete);
}
```

### Mocking Strategy

#### LLM Client Mock

```rust
// src/llm/mock.rs

pub struct MockLlmClient {
    responses: Vec<CompletionResponse>,
    call_count: AtomicUsize,
}

impl MockLlmClient {
    pub fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses,
            call_count: AtomicUsize::new(0),
        }
    }

    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        self.responses.get(idx)
            .cloned()
            .ok_or_else(|| eyre!("MockLlmClient: No more responses"))
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        _chunk_tx: mpsc::Sender<StreamChunk>,
    ) -> Result<CompletionResponse> {
        self.complete(request).await
    }
}
```

#### In-Memory TaskStore

```rust
// src/store/mod.rs

impl TaskStore {
    /// Create in-memory store for testing
    pub fn in_memory() -> Self {
        Self {
            jsonl_path: None,
            sqlite_path: None,
            records: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}
```

#### Tool Context Sandbox

```rust
// tests/tools_test.rs

#[tokio::test]
async fn test_sandbox_prevents_escape() {
    let temp = tempfile::tempdir()?;
    let worktree = temp.path().join("worktree");
    std::fs::create_dir(&worktree)?;

    let ctx = ToolContext::new(worktree.clone(), "test-exec".to_string());

    // Attempt to escape sandbox
    let result = ctx.validate_path(Path::new("/etc/passwd"));

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err().downcast_ref::<ToolError>(),
        Some(ToolError::SandboxViolation { .. })
    ));
}
```

---

## Test Patterns

### Table-Driven Tests

```rust
#[test]
fn test_status_transitions() {
    let cases = vec![
        // (from, to, expected_valid)
        (LoopStatus::Pending, LoopStatus::Running, true),
        (LoopStatus::Running, LoopStatus::Complete, true),
        (LoopStatus::Running, LoopStatus::Pending, false), // Invalid
        (LoopStatus::Complete, LoopStatus::Running, false), // Terminal
    ];

    for (from, to, expected) in cases {
        let result = validate_transition(from, to);
        assert_eq!(
            result.is_ok(),
            expected,
            "Transition {:?} -> {:?} should be {}",
            from,
            to,
            if expected { "valid" } else { "invalid" }
        );
    }
}
```

### Property-Based Tests

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_priority_always_positive(
        loop_type in prop_oneof![
            Just(LoopType::Plan),
            Just(LoopType::Spec),
            Just(LoopType::Phase),
            Just(LoopType::Ralph),
        ],
        age_ms in 0u64..86_400_000,  // Up to 1 day
        iteration in 0u32..100,
    ) {
        let mut record = LoopRecord::new(loop_type, "test");
        record.created_at = now_ms() - age_ms as i64;
        record.iteration = iteration;

        let priority = calculate_priority(&record);
        prop_assert!(priority > 0, "Priority should always be positive");
    }
}
```

### Async Test Utilities

```rust
// tests/common/mod.rs

/// Run async test with timeout
pub async fn with_timeout<F, T>(duration: Duration, f: F) -> T
where
    F: Future<Output = T>,
{
    tokio::time::timeout(duration, f)
        .await
        .expect("Test timed out")
}

/// Create test fixtures
pub fn test_loop_record(loop_type: LoopType) -> LoopRecord {
    LoopRecord {
        id: format!("test-{}", rand::random::<u32>()),
        loop_type,
        status: LoopStatus::Pending,
        parent_loop: None,
        triggered_by: None,
        conversation_id: None,
        iteration: 0,
        max_iterations: 10,
        progress: String::new(),
        context: serde_json::json!({}),
        created_at: now_ms(),
        updated_at: now_ms(),
    }
}
```

---

## CI Configuration

```yaml
# .github/workflows/test.yml
name: Test

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Run tests
        run: cargo test --all-features

      - name: Run coverage
        run: |
          cargo install cargo-tarpaulin
          cargo tarpaulin --out Xml --output-dir coverage/

      - name: Upload coverage
        uses: codecov/codecov-action@v4
        with:
          files: coverage/cobertura.xml
          fail_ci_if_error: true
          threshold: 70%  # Fail if below 70%
```

---

## Test Commands

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_priority_calculation

# Run tests with output
cargo test -- --nocapture

# Run tests in specific module
cargo test store::

# Run integration tests only
cargo test --test '*'

# Run with coverage
cargo tarpaulin --out Html

# Run benchmarks (if any)
cargo bench
```

---

## Dependencies

```toml
[dev-dependencies]
tokio-test = "0.4"
tempfile = "3"
proptest = "1"
criterion = "0.5"  # For benchmarks
```

---

## References

- [errors.md](errors.md) - Error handling (test error paths)
- [domain-types.md](domain-types.md) - Data structures to test
- [llm-client.md](llm-client.md) - MockLlmClient examples
