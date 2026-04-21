//! Typed-ID and typed-error adapter over `taskstore_async::AsyncStore`. The
//! anti-corruption layer between loopr's domain types and taskstore's async
//! persistence engine.

mod error;
mod plans;
mod store;
mod works;

pub use error::StoreError;
pub use plans::PlansStore;
pub use store::{Store, TASKSTORE_SUBPATH};
pub use works::WorksStore;
