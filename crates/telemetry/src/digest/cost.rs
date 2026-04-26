//! Per-model rate table for the LLM cost line in the per-process digest.
//!
//! Rates stored as **U.S. micro-dollars per 1M tokens** so the rest of
//! the digest math stays in `u64`. A `None` lookup means "model not in
//! table"; the caller should record the cost as zero and emit a
//! `debug!` so a future reader can update the table.
//!
//! Sourced from Anthropic's published pricing page; the digest body
//! includes a footnote `(rates as of <date>)` so a stale entry is
//! self-evident in the rendered file. Update is a one-line change.

/// Per-1M-token rate in U.S. micro-dollars (`u64`). Storing micros
/// keeps every downstream multiplication / addition in integer space;
/// the digest renderer divides once, at the end, to format dollars.
#[derive(Debug, Clone, Copy)]
pub struct ModelRate {
    pub input_per_million_micros: u64,
    pub output_per_million_micros: u64,
    /// Cache-write rate is typically 1.25x input; cache-read is
    /// typically 0.1x input. Stored as full per-1M micros rather than
    /// as a multiplier so the table is grep-able.
    pub cache_write_per_million_micros: u64,
    pub cache_read_per_million_micros: u64,
}

/// Look up a rate by concrete model id (e.g. `claude-opus-4-7`,
/// `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`).
///
/// Rates as of 2026-04-25 (per the published Anthropic pricing page).
pub fn rate_for(model: &str) -> Option<ModelRate> {
    match model {
        "claude-opus-4-7" => Some(ModelRate {
            input_per_million_micros: 15_000_000,
            output_per_million_micros: 75_000_000,
            cache_write_per_million_micros: 18_750_000,
            cache_read_per_million_micros: 1_500_000,
        }),
        "claude-sonnet-4-6" => Some(ModelRate {
            input_per_million_micros: 3_000_000,
            output_per_million_micros: 15_000_000,
            cache_write_per_million_micros: 3_750_000,
            cache_read_per_million_micros: 300_000,
        }),
        m if m.starts_with("claude-haiku-4-5") => Some(ModelRate {
            input_per_million_micros: 800_000,
            output_per_million_micros: 4_000_000,
            cache_write_per_million_micros: 1_000_000,
            cache_read_per_million_micros: 80_000,
        }),
        _ => None,
    }
}

/// Compute the U.S. micro-dollar cost of one LLM call given the model
/// and the four per-call usage counts. Unknown models return 0.
pub fn cost_micros(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens: u64,
) -> u64 {
    let Some(rate) = rate_for(model) else {
        return 0;
    };
    // Each multiplication can overflow if a single call somehow
    // burned > 1.2e10 tokens; saturating math protects the digest from
    // a meaningless "cost: 18.4 quintillion dollars" rendered line.
    let input = (input_tokens.saturating_mul(rate.input_per_million_micros)) / 1_000_000;
    let output = (output_tokens.saturating_mul(rate.output_per_million_micros)) / 1_000_000;
    let cache_w = (cache_write_tokens.saturating_mul(rate.cache_write_per_million_micros)) / 1_000_000;
    let cache_r = (cache_read_tokens.saturating_mul(rate.cache_read_per_million_micros)) / 1_000_000;
    input
        .saturating_add(output)
        .saturating_add(cache_w)
        .saturating_add(cache_r)
}

/// Format a `u64` micro-dollar amount as `$X.XX` (always two decimal places).
pub fn format_dollars(micros: u64) -> String {
    let dollars = micros / 1_000_000;
    let cents = (micros % 1_000_000) / 10_000;
    format!("${}.{:02}", dollars, cents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_return_a_rate() {
        assert!(rate_for("claude-opus-4-7").is_some());
        assert!(rate_for("claude-sonnet-4-6").is_some());
        assert!(rate_for("claude-haiku-4-5-20251001").is_some());
    }

    #[test]
    fn unknown_model_returns_none_and_zero_cost() {
        assert!(rate_for("not-a-model").is_none());
        assert_eq!(cost_micros("not-a-model", 1_000_000, 1_000_000, 0, 0), 0);
    }

    #[test]
    fn cost_micros_basic_input_output() {
        // 1M input tokens at $3/1M = $3 = 3_000_000 micros
        // 1M output tokens at $15/1M = $15 = 15_000_000 micros
        assert_eq!(cost_micros("claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0), 18_000_000);
    }

    #[test]
    fn cost_micros_includes_cache_columns() {
        // 1M cache_write at $3.75/1M + 1M cache_read at $0.30/1M
        let expected = 3_750_000 + 300_000;
        assert_eq!(cost_micros("claude-sonnet-4-6", 0, 0, 1_000_000, 1_000_000), expected);
    }

    #[test]
    fn format_dollars_two_decimal_places() {
        assert_eq!(format_dollars(0), "$0.00");
        assert_eq!(format_dollars(15_000), "$0.01");
        assert_eq!(format_dollars(1_000_000), "$1.00");
        assert_eq!(format_dollars(18_750_000), "$18.75");
    }
}
