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
pub fn parse_actions(raw: &str) -> Result<Vec<AgentAction>, ParseError> {
    let stripped = strip_markdown_fences(raw);
    let normalized = normalize_action_key(stripped);

    if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(&normalized) {
        return finalize(actions);
    }

    // Scan every `[` position in order; first that yields a balanced
    // substring parsing cleanly as `Vec<AgentAction>` wins. A single
    // `[` position is not enough: prose like "The result [is good]:
    // [...]" has a bracket-balanced non-array before the real one.
    let mut last_serde_err: Option<String> = None;
    for start in bracket_start_positions(&normalized) {
        if let Some(candidate) = extract_array_substring_from(&normalized, start) {
            match serde_json::from_str::<Vec<AgentAction>>(candidate) {
                Ok(actions) => return finalize(actions),
                Err(e) => last_serde_err = Some(e.to_string()),
            }
        }
    }

    if let Some(msg) = last_serde_err {
        return Err(ParseError::Serde(msg));
    }

    Err(ParseError::NoArrayFound)
}

fn bracket_start_positions(input: &str) -> impl Iterator<Item = usize> + '_ {
    input
        .char_indices()
        .filter_map(|(i, c)| if c == '[' { Some(i) } else { None })
}

/// Parse a single action (used by the correctable-tool-error
/// re-prompt path: the LLM is asked for exactly one corrected
/// action, not an array).
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
fn strip_markdown_fences(raw: &str) -> &str {
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

/// Replace `"action":` occurrences with `"type":`. The LLM sometimes
/// emits the v3 key name; serde's tag discriminator expects `type`.
/// Naive string replace is safe here: `"action":` is not a common
/// substring inside tool-input JSON (tool inputs carry keys like
/// `command`, `path`, etc., and any inline string that happens to
/// contain `"action":` as substring would be inside a quoted
/// string, which a future improvement could protect with a proper
/// JSON-aware rewriter).
fn normalize_action_key(input: &str) -> String {
    input.replace("\"action\":", "\"type\":")
}

/// Locate the first well-balanced `[ ... ]` substring starting at
/// byte offset `start`. Scans character-by-character, tracking
/// bracket depth and ignoring brackets inside string literals.
/// Returns the substring (including outer brackets) on success.
fn extract_array_substring_from(input: &str, start: usize) -> Option<&str> {
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
