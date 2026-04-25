//! `TranscriptIteration`: structured input the renderer consumes.

/// One LLM round-trip's worth of data, plus dispatcher outcome. Agents
/// populate this; the renderer produces the markdown block. Holding the
/// full text here (not just lengths) lets the renderer enforce the
/// per-iteration cap centrally instead of every agent reimplementing
/// truncation.
#[derive(Debug, Clone)]
pub struct TranscriptIteration {
    /// 1-based iteration index. Decomposer + Reviewer always emit `1`;
    /// Implementer counts up.
    pub iteration: u32,
    /// Concrete model id (e.g. `claude-opus-4-7`). NOT a tier alias.
    pub model: String,
    /// ISO-8601-ish timestamp when the LLM call started.
    pub started_at: String,
    /// Wall clock for the LLM call.
    pub latency_ms: u64,
    /// Token counts as reported by the API.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Session id this iteration belongs to.
    pub session_id: String,
    /// Process id of the agent that issued the call.
    pub process_id: String,
    /// Path to the run's events.log file (for the "Raw" link in the
    /// rendered iteration). Stringly-typed because the writer crate
    /// doesn't depend on telemetry's path types.
    pub events_log_path: String,
    /// Full system prompt as sent to the LLM.
    pub system_prompt: String,
    /// Full user message as sent to the LLM. For Implementer iterations
    /// 2..N this carries the accumulated history; that's the point.
    pub user_prompt: String,
    /// Verbatim LLM response text.
    pub response: String,
    /// Per-action one-liner summaries (rendered into the Parsed Actions
    /// section). Empty list = "no parsed actions" (e.g. a parse failure).
    pub parsed_actions: Vec<String>,
    /// Per-action dispatcher outcome one-liners.
    pub dispatcher_outcomes: Vec<String>,
    /// Lifeguard's decision after the iteration, if applicable
    /// (Implementer only). `None` for Decomposer/Reviewer.
    pub lifeguard_decision: Option<String>,
}

impl TranscriptIteration {
    /// Constructor with sensible defaults for Decomposer / Reviewer
    /// (single iteration, no lifeguard). Tests use this; agents
    /// populate fields manually.
    pub fn new_single_turn(model: impl Into<String>, started_at: impl Into<String>) -> Self {
        Self {
            iteration: 1,
            model: model.into(),
            started_at: started_at.into(),
            latency_ms: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            session_id: String::new(),
            process_id: String::new(),
            events_log_path: String::new(),
            system_prompt: String::new(),
            user_prompt: String::new(),
            response: String::new(),
            parsed_actions: Vec::new(),
            dispatcher_outcomes: Vec::new(),
            lifeguard_decision: None,
        }
    }
}
