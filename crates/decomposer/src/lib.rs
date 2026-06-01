//! Goal into Plan into Spec into Phase into Work DAG. The middle-end of the pipeline.

mod decompose;
mod error;
mod prompt;
mod resolve;
mod tool;
mod tree;

pub use decompose::decompose;
pub use error::DecomposerError;
