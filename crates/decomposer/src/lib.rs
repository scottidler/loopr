//! Goal into Plan into Spec into Phase into Work DAG. The middle-end of the pipeline.

mod cycles;
mod error;
mod tool;
mod tree;

pub use error::DecomposerError;
