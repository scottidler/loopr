use std::fmt::{self, Display, Formatter};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::Local;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_ALLOC_RETRIES: u32 = 1000;

/// A run identifier in `YYYYMMDD-HHMMSS[-N]` local-time format.
///
/// First run in a given second gets the clean form (`20260419-143012`);
/// subsequent runs in the same second get a disambiguator suffix
/// (`20260419-143012-2`, `20260419-143012-3`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Atomically allocate a new RunId by claiming a `.loopr/runs/<id>/`
    /// directory via `std::fs::create_dir`. The EEXIST errno is the collision
    /// signal: on EEXIST we bump the suffix and retry, starting at `-2`. The
    /// winning invocation is the one whose `create_dir` succeeded, which
    /// guarantees atomicity across concurrent loopr processes on the same
    /// target.
    pub fn allocate(runs_dir: &Path) -> Result<Self, RunIdAllocError> {
        let base = Local::now().format("%Y%m%d-%H%M%S").to_string();
        for attempt in 1..=MAX_ALLOC_RETRIES {
            let candidate = if attempt == 1 { base.clone() } else { format!("{base}-{attempt}") };
            let path = runs_dir.join(&candidate);
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(RunId(candidate)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(RunIdAllocError::Io { path, source }),
            }
        }
        Err(RunIdAllocError::MaxRetries {
            path: runs_dir.to_path_buf(),
        })
    }

    /// Parse a previously-written RunId string (e.g. from a dir listing).
    /// Validates the `YYYYMMDD-HHMMSS` skeleton and optional `-N` suffix;
    /// rejects anything else.
    pub fn parse(s: &str) -> Result<Self, RunIdParseError> {
        let (base, suffix) = if s.len() > 15 { (&s[..15], Some(&s[15..])) } else { (s, None) };
        if !is_valid_base(base) {
            return Err(RunIdParseError::Malformed(s.to_string()));
        }
        if let Some(rest) = suffix
            && (!rest.starts_with('-') || !is_valid_suffix(&rest[1..]))
        {
            return Err(RunIdParseError::Malformed(s.to_string()));
        }
        Ok(RunId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse the leading `YYYYMMDD-HHMMSS` of this id back into a
    /// `NaiveDateTime` for display in `loopr logs runs`. Suffix is ignored.
    pub fn started_at(&self) -> Option<chrono::NaiveDateTime> {
        let base = self.0.get(..15).unwrap_or(&self.0);
        chrono::NaiveDateTime::parse_from_str(base, "%Y%m%d-%H%M%S").ok()
    }
}

impl Display for RunId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RunId {
    type Err = RunIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RunId::parse(s)
    }
}

fn is_valid_base(s: &str) -> bool {
    // `YYYYMMDD-HHMMSS` = 8 digits, dash, 6 digits = 15 chars.
    if s.len() != 15 {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[8] != b'-' {
        return false;
    }
    bytes[..8].iter().all(|b| b.is_ascii_digit()) && bytes[9..].iter().all(|b| b.is_ascii_digit())
}

fn is_valid_suffix(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Error, Debug)]
pub enum RunIdParseError {
    #[error("run id `{0}` does not match YYYYMMDD-HHMMSS[-N]")]
    Malformed(String),
}

#[derive(Error, Debug)]
pub enum RunIdAllocError {
    /// Allocation retried past the 1000-attempt cap without claiming a free id.
    /// In practice this only fires if the runs directory has ~1000 colliding
    /// ids in the same wall-clock second; treated as unrecoverable.
    #[error("exhausted 1000 allocation retries under {path}", path = .path.display())]
    MaxRetries { path: PathBuf },
    /// `create_dir` failed for a reason other than EEXIST (permissions, ENOSPC,
    /// read-only filesystem, ...). Preserved so retry strategies at higher
    /// layers can distinguish "disk full" from "just collided a lot."
    #[error("failed to create run dir {path}: {source}", path = .path.display())]
    Io { path: PathBuf, source: io::Error },
}
