use std::collections::HashMap;

use tracing::info;

use super::types::Primitive;

/// Central registry of all primitives, keyed by name.
/// Built at daemon startup, immutable after initialization.
pub struct PrimitiveRegistry {
    primitives: HashMap<String, Box<dyn Primitive>>,
}

impl PrimitiveRegistry {
    pub fn new() -> Self {
        Self {
            primitives: HashMap::new(),
        }
    }

    /// Register a primitive. Called at startup.
    /// Returns an error if a primitive with the same name is already registered.
    pub fn register(&mut self, primitive: Box<dyn Primitive>) -> eyre::Result<()> {
        let name = primitive.name().to_string();
        if self.primitives.contains_key(&name) {
            eyre::bail!("duplicate primitive name: '{}'", name);
        }
        info!("registered primitive: {}", name);
        self.primitives.insert(name, primitive);
        Ok(())
    }

    /// Look up a primitive by name. Returns None if not found.
    pub fn get(&self, name: &str) -> Option<&dyn Primitive> {
        self.primitives.get(name).map(|p| p.as_ref())
    }

    /// Returns the number of registered primitives.
    pub fn len(&self) -> usize {
        self.primitives.len()
    }

    /// Returns true if no primitives are registered.
    pub fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }

    /// Validate that all primitive names referenced in YAML exist in the registry.
    /// Returns a list of names that are NOT registered.
    pub fn validate_references(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .filter(|n| !self.primitives.contains_key(n.as_str()))
            .cloned()
            .collect()
    }

    /// Returns an iterator over all registered primitive names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.primitives.keys().map(|s| s.as_str())
    }
}

impl Default for PrimitiveRegistry {
    fn default() -> Self {
        Self::new()
    }
}
