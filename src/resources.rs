use std::path::Path;

use rust_embed::Embed;
use tracing::info;

/// Embedded prompt files (.pmt) compiled into the binary at build time.
/// In debug builds, files are read from disk (enables hot-reload during development).
#[derive(Embed)]
#[folder = "prompts/"]
struct EmbeddedPrompts;

/// Embedded strategy files (.yml) compiled into the binary at build time.
/// In debug builds, files are read from disk (enables hot-reload during development).
#[derive(Embed)]
#[folder = "strategies/"]
struct EmbeddedStrategies;

/// Unified resource loader - compiled-in defaults with filesystem override semantics.
///
/// During Phase A (phases 1-4), two embedded structs back this loader:
/// - `.pmt` files are served from `EmbeddedPrompts` (embedded `prompts/` directory)
/// - `.yml`/`.yaml` files are served from `EmbeddedStrategies` (embedded `strategies/` directory)
///
/// After Phase B (directory reorganization in phase 5), both collapse into a single
/// `EmbeddedResources` struct backed by the `resources/` directory.
pub struct Resources;

impl Resources {
    /// Load a text resource by path.
    ///
    /// Resolution order:
    /// 0. Absolute path - if `path` starts with `/`, load directly from the filesystem.
    ///    Fatal on failure or empty content (A/B experiment mode: prevents silent fallback
    ///    that would corrupt experiment data by scoring the baseline as the trial prompt).
    /// 1. Repo-local override - `{repo_path}/resources/{path}`
    /// 2. XDG override - `~/.config/loopr/resources/{path}`
    /// 3. Embedded default - compiled into the binary via rust-embed
    pub fn load(path: &str, repo_path: Option<&Path>) -> eyre::Result<String> {
        // 0. Absolute path: direct load, fatal on failure
        if path.starts_with('/') {
            let content = std::fs::read_to_string(path)
                .map_err(|e| eyre::eyre!("absolute resource path not found: {}: {}", path, e))?;
            eyre::ensure!(!content.trim().is_empty(), "absolute resource path is empty: {}", path);
            return Ok(content);
        }

        // 1. Repo-local override
        if let Some(repo) = repo_path {
            let local = repo.join("resources").join(path);
            if let Ok(content) = std::fs::read_to_string(&local)
                && !content.trim().is_empty()
            {
                info!("resource override loaded: {}", local.display());
                return Ok(content);
            }
        }

        // 2. XDG override
        if let Some(config_dir) = dirs::config_dir() {
            let xdg = config_dir.join("loopr/resources").join(path);
            if let Ok(content) = std::fs::read_to_string(&xdg)
                && !content.trim().is_empty()
            {
                info!("resource XDG override loaded: {}", xdg.display());
                return Ok(content);
            }
        }

        // 3. Embedded default
        Self::get_embedded(path)
    }

    /// Load all files matching a directory prefix.
    ///
    /// Merges filesystem overrides with embedded defaults on a per-file basis:
    /// if a file exists in the repo-local or XDG override location, it replaces
    /// the embedded version for that specific file. Files only present in the
    /// embedded set are still included.
    ///
    /// Returns `Vec<(relative_path, content)>` sorted by path for determinism.
    pub fn load_dir(prefix: &str, repo_path: Option<&Path>) -> eyre::Result<Vec<(String, String)>> {
        let paths = Self::list_embedded(prefix);
        if paths.is_empty() {
            return Err(eyre::eyre!("no embedded resources found with prefix: {}", prefix));
        }
        let mut results = Vec::new();
        for rel_path in paths {
            let content = Self::load(&rel_path, repo_path)?;
            results.push((rel_path, content));
        }
        Ok(results)
    }

    /// Check whether a resource exists in any layer.
    pub fn exists(path: &str, repo_path: Option<&Path>) -> bool {
        if path.starts_with('/') {
            return Path::new(path).exists();
        }
        if let Some(repo) = repo_path
            && repo.join("resources").join(path).exists()
        {
            return true;
        }
        if let Some(config_dir) = dirs::config_dir()
            && config_dir.join("loopr/resources").join(path).exists()
        {
            return true;
        }
        Self::get_embedded(path).is_ok()
    }

    fn get_embedded(path: &str) -> eyre::Result<String> {
        let data_opt = if path.ends_with(".pmt") {
            EmbeddedPrompts::get(path).map(|f| f.data)
        } else if path.ends_with(".yml") || path.ends_with(".yaml") {
            EmbeddedStrategies::get(path).map(|f| f.data)
        } else {
            EmbeddedPrompts::get(path)
                .map(|f| f.data)
                .or_else(|| EmbeddedStrategies::get(path).map(|f| f.data))
        };

        match data_opt {
            Some(data) => {
                let content = std::str::from_utf8(data.as_ref())
                    .map_err(|e| eyre::eyre!("resource is not valid UTF-8: {}: {}", path, e))?;
                Ok(content.to_string())
            }
            None => Err(eyre::eyre!("resource not found: {}", path)),
        }
    }

    fn list_embedded(prefix: &str) -> Vec<String> {
        let mut paths: Vec<String> = EmbeddedStrategies::iter()
            .filter(|p| p.starts_with(prefix))
            .map(|p| p.to_string())
            .collect();
        let prompt_paths: Vec<String> = EmbeddedPrompts::iter()
            .filter(|p| p.starts_with(prefix))
            .map(|p| p.to_string())
            .collect();
        paths.extend(prompt_paths);
        paths.sort();
        paths
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
