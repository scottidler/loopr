//! CLI module for loopr - command-line interface and subcommands.
//!
//! Provides the main entry point with subcommands for daemon management,
//! loop operations, and TUI launch.

pub mod commands;

pub use commands::Cli;
