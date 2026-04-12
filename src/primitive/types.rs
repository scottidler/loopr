use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Declares a named, typed output field that a primitive produces.
/// Used for startup validation of $context references between primitives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputField {
    pub name: String,
    pub field_type: OutputType,
}

/// Types that primitive inputs and outputs can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputType {
    String,
    U32,
    U64,
    F64,
    Bool,
    StringArray,
    /// Opaque JSON for complex/variable-shape outputs.
    Json,
}

impl OutputType {
    /// Returns true when a value of type `self` can be wired into a slot
    /// expecting type `target`. Currently strict equality; widen later if
    /// needed (e.g. U32 -> U64 promotion).
    pub fn compatible_with(&self, target: &OutputType) -> bool {
        self == target
    }
}

/// Declares a named, typed input parameter that a primitive accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputField {
    pub name: String,
    pub field_type: OutputType,
    pub required: bool,
}

/// The result of executing a primitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimitiveOutput {
    /// Named, typed outputs that subsequent primitives can reference.
    /// Keys must match the names declared in output_schema().
    pub values: HashMap<String, serde_json::Value>,
    /// Human-readable summary for logging/TUI.
    pub summary: String,
}

/// Context available to every primitive during execution.
pub struct PrimitiveContext<'a> {
    pub stores: &'a Stores,
    pub bridge: &'a AgentIpcBridge,
    pub event_tx: &'a broadcast::Sender<DaemonEvent>,
    pub repo_path: &'a Path,
    pub worktree_mgr: &'a WorktreeManager,
    /// Strategy-scoped scratchpad for inter-primitive communication.
    pub strategy_ctx: &'a mut HashMap<String, serde_json::Value>,
}

/// Idempotency guarantee for a primitive.
///
/// Strategies can partially execute before a crash; the next tick
/// re-evaluates triggers and may re-invoke primitives that already ran.
/// Primitives must document their behavior on re-execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Idempotency {
    /// Safe to call multiple times with same params. No duplicate side effects.
    Idempotent,
    /// Safe if a guard condition is checked first (e.g., "record doesn't already exist").
    GuardRequired,
    /// NOT safe to re-call. Must be last in action sequence or protected by cooldown.
    NonIdempotent,
}

/// Every primitive implements this trait.
///
/// Future enhancement: consider splitting into Primitive (side-effecting, takes &mut ctx)
/// and QueryPrimitive (pure read, takes &ctx). This would let the engine run multiple
/// queries concurrently within a strategy's "gather" phase, since &ctx doesn't require
/// exclusive access. Not worth the complexity yet - start with one trait, split when
/// query parallelism becomes a measurable bottleneck.
pub trait Primitive: Send + Sync {
    /// Unique name used in YAML references (e.g., "spawn-agent").
    fn name(&self) -> &'static str;

    /// Execute the primitive.
    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>>;

    /// Declare the typed output fields this primitive produces.
    /// Used at startup to validate $context references between primitives:
    /// if step B references $context.step-A.session-id, the engine verifies
    /// that step A's primitive declares an output named "session-id" of a
    /// compatible type with step B's expected input type.
    fn output_schema(&self) -> Vec<OutputField>;

    /// Declare the typed input params this primitive accepts.
    /// Used at startup to validate strategy YAML params and $context type
    /// compatibility. Each InputField has a name, type, and required flag.
    fn input_schema(&self) -> Vec<InputField>;

    /// Validate params at startup (before any work starts).
    /// Default implementation checks params against input_schema().
    fn validate_params(&self, params: &serde_json::Value) -> eyre::Result<()> {
        let obj = params.as_object();
        for field in self.input_schema() {
            if field.required {
                let present = obj.map(|o| o.contains_key(&field.name)).unwrap_or(false);
                if !present {
                    eyre::bail!(
                        "primitive '{}': required param '{}' is missing",
                        self.name(),
                        field.name
                    );
                }
            }
        }
        Ok(())
    }

    /// Idempotency guarantee. See `Idempotency` enum docs.
    fn idempotency(&self) -> Idempotency;

    /// Whether this primitive requires exclusive git worktree access.
    /// If true, the engine acquires a centralized async git mutex before
    /// calling execute(). Default: false.
    fn requires_git_lock(&self) -> bool {
        false
    }
}
