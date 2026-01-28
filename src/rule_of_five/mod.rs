//! Rule of Five: Structured Plan Refinement.
//!
//! This module implements the 5-pass review methodology for creating high-quality
//! Plan documents. Each pass focuses on a specific quality dimension:
//!
//! 1. **Completeness** - Are all sections present? Missing requirements?
//! 2. **Correctness** - Logical errors? Wrong assumptions?
//! 3. **Edge Cases** - What could go wrong? Error handling?
//! 4. **Architecture** - Does this fit the larger system?
//! 5. **Clarity** - Is it implementable? Ambiguous sections?

#![allow(unused_imports)]

mod executor;
mod passes;
mod validation;

pub use executor::{ExecutorError, PassResult, RuleOfFiveConfig, RuleOfFiveExecutor};
pub use passes::{PassPrompt, ReviewPass, get_pass_prompt};
pub use validation::{PassValidationResult, PassValidator, validate_pass};

/// The five review passes.
pub const TOTAL_PASSES: u32 = 5;
