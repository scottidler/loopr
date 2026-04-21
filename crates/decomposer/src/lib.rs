//! Goal into Plan into Spec into Phase into Work DAG. The middle-end of the pipeline.

mod cycles;
mod decompose;
mod error;
mod prompt;
mod tool;
mod tree;

pub use decompose::decompose;
pub use error::DecomposerError;
