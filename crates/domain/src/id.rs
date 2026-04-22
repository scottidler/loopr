//! ID generation primitives and the `id_type!` macro.
//!
//! `generate_id(prefix)` produces `{prefix}-{5-char-base36}` — an 8-char ID
//! readable in logs and sized to 36^5 ≈ 60M per-prefix entropy, sufficient for
//! any single repo's record cardinality.
//!
//! `now_millis()` returns wall-clock Unix time in milliseconds.
//!
//! `id_type!` stamps out a `String`-backed newtype with the derives, serde
//! posture, and `AsRef<str>` / `Display` / `FromStr` impls that
//! `#[derive(Record)]` and downstream Store code expect. See the macro's
//! doc-comment for its contract.

use rand::RngExt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Generate an ID of the form `{prefix}-{5-char base36}`.
///
/// `prefix` is expected to be 2 lowercase ASCII chars (by convention). The
/// body uses the digits `0-9` then lowercase `a-z`.
pub fn generate_id(prefix: &str) -> String {
    let mut rng = rand::rng();
    let code: String = (0..5)
        .map(|_| {
            let idx = rng.random_range(0..36u8);
            if idx < 10 { (b'0' + idx) as char } else { (b'a' + idx - 10) as char }
        })
        .collect();
    format!("{prefix}-{code}")
}

/// Return the current Unix timestamp in milliseconds.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as i64
}

/// Declare a typed-ID newtype with a fixed 2-char prefix.
///
/// Invocation:
///
/// ```ignore
/// id_type!(PlanId, "pl");
/// ```
///
/// Generates a public `struct PlanId(String)` with:
///
/// - derives: `Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize`
/// - `#[serde(transparent)]` so the wire form is the inner string, not a
///   one-element array
/// - `impl AsRef<str>` — required by `#[derive(Record)]` on consumer structs
/// - `impl Display` — `#[derive(Record)]` calls `ToString::to_string` on
///   indexed fields; this lets a typed ID be indexed if a record ever needs it
/// - `impl FromStr<Err = Infallible>` — symmetric serde round-trip; no
///   format validation (trusted store)
/// - `PlanId::new()` calling `generate_id("pl")`
/// - `PlanId::prefix()` returning `"pl"`
///
/// Constraints (by convention, enforced by code review):
///
/// - Prefix must be a 2-char lowercase ASCII string literal. Longer breaks
///   the 8-char ID convention; shorter loses readability; uppercase clashes
///   with the lowercase base36 body.
/// - Inner type is fixed at `String`.
/// - Derive set is fixed.
///
/// This macro exists to collapse the seven near-identical ID newtypes across
/// Plan/Spec/Phase/Work/Bundle/Tick/Run. Any record wanting a different shape
/// (different derive set, different inner type, different ID format) writes a
/// manual newtype instead. No v5 first-gate record needs that escape hatch.
#[macro_export]
macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(
            ::std::fmt::Debug,
            ::std::clone::Clone,
            ::std::cmp::PartialEq,
            ::std::cmp::Eq,
            ::std::hash::Hash,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(::std::string::String);

        impl $name {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self($crate::id::generate_id($prefix))
            }

            pub fn prefix() -> &'static str {
                $prefix
            }
        }

        impl ::std::convert::AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.0.as_ref()
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = ::std::convert::Infallible;
            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                ::std::result::Result::Ok(Self(::std::string::ToString::to_string(s)))
            }
        }
    };
}

id_type!(PlanId, "pl");
id_type!(WorkId, "wk");
id_type!(BundleId, "bd");
id_type!(TickId, "tk");

#[cfg(test)]
mod tests;
