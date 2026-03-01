use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use eyre::{Result, eyre};

/// Denylist patterns for path sandboxing. Matched against the file name component.
pub const PATH_DENYLIST: &[&str] = &[".env", "credentials.", "secret"];
pub const EXT_DENYLIST: &[&str] = &["key", "pem"];

/// Validate that `relative` resolves within `root`, even if the file doesn't exist.
///
/// Layer 1 (lexical): reject absolute paths and `..` components — no I/O.
/// Layer 2 (filesystem): canonicalize deepest existing ancestor, append tail,
///   verify containment via `starts_with` against canonicalized root.
/// Layer 3 (denylist): block sensitive file patterns (optional).
pub fn validate_sandboxed_path(root: &Path, relative: &str, check_denylist: bool) -> Result<PathBuf> {
    let rel_path = Path::new(relative);

    // Layer 1a: reject absolute paths
    if rel_path.is_absolute() {
        return Err(eyre!("absolute paths not allowed: {}", relative));
    }

    // Layer 1b: reject `..` components
    for component in rel_path.components() {
        if component == Component::ParentDir {
            return Err(eyre!("path traversal not allowed: {}", relative));
        }
    }

    let full = root.join(relative);

    // Layer 2: canonicalize deepest existing ancestor + append remainder
    let canonical = canonicalize_nonexistent(&full);
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    if !canonical.starts_with(&root_canonical) {
        return Err(eyre!("path escapes sandbox: {}", relative));
    }

    // Layer 3: denylist
    if check_denylist {
        check_denylist_path(&full, relative)?;
    }

    Ok(full)
}

/// Check a path against the denylist patterns and extensions.
fn check_denylist_path(full: &Path, relative: &str) -> Result<()> {
    let file_name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    for pattern in PATH_DENYLIST {
        if file_name.contains(pattern) {
            return Err(eyre!("path denied by security policy: {}", relative));
        }
    }

    if let Some(ext) = full.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        for denied_ext in EXT_DENYLIST {
            if ext_lower == *denied_ext {
                return Err(eyre!("file extension denied by security policy: {}", relative));
            }
        }
    }

    Ok(())
}

/// Canonicalize a path that may not exist by walking up to the deepest existing
/// ancestor, canonicalizing it, then appending the non-existent tail.
fn canonicalize_nonexistent(path: &Path) -> PathBuf {
    if path.exists() {
        return path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    }

    let mut ancestor = path.to_path_buf();
    let mut tail_parts: Vec<OsString> = Vec::new();

    while let Some(name) = ancestor.file_name() {
        tail_parts.push(name.to_os_string());
        if !ancestor.pop() {
            break;
        }
        if ancestor.exists() {
            break;
        }
    }

    tail_parts.reverse();
    let canonical_ancestor = ancestor.canonicalize().unwrap_or(ancestor);
    canonical_ancestor.join(tail_parts.iter().collect::<PathBuf>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    fn make_test_dir(label: &str) -> TestDir {
        TestDir::new(&format!("loopr-sandbox-{label}"))
    }

    // --- Layer 1: Lexical checks ---

    #[test]
    fn test_rejects_absolute_path() {
        let dir = make_test_dir("abs");
        let result = validate_sandboxed_path(&dir, "/etc/passwd", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("absolute"));
    }

    #[test]
    fn test_rejects_parent_dir_traversal() {
        let dir = make_test_dir("parent");
        let result = validate_sandboxed_path(&dir, "../etc/passwd", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[test]
    fn test_rejects_nested_parent_dir() {
        let dir = make_test_dir("nested-parent");
        let result = validate_sandboxed_path(&dir, "foo/../../etc/passwd", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    // --- Layer 2: Filesystem containment ---

    #[test]
    fn test_allows_existing_file_within_root() {
        let dir = make_test_dir("exist");
        std::fs::write(dir.join("test.rs"), "fn main() {}").unwrap();
        let result = validate_sandboxed_path(&dir, "test.rs", false);
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with(&dir));
    }

    #[test]
    fn test_allows_new_file_within_root() {
        let dir = make_test_dir("new-file");
        let result = validate_sandboxed_path(&dir, "src/new_file.rs", false);
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with(&dir));
    }

    #[test]
    fn test_allows_nested_nonexistent_directories() {
        let dir = make_test_dir("deep");
        let result = validate_sandboxed_path(&dir, "a/b/c/d/file.txt", false);
        assert!(result.is_ok());
        let full = result.unwrap();
        assert!(full.starts_with(&dir));
        assert!(full.ends_with("a/b/c/d/file.txt"));
    }

    #[test]
    fn test_allows_current_dir_component() {
        let dir = make_test_dir("curdir");
        std::fs::write(dir.join("test.rs"), "").unwrap();
        let result = validate_sandboxed_path(&dir, "./test.rs", false);
        assert!(result.is_ok());
    }

    // --- Layer 3: Denylist ---

    #[test]
    fn test_denylist_blocks_env_file() {
        let dir = make_test_dir("env");
        let result = validate_sandboxed_path(&dir, ".env", true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_denylist_blocks_credentials() {
        let dir = make_test_dir("cred");
        let result = validate_sandboxed_path(&dir, "credentials.json", true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_denylist_blocks_secret_file() {
        let dir = make_test_dir("secret");
        let result = validate_sandboxed_path(&dir, "my_secret_config.yml", true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_denylist_blocks_key_extension() {
        let dir = make_test_dir("key");
        let result = validate_sandboxed_path(&dir, "server.key", true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_denylist_blocks_pem_extension() {
        let dir = make_test_dir("pem");
        let result = validate_sandboxed_path(&dir, "cert.pem", true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_denylist_skipped_when_disabled() {
        let dir = make_test_dir("no-deny");
        // .env should pass when denylist is disabled
        let result = validate_sandboxed_path(&dir, ".env", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_allows_normal_file_with_denylist() {
        let dir = make_test_dir("normal");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        let result = validate_sandboxed_path(&dir, "src/main.rs", true);
        assert!(result.is_ok());
    }
}
