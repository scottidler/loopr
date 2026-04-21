use std::path::PathBuf;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::denylist::BashDenylist;
use crate::error::ToolError;
use crate::lane::Lane;
use crate::router::LaneRouter;
use crate::sandbox::SandboxMode;
use crate::schema::ToolSchema;

pub trait Tool: Sized + Send + Sync {
    type Input: for<'de> Deserialize<'de> + JsonSchema + Send;
    type Output: Serialize + Send;
    type Error: Into<ToolError> + Send;

    fn name() -> &'static str;
    fn description() -> &'static str;
    fn lane() -> Lane;
    fn schema() -> ToolSchema;

    fn execute(
        input: Self::Input,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send;
}

/// Per-invocation state threaded into every tool call.
///
/// Phase 1 carries the subset that does not depend on Phase 2 types
/// (`LaneRouter`, `BashDenylist`); Phase 2 extends this struct with
/// `router: Arc<LaneRouter>` and `bash_denylist: Arc<BashDenylist>`.
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub router: Arc<LaneRouter>,
    pub sandbox: SandboxMode,
    pub path_deny_patterns: Vec<String>,
    pub bash_denylist: Arc<BashDenylist>,
    /// Base directory for persisting subprocess output when inline-truncation
    /// fires. Agents set `Some(.loopr/runs/<run-id>/work/<work-id>/)`; unit
    /// tests leave `None` (spawn falls back to
    /// `std::env::temp_dir().join("loopr-tool-output/")`).
    pub persist_base: Option<PathBuf>,
    /// Unique identifier for this tool invocation. Used to name the
    /// persist-overflow file (`<invocation_id>.log`). `None` in unit tests —
    /// spawn synthesizes a timestamp-based fallback. Explicit field, **not**
    /// read from `tracing::Span::current()` (Architect R1).
    pub invocation_id: Option<Uuid>,
}
