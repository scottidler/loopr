//! Typed-ID and typed-error adapter over `taskstore_async::AsyncStore`. The
//! anti-corruption layer between loopr's domain types and taskstore's async
//! persistence engine.

mod bundles;
mod checks;
mod error;
mod notes;
mod plans;
mod reviews;
mod store;
mod ticks;
mod works;

pub use bundles::{BundleUpdateError, BundleUpdateSink, BundlesStore};
pub use checks::{CheckRunSink, CheckRunsStore};
pub use error::StoreError;
pub use notes::NotesStore;
pub use plans::{PlanUpdateError, PlanUpdateSink, PlansStore};
pub use reviews::ReviewsStore;
pub use store::{STORE_VERSION, Store, TASKSTORE_SUBPATH};
pub use ticks::TicksStore;
pub use works::{WorkUpdateError, WorkUpdateSink, WorksStore};

// Re-exports from taskstore for the corruption-tolerant read path used by
// the daemon's reconcile sweep. Surfacing them via `store::*` keeps the
// downstream `loopr` crate from naming `taskstore_traits` directly.
// `taskstore_async` re-exports these from `taskstore_traits`.
pub use taskstore_async::{Category, CorruptionEntry, CorruptionError, ListResult};
