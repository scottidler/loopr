use std::fmt::{self, Display, Formatter};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::Local;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_ALLOC_RETRIES: u32 = 1000;

/// A session identifier in `YYYYMMDD-HHMMSS[-N]` local-time format.
///
/// First session in a given second gets the clean form (`20260419-143012`);
/// subsequent sessions in the same second get a disambiguator suffix
/// (`20260419-143012-2`, `20260419-143012-3`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Atomically allocate a new SessionId by claiming an `<anchor>/<id>/`
    /// directory via `std::fs::create_dir`. The EEXIST errno is the collision
    /// signal: on EEXIST we bump the suffix and retry, starting at `-2`. The
    /// winning invocation is the one whose `create_dir` succeeded, which
    /// guarantees atomicity across concurrent loopr processes sharing the
    /// same `anchor` dir.
    ///
    /// The `anchor` is `<xdg>/loopr/sessions/` as of Phase 5; callers in
    /// `loopr::session` compose it before invoking.
    pub fn allocate(anchor: &Path) -> Result<Self, SessionIdAllocError> {
        let base = Local::now().format("%Y%m%d-%H%M%S").to_string();
        for attempt in 1..=MAX_ALLOC_RETRIES {
            let candidate = if attempt == 1 { base.clone() } else { format!("{base}-{attempt}") };
            let path = anchor.join(&candidate);
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(SessionId(candidate)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(SessionIdAllocError::Io { path, source }),
            }
        }
        Err(SessionIdAllocError::MaxRetries {
            path: anchor.to_path_buf(),
        })
    }

    /// Parse a previously-written SessionId string (e.g. from a dir listing).
    /// Validates the `YYYYMMDD-HHMMSS` skeleton and optional `-N` suffix;
    /// rejects anything else.
    pub fn parse(s: &str) -> Result<Self, SessionIdParseError> {
        // Byte-slice (`&s[..15]`) panics when byte 15 is not a UTF-8 char
        // boundary; `query.rs` feeds raw directory names here, so a
        // multibyte dir name must yield `Malformed`, not a panic. `get`
        // returns `None` on a non-boundary split, falling through to
        // `(s, None)` which then fails `is_valid_base`.
        let (base, suffix) = if s.len() > 15 {
            match (s.get(..15), s.get(15..)) {
                (Some(b), Some(rest)) => (b, Some(rest)),
                _ => (s, None),
            }
        } else {
            (s, None)
        };
        if !is_valid_base(base) {
            return Err(SessionIdParseError::Malformed(s.to_string()));
        }
        if let Some(rest) = suffix
            && (!rest.starts_with('-') || !is_valid_suffix(&rest[1..]))
        {
            return Err(SessionIdParseError::Malformed(s.to_string()));
        }
        Ok(SessionId(s.to_string()))
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

impl Display for SessionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SessionId {
    type Err = SessionIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SessionId::parse(s)
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
pub enum SessionIdParseError {
    #[error("session id `{0}` does not match YYYYMMDD-HHMMSS[-N]")]
    Malformed(String),
}

#[derive(Error, Debug)]
pub enum SessionIdAllocError {
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
