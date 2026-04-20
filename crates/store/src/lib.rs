//! Typed-ID and typed-error adapter over `taskstore_async::AsyncStore`. The
//! anti-corruption layer between loopr's domain types and taskstore's async
//! persistence engine.

mod error;
mod store;

pub use error::StoreError;
pub use store::Store;
