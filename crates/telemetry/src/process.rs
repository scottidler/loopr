use std::fmt::{self, Display, Formatter};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rand::RngExt;
use rand::distr::Alphanumeric;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_ALLOC_RETRIES: u32 = 1000;
const SLUG_LEN: usize = 6;
const PREFIX: &str = "pc-";

/// A process identifier in `pc-<6char>` lowercase alphanumeric slug form.
///
/// One per loopr OS process (daemon boot, client CLI invocation). Unlike
/// `SessionId` (which is timestamp-shaped and user-facing), `ProcessId` is
/// opaque: processes are numerous and ephemeral, and a short random slug
/// is the right shape for filesystem fanout.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessId(String);

impl ProcessId {
    /// Atomically allocate a new ProcessId by claiming `<runs_dir>/<id>/`
    /// via `std::fs::create_dir`. Same EEXIST-race pattern as
    /// `SessionId::allocate`: on collision, re-roll the slug and retry.
    pub fn allocate(runs_dir: &Path) -> Result<Self, ProcessIdAllocError> {
        let mut rng = rand::rng();
        for _ in 0..MAX_ALLOC_RETRIES {
            let slug: String = (&mut rng)
                .sample_iter(Alphanumeric)
                .take(SLUG_LEN)
                .map(|b| (b as char).to_ascii_lowercase())
                .collect();
            let candidate = format!("{PREFIX}{slug}");
            let path = runs_dir.join(&candidate);
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(ProcessId(candidate)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(ProcessIdAllocError::Io { path, source }),
            }
        }
        Err(ProcessIdAllocError::MaxRetries {
            path: runs_dir.to_path_buf(),
        })
    }

    /// Parse a previously-written ProcessId string. Validates the `pc-`
    /// prefix and a 6-char lowercase alphanumeric slug.
    pub fn parse(s: &str) -> Result<Self, ProcessIdParseError> {
        if !is_valid(s) {
            return Err(ProcessIdParseError::Malformed(s.to_string()));
        }
        Ok(ProcessId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ProcessId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ProcessId {
    type Err = ProcessIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ProcessId::parse(s)
    }
}

fn is_valid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix(PREFIX) else {
        return false;
    };
    rest.len() == SLUG_LEN && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

#[derive(Error, Debug)]
pub enum ProcessIdParseError {
    #[error("process id `{0}` does not match pc-<6 lowercase alnum>")]
    Malformed(String),
}

#[derive(Error, Debug)]
pub enum ProcessIdAllocError {
    /// Allocation retried past the cap without claiming a free id.
    /// With ~2 billion possible slugs and retries bounded at 1000, this
    /// only fires if the runs dir is catastrophically full.
    #[error("exhausted {MAX_ALLOC_RETRIES} allocation retries under {path}", path = .path.display())]
    MaxRetries { path: PathBuf },
    /// `create_dir` failed for a reason other than EEXIST. Preserved so
    /// higher layers can distinguish "disk full" from "just collided a lot."
    #[error("failed to create process dir {path}: {source}", path = .path.display())]
    Io { path: PathBuf, source: io::Error },
}
