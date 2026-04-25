//! `PromptLoader`: three-layer `.pmt` resolution + handlebars rendering.
//!
//! The loader is a thin wrapper around handlebars's built-in registry.
//! At construction it walks the layers in priority order — baked
//! (`include_dir!()`-embedded `crates/context/prompts/`), then
//! `user_root` (typically `~/.config/loopr/prompts/`), then
//! `target_root` (typically `<cwd>/.loopr/prompts/`) — re-registering
//! the same names so each later layer overwrites earlier ones. The
//! handlebars registry IS the cache; no manual invalidation logic
//! needed.
//!
//! Files under `partials/` are registered as handlebars partials with
//! name = filename stem (e.g. `partials/tools-list.pmt` -> partial
//! `tools-list`). Files elsewhere are registered as templates with
//! name = relative path from the prompts/ root (e.g.
//! `agents/implementer/system.pmt`). `.gitkeep` files are skipped.

use std::path::{Path, PathBuf};

use handlebars::Handlebars;
use include_dir::{Dir, DirEntry, include_dir};
use serde::Serialize;
use tracing::instrument;

/// The baked prompt tree, embedded at compile time. Exposed so
/// `loopr init` can walk it to seed `<target>/.loopr/prompts/` and
/// integration tests can construct a baked-only loader without
/// touching the filesystem.
pub static BAKED_PROMPTS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/prompts");

/// Returns a reference to the baked prompt tree. `loopr init` uses
/// this; production loader construction also walks it.
pub fn baked_prompts() -> &'static Dir<'static> {
    &BAKED_PROMPTS
}

/// Wraps a handlebars registry pre-populated with every `.pmt`
/// template found across the baked, user, and target layers. Strict
/// mode is enabled; HTML escaping is disabled (LLM prompts are not
/// HTML).
#[derive(Debug)]
pub struct PromptLoader {
    registry: Handlebars<'static>,
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("prompt not registered (no layer provides it): {name}")]
    NotFound { name: String },
    #[error("handlebars parse error in {name}: {source}")]
    Parse {
        name: String,
        #[source]
        source: handlebars::TemplateError,
    },
    #[error("handlebars render error in {name}: {source}")]
    Render {
        name: String,
        #[source]
        source: handlebars::RenderError,
    },
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl PromptLoader {
    /// Construct with optional `target_root` (typically
    /// `<cwd>/.loopr/prompts/`) and `user_root` (typically
    /// `~/.config/loopr/prompts/`). Either may be `None` to skip that
    /// layer; the baked layer is always registered.
    ///
    /// Construction fails (`PromptError::Parse` / `PromptError::Io`)
    /// only if a `.pmt` file in any layer is malformed handlebars or
    /// unreadable. Missing optional layers are silently skipped.
    pub fn new(target_root: Option<PathBuf>, user_root: Option<PathBuf>) -> Result<Self, PromptError> {
        let mut registry = Handlebars::new();
        registry.set_strict_mode(true);
        registry.register_escape_fn(handlebars::no_escape);

        register_from_dir(&mut registry, &BAKED_PROMPTS)?;
        if let Some(root) = user_root.as_deref()
            && root.exists()
        {
            register_from_fs(&mut registry, root, root)?;
        }
        if let Some(root) = target_root.as_deref()
            && root.exists()
        {
            register_from_fs(&mut registry, root, root)?;
        }

        Ok(Self { registry })
    }

    /// Render a `.pmt` template by name with the given context. The
    /// name is the relative path under `prompts/` (e.g.
    /// `"agents/implementer/system.pmt"`). Lookup uses the highest-
    /// priority registration: target > user > baked.
    #[instrument(level = "debug", skip_all, fields(template = name, rendered_chars = tracing::field::Empty), err)]
    pub fn render<C: Serialize>(&self, name: &str, ctx: &C) -> Result<String, PromptError> {
        if !self.registry.has_template(name) {
            return Err(PromptError::NotFound { name: name.to_string() });
        }
        let out = self.registry.render(name, ctx).map_err(|source| PromptError::Render {
            name: name.to_string(),
            source,
        })?;
        tracing::Span::current().record("rendered_chars", out.len());
        Ok(out)
    }
}

fn register_from_dir(registry: &mut Handlebars<'static>, dir: &Dir<'_>) -> Result<(), PromptError> {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(d) => register_from_dir(registry, d)?,
            DirEntry::File(f) => {
                let path = f.path();
                let Some(name) = template_name_from_path(path) else {
                    continue;
                };
                let source = std::str::from_utf8(f.contents()).expect("baked .pmt files must be valid UTF-8");
                register_one(registry, &name, source)?;
            }
        }
    }
    Ok(())
}

fn register_from_fs(registry: &mut Handlebars<'static>, root: &Path, dir: &Path) -> Result<(), PromptError> {
    let entries = std::fs::read_dir(dir).map_err(|source| PromptError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| PromptError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PromptError::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            register_from_fs(registry, root, &path)?;
        } else if file_type.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let Some(name) = template_name_from_path(rel) else {
                continue;
            };
            let source = std::fs::read_to_string(&path).map_err(|source| PromptError::Io {
                path: path.clone(),
                source,
            })?;
            register_one(registry, &name, &source)?;
        }
    }
    Ok(())
}

/// Compute the registration name for a path. Returns `None` when the
/// file should be skipped (`.gitkeep`, anything not ending in `.pmt`).
/// Paths under `partials/` are registered as partials and use the
/// filename stem; everything else is a template and uses the full
/// relative path.
fn template_name_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    if file_name == ".gitkeep" {
        return None;
    }
    if !file_name.ends_with(".pmt") {
        return None;
    }
    Some(path.to_string_lossy().replace('\\', "/"))
}

fn register_one(registry: &mut Handlebars<'static>, name: &str, source: &str) -> Result<(), PromptError> {
    if name.starts_with("partials/") {
        let stem = name
            .trim_start_matches("partials/")
            .trim_end_matches(".pmt")
            .to_string();
        registry
            .register_partial(&stem, source)
            .map_err(|source| PromptError::Parse {
                name: name.to_string(),
                source,
            })
    } else {
        registry
            .register_template_string(name, source)
            .map_err(|source| PromptError::Parse {
                name: name.to_string(),
                source,
            })
    }
}

#[cfg(test)]
mod tests;
