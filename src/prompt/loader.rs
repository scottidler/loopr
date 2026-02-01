//! Prompt Loader - Load and cache prompt templates from files
//!
//! This module provides the PromptLoader struct which loads prompt templates
//! from a directory and caches them in memory for efficient access.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::error::{LooprError, Result};

/// Loads and caches prompt templates from a directory
pub struct PromptLoader {
    /// Base directory containing prompt template files
    templates_dir: PathBuf,
    /// In-memory cache of loaded templates
    cache: RwLock<HashMap<String, String>>,
}

impl PromptLoader {
    /// Create a new PromptLoader with the given templates directory
    pub fn new(templates_dir: impl AsRef<Path>) -> Self {
        Self {
            templates_dir: templates_dir.as_ref().to_path_buf(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Load a template from disk and cache it
    ///
    /// # Arguments
    /// * `name` - The template name (without .md extension)
    ///
    /// # Returns
    /// The template content as a string
    pub fn load(&self, name: &str) -> Result<String> {
        // Check cache first
        {
            let cache = self.cache.read().map_err(|e| {
                LooprError::Storage(format!("Failed to acquire read lock: {}", e))
            })?;
            if let Some(content) = cache.get(name) {
                return Ok(content.clone());
            }
        }

        // Load from disk
        let path = self.template_path(name);
        let content = std::fs::read_to_string(&path).map_err(|e| {
            LooprError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to load template '{}' from {:?}: {}", name, path, e),
            ))
        })?;

        // Cache the loaded template
        {
            let mut cache = self.cache.write().map_err(|e| {
                LooprError::Storage(format!("Failed to acquire write lock: {}", e))
            })?;
            cache.insert(name.to_string(), content.clone());
        }

        Ok(content)
    }

    /// Get a cached template without loading from disk
    ///
    /// # Arguments
    /// * `name` - The template name (without .md extension)
    ///
    /// # Returns
    /// The template content if cached, None otherwise
    pub fn get(&self, name: &str) -> Option<String> {
        let cache = self.cache.read().ok()?;
        cache.get(name).cloned()
    }

    /// Check if a template exists on disk
    ///
    /// # Arguments
    /// * `name` - The template name (without .md extension)
    ///
    /// # Returns
    /// true if the template file exists
    pub fn exists(&self, name: &str) -> bool {
        self.template_path(name).exists()
    }

    /// Get the full path for a template by name
    fn template_path(&self, name: &str) -> PathBuf {
        self.templates_dir.join(format!("{}.md", name))
    }

    /// List all available templates in the directory
    pub fn list_available(&self) -> Result<Vec<String>> {
        let entries = std::fs::read_dir(&self.templates_dir).map_err(|e| {
            LooprError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "Failed to read templates directory {:?}: {}",
                    self.templates_dir, e
                ),
            ))
        })?;

        let mut templates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md")
                && let Some(stem) = path.file_stem()
                && let Some(name) = stem.to_str()
            {
                templates.push(name.to_string());
            }
        }

        templates.sort();
        Ok(templates)
    }

    /// Preload all templates from the directory into cache
    pub fn preload_all(&self) -> Result<usize> {
        let available = self.list_available()?;
        let mut loaded = 0;
        for name in &available {
            self.load(name)?;
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Clear the template cache
    pub fn clear_cache(&self) -> Result<()> {
        let mut cache = self.cache.write().map_err(|e| {
            LooprError::Storage(format!("Failed to acquire write lock: {}", e))
        })?;
        cache.clear();
        Ok(())
    }

    /// Get the templates directory path
    pub fn templates_dir(&self) -> &Path {
        &self.templates_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_loader() -> (PromptLoader, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let loader = PromptLoader::new(temp_dir.path());
        (loader, temp_dir)
    }

    fn write_template(temp_dir: &TempDir, name: &str, content: &str) {
        let path = temp_dir.path().join(format!("{}.md", name));
        fs::write(path, content).unwrap();
    }

    #[test]
    fn test_new_loader() {
        let (loader, temp_dir) = create_test_loader();
        assert_eq!(loader.templates_dir(), temp_dir.path());
    }

    #[test]
    fn test_load_template() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "test", "Hello {{name}}!");

        let content = loader.load("test").unwrap();
        assert_eq!(content, "Hello {{name}}!");
    }

    #[test]
    fn test_load_caches_template() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "test", "Original content");

        // First load
        let content1 = loader.load("test").unwrap();
        assert_eq!(content1, "Original content");

        // Modify file on disk
        write_template(&temp_dir, "test", "Modified content");

        // Second load should return cached version
        let content2 = loader.load("test").unwrap();
        assert_eq!(content2, "Original content");
    }

    #[test]
    fn test_get_cached() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "test", "Cached content");

        // Before loading, get returns None
        assert!(loader.get("test").is_none());

        // Load the template
        loader.load("test").unwrap();

        // After loading, get returns the content
        assert_eq!(loader.get("test"), Some("Cached content".to_string()));
    }

    #[test]
    fn test_get_not_cached() {
        let (loader, _temp_dir) = create_test_loader();
        assert!(loader.get("nonexistent").is_none());
    }

    #[test]
    fn test_exists() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "exists", "content");

        assert!(loader.exists("exists"));
        assert!(!loader.exists("nonexistent"));
    }

    #[test]
    fn test_load_nonexistent() {
        let (loader, _temp_dir) = create_test_loader();
        let result = loader.load("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_list_available() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "plan", "plan template");
        write_template(&temp_dir, "spec", "spec template");
        write_template(&temp_dir, "code", "code template");

        let available = loader.list_available().unwrap();
        assert_eq!(available, vec!["code", "plan", "spec"]);
    }

    #[test]
    fn test_list_available_empty() {
        let (loader, _temp_dir) = create_test_loader();
        let available = loader.list_available().unwrap();
        assert!(available.is_empty());
    }

    #[test]
    fn test_list_available_ignores_non_md_files() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "valid", "content");
        fs::write(temp_dir.path().join("ignore.txt"), "not a template").unwrap();
        fs::write(temp_dir.path().join("ignore.json"), "{}").unwrap();

        let available = loader.list_available().unwrap();
        assert_eq!(available, vec!["valid"]);
    }

    #[test]
    fn test_preload_all() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "plan", "plan template");
        write_template(&temp_dir, "spec", "spec template");
        write_template(&temp_dir, "code", "code template");

        let loaded = loader.preload_all().unwrap();
        assert_eq!(loaded, 3);

        // All should now be cached
        assert!(loader.get("plan").is_some());
        assert!(loader.get("spec").is_some());
        assert!(loader.get("code").is_some());
    }

    #[test]
    fn test_clear_cache() {
        let (loader, temp_dir) = create_test_loader();
        write_template(&temp_dir, "test", "content");

        // Load and verify cached
        loader.load("test").unwrap();
        assert!(loader.get("test").is_some());

        // Clear cache
        loader.clear_cache().unwrap();
        assert!(loader.get("test").is_none());
    }

    #[test]
    fn test_template_path() {
        let (loader, temp_dir) = create_test_loader();
        let expected = temp_dir.path().join("mytemplate.md");
        assert_eq!(loader.template_path("mytemplate"), expected);
    }

    #[test]
    fn test_templates_dir() {
        let (loader, temp_dir) = create_test_loader();
        assert_eq!(loader.templates_dir(), temp_dir.path());
    }
}
