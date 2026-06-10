//! `DecomposerError`: typed failure modes of `decompose`.
//!
//! Each variant names a distinct caller-distinguishable failure. The
//! daemon's `plan.create` handler maps these to log messages and
//! structured span fields; the retry-with-error-in-prompt path uses
//! the `Display` impl as the error text embedded into the retry
//! prompt so the model can self-correct.

use domain::PlanId;
use llm::LlmError;

#[derive(Debug, thiserror::Error)]
pub enum DecomposerError {
    /// The LLM call failed after the single retry. Carries the last
    /// error so callers can inspect whether it was `Retryable` (the
    /// caller may choose to loop again) or `Fatal` (bail to user).
    /// Boxed: `LlmError` is large (its `Fatal` body can be several KiB),
    /// and inlining it bloated every `DecomposerError` (and the
    /// `Result<_, ValidationFailure>` carrying it) past the
    /// large-error-variant threshold.
    #[error("LLM call failed: {0}")]
    LlmFailed(#[from] Box<LlmError>),

    /// The model returned `children: []`. Scope memo A+1: bail loudly.
    #[error("LLM produced zero child Works for plan {0}")]
    ZeroChildren(PlanId),

    /// The model returned more children than `decomposer.max_children`
    /// allows. The handler spawns an Implementer per unblocked Work with
    /// no pool cap, so an oversized decomposition would fan out too many
    /// concurrent agents; bail (after one retry) and ask for fewer,
    /// coarser Works.
    #[error("LLM produced {count} child Works; max-children is {max}. Decompose into at most {max} coarser Works.")]
    TooManyChildren { count: usize, max: usize },

    /// One or more children named a sibling title that did not appear
    /// in the same batch. The LLM hallucinated a dependency target.
    #[error("unresolved sibling dependencies: {0}")]
    UnresolvedDeps(String),

    /// Title-to-id resolution produced a DAG with a cycle among the
    /// named titles.
    #[error("dependency cycle among: {0}")]
    CycleDetected(String),

    /// Workspace scan (`git ls-files` + fallback) failed at the IO
    /// layer. Empty workspace is legal (yields `(empty workspace)`);
    /// this variant fires only on permissions / non-existent target.
    #[error("workspace scan failed: {0}")]
    WorkspaceScanFailed(String),

    /// A Work's `acceptance_criteria` came back empty and markdown
    /// extraction from its `content` also yielded zero criteria. A
    /// Work with empty AC would deadlock Stage 7's
    /// `Ready -> InProgress` precondition, so we bail at decompose
    /// time rather than persist the broken record.
    #[error("Work {0:?} has zero acceptance criteria; LLM must produce at least one")]
    EmptyAcceptanceCriteria(String),

    /// The `llm` crate returned a well-formed `ToolCall` (tool-use
    /// block present, `input` is valid JSON), but the `input` did not
    /// deserialize into `DecomposeResponse` — missing `children`
    /// field, wrong per-child shape, non-string `title`, etc. This
    /// is a decomposer-layer structural problem distinct from
    /// `llm::FatalReason::SchemaValidation`.
    #[error("tool_call input didn't deserialize into decompose schema: {0}")]
    MalformedChildren(String),

    /// Two or more children in the same decomposition normalize to
    /// the same title (after `trim().to_lowercase()`). The
    /// server-side title-to-id map cannot disambiguate dependency
    /// targets.
    #[error("LLM produced duplicate child titles: {0:?}")]
    DuplicateTitles(Vec<String>),

    /// At least one child's `title` was empty or whitespace-only.
    /// Included in its own variant (rather than folded into
    /// `MalformedChildren`) because the semantics is "the model
    /// produced a well-shaped but unusable Work" — a distinct failure
    /// from schema malformation. The usize is the child's index into
    /// the `children` array so the retry prompt can point at it.
    #[error("child at index {0} has empty title")]
    EmptyTitle(usize),

    /// A child's `files` scope contained an unusable path: absolute, a
    /// parent-traversal (`..`), or a backslash separator. The per-Work scope
    /// is rendered into the implementer/reviewer prompts (Phase-5 finding
    /// 10) and is meaningless — or an escape — if it isn't a repo-relative
    /// forward-slash path. Caught at produce time and routed through the
    /// retry-with-error path so the model re-emits a clean scope.
    #[error("child {child:?} has invalid scope path {path:?}: {why}")]
    InvalidFiles {
        child: String,
        path: String,
        why: &'static str,
    },

    /// Failure constructing or rendering a `.pmt` template via the
    /// `context::PromptLoader`. Surfaces e.g. a malformed override
    /// `.pmt` file in `<target>/.loopr/prompts/`, or a missing
    /// template under any layer.
    #[error("prompt error: {0}")]
    Prompt(#[from] context::PromptError),
}
