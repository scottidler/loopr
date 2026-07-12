#![allow(clippy::unwrap_used)]
//! Phase 2 regression: the restart-budget healthy-run reset must apply
//! on the FINAL-restart arm too, not only while `restart < max_restarts`.
//! Pulled into its own submodule (mirroring `operator.rs`) to keep the
//! parent `tests.rs` under the 1500-line bloat-task cap.
//!
//! `use super::*;` imports the scaffolding (`FakeLlm`, `FakeStore`,
//! `RecordingSpawner`, `make_work`, `fast_config`, `make_deps`, etc.)
//! from the parent `tests` module via submodule privilege.

use std::sync::Arc;
use std::sync::Mutex;

use serde_json::json;
use tokio::sync::Notify;

use domain::{PlanId, WorkStatus};
use llm::{LlmClient, LlmError, Message, RetryableReason, ToolCall, ToolSchema as LlmToolSchema, Usage};

use super::{FakeStore, RecordingSpawner, fast_config, make_deps, make_work};
use crate::director::run_director;

/// `FakeLlm` variant that returns `Err` on specific 1-based call numbers
/// and a fixed `{action: done}` response otherwise. The queue-based
/// `FakeLlm` in the parent module only ever returns `Ok`, so it cannot
/// drive `run_director`'s outer restart dispatcher through a real
/// `LlmError` — which is exactly what this regression needs to exercise
/// the healthy-run restart-budget reset.
struct FlakyLlm {
    call_count: Mutex<u32>,
    error_at: Vec<u32>,
    /// Mirrors `FakeLlm::with_shutdown_after`: fire `notify.notify_one()`
    /// once `call_count` reaches `threshold`, so the test can end the
    /// (otherwise-infinite) Director loop deterministically once it has
    /// proven it survived past the budget-exhaustion point.
    shutdown_after: Option<(Arc<Notify>, u32)>,
}

impl FlakyLlm {
    fn new(error_at: Vec<u32>) -> Self {
        Self {
            call_count: Mutex::new(0),
            error_at,
            shutdown_after: None,
        }
    }

    fn with_shutdown_after(mut self, notify: Arc<Notify>, calls: u32) -> Self {
        self.shutdown_after = Some((notify, calls));
        self
    }

    fn calls(&self) -> u32 {
        *self.call_count.lock().unwrap()
    }
}

impl LlmClient for FlakyLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: LlmToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        panic!("FlakyLlm: complete_with_tool not used in Director tests")
    }

    async fn complete_free(
        &self,
        _system: &str,
        _messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        let count = {
            let mut c = self.call_count.lock().unwrap();
            *c += 1;
            *c
        };
        if let Some((notify, threshold)) = &self.shutdown_after
            && count >= *threshold
        {
            notify.notify_one();
        }
        if self.error_at.contains(&count) {
            return Err(LlmError::Retryable {
                reason: RetryableReason::ServerError { status: 503 },
            });
        }
        Ok((
            json!([{ "action": "done", "summary": "ok" }]).to_string(),
            Usage::default(),
        ))
    }
}

/// Break-to-prove: on pre-fix code the healthy-run reset only fired
/// inside the `restart < max_restarts` guard. With `max_restarts = 1`:
///
/// - Restart attempt 1 fails immediately (call #1; 0 healthy
///   iterations). `0 < 1` -> budget check applies, no reset (0 < 10
///   healthy iters), `restart` becomes 1.
/// - Restart attempt 2 runs 10 healthy iterations (calls #2-#11) then
///   fails (call #12). Now `restart == max_restarts` (`1 == 1`): the
///   OLD guard (`restart < max_restarts`) is false here, so pre-fix
///   code fell straight to `Err(e) => return Err(e)` and the Director
///   died on its very next transient blip despite 10 healthy
///   iterations — exactly the bug bullet 7 exists to prevent. The FIXED
///   guard also matches when `iterations_completed >=
///   HEALTHY_ITERS_BEFORE_RESTART_RESET`, so the budget resets and a
///   3rd restart attempt begins instead of the loop terminating.
/// - Restart attempt 3's first call (#13) succeeds and fires the
///   shutdown notify, so the test observes a clean `Ok(())` exit rather
///   than the `Err` a pre-fix run would have returned at call #12.
#[tokio::test]
async fn run_director_healthy_run_resets_budget_on_final_restart_arm() {
    let plan_id = PlanId::new();
    let pending = make_work(plan_id.clone(), "wk-pending", WorkStatus::Pending);
    let store = FakeStore::with(vec![pending], vec![]);
    let shutdown = Arc::new(Notify::new());

    let llm = FlakyLlm::new(vec![1, 12]).with_shutdown_after(shutdown.clone(), 13);
    let spawner = Arc::new(RecordingSpawner::default());
    let mut config = fast_config();
    config.max_restarts = 1;
    let deps = make_deps(llm, store, spawner, config, shutdown.clone());

    run_director(&plan_id, &deps)
        .await
        .expect("director must survive the 2nd (final-restart-arm) failure and exit cleanly on shutdown");

    assert!(
        deps.llm.calls() >= 13,
        "expected the Director to reach a 3rd restart attempt (>= 13 LLM calls); got {}",
        deps.llm.calls()
    );
}
