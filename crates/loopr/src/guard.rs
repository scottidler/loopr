use std::path::Path;

use crate::error::LooprError;

const SENTINEL: &str = ".loopr-source-guard";

/// Walk from `start` toward `/`, returning `Err(SourceGuardTripped)` if
/// `.loopr-source-guard` is found at any ancestor (including `start` itself).
/// Returns `Ok(())` if the walk reaches `/` without finding the sentinel.
pub fn check(start: &Path) -> Result<(), LooprError> {
    for ancestor in start.ancestors() {
        let sentinel = ancestor.join(SENTINEL);
        if sentinel.exists() {
            return Err(LooprError::SourceGuardTripped {
                path: start.to_path_buf(),
                sentinel,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
