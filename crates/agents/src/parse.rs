//! Parse an LLM free-form response into a `Vec<AgentAction>`.
//!
//! The LLM is asked to return a JSON array of actions. In practice
//! the response may:
//! - be wrapped in a ```json ... ``` markdown fence
//! - be preceded or followed by prose the model "helpfully" added
//! - use `"action"` instead of the canonical `"type"` discriminator
//!
//! `parse_actions` handles each of these: strip fences, normalize
//! the discriminator, try direct deserialization, then fall back to
//! balanced-bracket scanning to locate a JSON array embedded in
//! prose. `rfind(']')` would be wrong: it greedily captures any
//! trailing `]` in surrounding text.

use tracing::{debug, instrument};

use crate::action::AgentAction;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("response contained no JSON array")]
    NoArrayFound,
    #[error("parsed array was empty")]
    EmptyArray,
    #[error("JSON deserialization failed: {0}")]
    Serde(String),
}

/// Parse an LLM response string into a vec of `AgentAction`.
///
/// Pipeline:
/// 1. Strip markdown code fences (` ```json ` / ` ``` `) if present.
/// 2. Normalize `"action"` key to `"type"`.
/// 3. Try to deserialize the full stripped string.
/// 4. On failure, extract a JSON array via balanced-bracket scan.
/// 5. Try to deserialize the extracted substring.
///
/// Returns `Err(EmptyArray)` if parsing succeeds but the array has
/// zero elements (the LLM emitted nothing actionable).
#[instrument(level = "debug", skip_all, fields(raw_chars = raw.len(), action_count = tracing::field::Empty), err)]
pub fn parse_actions(raw: &str) -> Result<Vec<AgentAction>, ParseError> {
    let stripped = strip_markdown_fences(raw);
    let normalized = normalize_action_key(stripped);
    let span = tracing::Span::current();

    if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(&normalized) {
        let result = finalize(actions);
        if let Ok(ref a) = result {
            span.record("action_count", a.len());
            debug!(action_count = a.len(), path = "direct", "parse: actions parsed");
        }
        return result;
    }

    // Scan every `[` position in order; first that yields a balanced
    // substring parsing cleanly as `Vec<AgentAction>` wins. A single
    // `[` position is not enough: prose like "The result [is good]:
    // [...]" has a bracket-balanced non-array before the real one.
    //
    // `normalize_action_key` is re-applied per candidate (not just to
    // the whole `stripped` body above): when the whole response fails
    // to parse as JSON (because of surrounding prose), the earlier
    // whole-body normalization was a no-op, so any legacy `"action"`
    // key in the isolated candidate still needs renaming here, once
    // it is standalone valid JSON.
    let mut last_serde_err: Option<String> = None;
    for start in bracket_start_positions(&normalized) {
        if let Some(candidate) = extract_array_substring_from(&normalized, start) {
            let candidate = normalize_action_key(candidate);
            match serde_json::from_str::<Vec<AgentAction>>(&candidate) {
                Ok(actions) => {
                    let result = finalize(actions);
                    if let Ok(ref a) = result {
                        span.record("action_count", a.len());
                        debug!(action_count = a.len(), path = "extracted", "parse: actions parsed");
                    }
                    return result;
                }
                Err(e) => last_serde_err = Some(e.to_string()),
            }
        }
    }

    if let Some(msg) = last_serde_err {
        return Err(ParseError::Serde(msg));
    }

    Err(ParseError::NoArrayFound)
}

pub(crate) fn bracket_start_positions(input: &str) -> impl Iterator<Item = usize> + '_ {
    input
        .char_indices()
        .filter_map(|(i, c)| if c == '[' { Some(i) } else { None })
}

/// Parse a single action (used by the correctable-tool-error
/// re-prompt path: the LLM is asked for exactly one corrected
/// action, not an array).
#[instrument(level = "debug", skip_all, fields(raw_chars = raw.len()), err)]
pub fn parse_one(raw: &str) -> Result<AgentAction, ParseError> {
    let stripped = strip_markdown_fences(raw);
    let normalized = normalize_action_key(stripped);
    if let Ok(action) = serde_json::from_str::<AgentAction>(&normalized) {
        return Ok(action);
    }
    // Fall back to array-parse and take the first element if the LLM
    // ignored our instruction and emitted an array anyway.
    let actions = parse_actions(raw)?;
    actions.into_iter().next().ok_or(ParseError::EmptyArray)
}

fn finalize(actions: Vec<AgentAction>) -> Result<Vec<AgentAction>, ParseError> {
    if actions.is_empty() { Err(ParseError::EmptyArray) } else { Ok(actions) }
}

/// Strip ``` ```json ... ``` ``` and ``` ``` ... ``` ``` fences, if
/// present, returning the inner content trimmed of surrounding
/// whitespace. A fence-less input is returned as-is (trimmed).
pub(crate) fn strip_markdown_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        let rest = rest.trim_start_matches(char::is_whitespace);
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.trim_start_matches(char::is_whitespace);
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    trimmed
}

/// Rename the legacy `"action"` discriminator key to `"type"` on the
/// top-level action object(s) only. The LLM sometimes emits the v3 key
/// name; serde's tag discriminator expects `type`.
///
/// JSON-aware, not a blind string replace: `input` is parsed to a
/// `serde_json::Value` and only each top-level object's own `"action"`
/// key is renamed (an array of action objects, or a single action
/// object). Nested content is never touched, so a `run_tool` action
/// whose `input` writes a file containing the literal text `"action":`
/// (e.g. this very module's own doc comments, or any JSON/code the
/// implementer is writing) survives byte-for-byte. A prior blind
/// `input.replace("\"action\":", "\"type\":")` corrupted exactly that
/// case: every occurrence of the substring anywhere in the response,
/// including inside quoted file content, was rewritten.
///
/// Returns `input` unchanged (as an owned `String`) when it does not
/// parse as JSON at all - e.g. the model wrapped the array in prose.
/// Callers that later isolate a JSON substring via balanced-bracket
/// extraction (see `bracket_start_positions` / `extract_array_substring_from`)
/// re-apply this function to that substring, where it will parse
/// cleanly once isolated from the surrounding prose.
fn normalize_action_key(input: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(input) else {
        return input.to_string();
    };
    rename_top_level_action_keys(&mut value);
    serde_json::to_string(&value).unwrap_or_else(|_| input.to_string())
}

/// Rename `"action"` -> `"type"` on the object itself, or on every
/// object in a top-level array. Never descends into nested objects or
/// arrays (tool-input payloads), which may legitimately carry
/// unrelated `action`/`type` keys of their own.
fn rename_top_level_action_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(v) = map.remove("action") {
                map.insert("type".to_string(), v);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let serde_json::Value::Object(map) = item
                    && let Some(v) = map.remove("action")
                {
                    map.insert("type".to_string(), v);
                }
            }
        }
        _ => {}
    }
}

/// Locate the first well-balanced `[ ... ]` substring starting at
/// byte offset `start`. Scans character-by-character, tracking
/// bracket depth and ignoring brackets inside string literals.
/// Returns the substring (including outer brackets) on success.
pub(crate) fn extract_array_substring_from(input: &str, start: usize) -> Option<&str> {
    let bytes = input.as_bytes();
    if start >= bytes.len() || bytes[start] != b'[' {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests;
