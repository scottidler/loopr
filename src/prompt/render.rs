//! Prompt Renderer - Render templates with context variables using Handlebars
//!
//! This module provides the PromptRenderer struct which uses Handlebars to
//! render prompt templates with context variables.

use std::collections::HashMap;

use handlebars::Handlebars;
use serde::Serialize;
use serde_json::Value;

use crate::error::{LooprError, Result};

/// Renders prompt templates using Handlebars templating
pub struct PromptRenderer {
    handlebars: Handlebars<'static>,
}

impl Default for PromptRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptRenderer {
    /// Create a new PromptRenderer with default settings
    pub fn new() -> Self {
        let mut handlebars = Handlebars::new();
        // Don't escape HTML entities in output
        handlebars.set_strict_mode(false);
        // Register escape fn to prevent HTML escaping
        handlebars.register_escape_fn(handlebars::no_escape);
        Self { handlebars }
    }

    /// Render a template string with the given context
    ///
    /// # Arguments
    /// * `template` - The template string containing {{variable}} placeholders
    /// * `context` - A HashMap of variable names to values
    ///
    /// # Returns
    /// The rendered template as a string
    pub fn render(&self, template: &str, context: &HashMap<String, String>) -> Result<String> {
        self.handlebars
            .render_template(template, context)
            .map_err(|e| LooprError::InvalidState(format!("Failed to render template: {}", e)))
    }

    /// Render a template string with a JSON context
    ///
    /// # Arguments
    /// * `template` - The template string containing {{variable}} placeholders
    /// * `context` - A serde_json::Value containing the context data
    ///
    /// # Returns
    /// The rendered template as a string
    pub fn render_json(&self, template: &str, context: &Value) -> Result<String> {
        self.handlebars
            .render_template(template, context)
            .map_err(|e| LooprError::InvalidState(format!("Failed to render template: {}", e)))
    }

    /// Render a template string with any serializable context
    ///
    /// # Arguments
    /// * `template` - The template string containing {{variable}} placeholders
    /// * `context` - Any type that implements Serialize
    ///
    /// # Returns
    /// The rendered template as a string
    pub fn render_with<T: Serialize>(&self, template: &str, context: &T) -> Result<String> {
        self.handlebars
            .render_template(template, context)
            .map_err(|e| LooprError::InvalidState(format!("Failed to render template: {}", e)))
    }

    /// Render a template with context and append progress/feedback section
    ///
    /// # Arguments
    /// * `template` - The template string containing {{variable}} placeholders
    /// * `context` - A HashMap of variable names to values
    /// * `progress` - Progress/feedback text to append
    ///
    /// # Returns
    /// The rendered template with progress section appended
    pub fn render_with_progress(
        &self,
        template: &str,
        context: &HashMap<String, String>,
        progress: &str,
    ) -> Result<String> {
        let rendered = self.render(template, context)?;

        if progress.is_empty() {
            return Ok(rendered);
        }

        // Append progress section
        Ok(format!(
            "{}\n\n---\n\n## Previous Iteration Feedback\n\n{}",
            rendered, progress
        ))
    }

    /// Register a named template for later use
    ///
    /// # Arguments
    /// * `name` - The name to register the template under
    /// * `template` - The template string
    pub fn register_template(&mut self, name: &str, template: &str) -> Result<()> {
        self.handlebars
            .register_template_string(name, template)
            .map_err(|e| LooprError::InvalidState(format!("Failed to register template '{}': {}", name, e)))
    }

    /// Render a previously registered template
    ///
    /// # Arguments
    /// * `name` - The name of the registered template
    /// * `context` - A HashMap of variable names to values
    ///
    /// # Returns
    /// The rendered template as a string
    pub fn render_named(&self, name: &str, context: &HashMap<String, String>) -> Result<String> {
        self.handlebars
            .render(name, context)
            .map_err(|e| LooprError::InvalidState(format!("Failed to render template: {}", e)))
    }

    /// Check if a named template is registered
    pub fn has_template(&self, name: &str) -> bool {
        self.handlebars.get_template(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_renderer() -> PromptRenderer {
        PromptRenderer::new()
    }

    #[test]
    fn test_new_renderer() {
        let renderer = create_renderer();
        assert!(!renderer.has_template("nonexistent"));
    }

    #[test]
    fn test_default_renderer() {
        let renderer = PromptRenderer::default();
        assert!(!renderer.has_template("test"));
    }

    #[test]
    fn test_render_simple() {
        let renderer = create_renderer();
        let template = "Hello, {{name}}!";
        let mut context = HashMap::new();
        context.insert("name".to_string(), "World".to_string());

        let result = renderer.render(template, &context).unwrap();
        assert_eq!(result, "Hello, World!");
    }

    #[test]
    fn test_render_multiple_variables() {
        let renderer = create_renderer();
        let template = "{{greeting}}, {{name}}! Welcome to {{place}}.";
        let mut context = HashMap::new();
        context.insert("greeting".to_string(), "Hello".to_string());
        context.insert("name".to_string(), "Alice".to_string());
        context.insert("place".to_string(), "Loopr".to_string());

        let result = renderer.render(template, &context).unwrap();
        assert_eq!(result, "Hello, Alice! Welcome to Loopr.");
    }

    #[test]
    fn test_render_missing_variable_empty_string() {
        let renderer = create_renderer();
        let template = "Hello, {{name}}!";
        let context: HashMap<String, String> = HashMap::new();

        // Missing variables should render as empty string (non-strict mode)
        let result = renderer.render(template, &context).unwrap();
        assert_eq!(result, "Hello, !");
    }

    #[test]
    fn test_render_no_escape_html() {
        let renderer = create_renderer();
        let template = "Code: {{code}}";
        let mut context = HashMap::new();
        context.insert("code".to_string(), "<script>alert('xss')</script>".to_string());

        // Should NOT escape HTML
        let result = renderer.render(template, &context).unwrap();
        assert_eq!(result, "Code: <script>alert('xss')</script>");
    }

    #[test]
    fn test_render_json_context() {
        let renderer = create_renderer();
        let template = "Task: {{task}}, Status: {{status}}";
        let context = serde_json::json!({
            "task": "Build feature",
            "status": "in_progress"
        });

        let result = renderer.render_json(template, &context).unwrap();
        assert_eq!(result, "Task: Build feature, Status: in_progress");
    }

    #[test]
    fn test_render_with_serializable() {
        #[derive(Serialize)]
        struct Context {
            name: String,
            count: i32,
        }

        let renderer = create_renderer();
        let template = "{{name}} has {{count}} items";
        let context = Context {
            name: "Bob".to_string(),
            count: 5,
        };

        let result = renderer.render_with(template, &context).unwrap();
        assert_eq!(result, "Bob has 5 items");
    }

    #[test]
    fn test_render_with_progress_empty() {
        let renderer = create_renderer();
        let template = "Task: {{task}}";
        let mut context = HashMap::new();
        context.insert("task".to_string(), "Build feature".to_string());

        let result = renderer.render_with_progress(template, &context, "").unwrap();
        assert_eq!(result, "Task: Build feature");
    }

    #[test]
    fn test_render_with_progress_non_empty() {
        let renderer = create_renderer();
        let template = "Task: {{task}}";
        let mut context = HashMap::new();
        context.insert("task".to_string(), "Build feature".to_string());

        let progress = "Iteration 1 failed: tests not passing";
        let result = renderer.render_with_progress(template, &context, progress).unwrap();

        assert!(result.contains("Task: Build feature"));
        assert!(result.contains("---"));
        assert!(result.contains("## Previous Iteration Feedback"));
        assert!(result.contains("Iteration 1 failed: tests not passing"));
    }

    #[test]
    fn test_render_with_progress_multiline() {
        let renderer = create_renderer();
        let template = "## System\n\nYou are a helpful assistant.";
        let context: HashMap<String, String> = HashMap::new();

        let progress = "Iteration 1:\n- Error 1\n- Error 2\n\nIteration 2:\n- Error 3";
        let result = renderer.render_with_progress(template, &context, progress).unwrap();

        assert!(result.contains("## System"));
        assert!(result.contains("## Previous Iteration Feedback"));
        assert!(result.contains("- Error 1"));
        assert!(result.contains("- Error 3"));
    }

    #[test]
    fn test_register_template() {
        let mut renderer = create_renderer();
        renderer.register_template("greeting", "Hello, {{name}}!").unwrap();
        assert!(renderer.has_template("greeting"));
    }

    #[test]
    fn test_render_named() {
        let mut renderer = create_renderer();
        renderer.register_template("greeting", "Hello, {{name}}!").unwrap();

        let mut context = HashMap::new();
        context.insert("name".to_string(), "World".to_string());

        let result = renderer.render_named("greeting", &context).unwrap();
        assert_eq!(result, "Hello, World!");
    }

    #[test]
    fn test_render_named_not_found() {
        let renderer = create_renderer();
        let context: HashMap<String, String> = HashMap::new();

        let result = renderer.render_named("nonexistent", &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_has_template() {
        let mut renderer = create_renderer();
        assert!(!renderer.has_template("test"));

        renderer.register_template("test", "content").unwrap();
        assert!(renderer.has_template("test"));
    }

    #[test]
    fn test_render_complex_template() {
        let renderer = create_renderer();
        let template = r#"# {{loop_type}} Loop

## Task
{{task}}

## Context
- Parent: {{parent_id}}
- Iteration: {{iteration}}

## Instructions
Follow the guidelines and produce valid output.
"#;

        let mut context = HashMap::new();
        context.insert("loop_type".to_string(), "Plan".to_string());
        context.insert("task".to_string(), "Build a web app".to_string());
        context.insert("parent_id".to_string(), "root".to_string());
        context.insert("iteration".to_string(), "1".to_string());

        let result = renderer.render(template, &context).unwrap();

        assert!(result.contains("# Plan Loop"));
        assert!(result.contains("Build a web app"));
        assert!(result.contains("- Parent: root"));
        assert!(result.contains("- Iteration: 1"));
    }

    #[test]
    fn test_render_preserves_whitespace() {
        let renderer = create_renderer();
        let template = "Line 1\n\nLine 3\n\n\nLine 6";
        let context: HashMap<String, String> = HashMap::new();

        let result = renderer.render(template, &context).unwrap();
        assert_eq!(result, "Line 1\n\nLine 3\n\n\nLine 6");
    }

    #[test]
    fn test_render_special_characters() {
        let renderer = create_renderer();
        let template = "Path: {{path}}";
        let mut context = HashMap::new();
        context.insert("path".to_string(), "/home/user/file.txt".to_string());

        let result = renderer.render(template, &context).unwrap();
        assert_eq!(result, "Path: /home/user/file.txt");
    }
}
