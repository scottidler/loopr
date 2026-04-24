//! CLI verb bodies. One submodule per `Command` variant (or cluster of
//! closely-related variants). Each body is a thin shell that:
//!
//! 1. Builds an IPC payload from CLI args
//! 2. Connects to the daemon via `crate::transport::connect_or_wait`
//! 3. Issues one `MethodName::*` request
//! 4. Decodes the typed result
//! 5. Prints human or structured output via `crate::output`
//!
//! Tests for each body live alongside the body file, per
//! `rules/rust.md` §Test file placement.

pub mod list;
