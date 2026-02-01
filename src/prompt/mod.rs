//! Prompt System - Template loading and rendering
//!
//! This module provides functionality for loading prompt templates from files
//! and rendering them with context variables using Handlebars.

mod loader;
mod render;

pub use loader::PromptLoader;
pub use render::PromptRenderer;
