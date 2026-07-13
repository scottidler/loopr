//! Test-only support for asserting on emitted `tracing` events.
//!
//! # Why a process-global default is required
//!
//! Several tests here install a thread-local capturing subscriber via
//! `tracing::subscriber::set_default` and then assert on a specific
//! `warn!`/`error!` line emitted by the code under test. Without the global
//! default installed by [`ensure_global_interested_default`], those
//! assertions are flaky (~16% over 150 full-binary runs), intermittently
//! seeing zero captured lines.
//!
//! The mechanism is `tracing`'s process-global per-callsite interest cache.
//! A callsite's `Interest` is computed once, lazily, the first time the
//! callsite executes, and cached for the whole process. When only zero or
//! one dispatcher is globally registered, `tracing-core` computes that
//! interest via the *registering thread's* thread-local default
//! (`Rebuilder::JustOne` -> `dispatcher::get_default`). A sibling test that
//! has no capturing subscriber can therefore be the first to hit a shared
//! `warn!`/`error!` callsite: on its thread the default is `NoSubscriber`,
//! which returns `Interest::never()`, and that `never` is cached globally.
//! A later capturing test that asserts on the same callsite then sees the
//! cached `never`, the event is dropped before dispatch, and its buffer is
//! empty. A thread-local `set_default` cannot fix this: it lives only on the
//! asserting thread, and for a callsite that fires after an `.await` the
//! poison happens in a sibling during the yield window.
//!
//! Installing a global default that is interested in every callsite makes
//! `get_default` return an interested subscriber on *every* thread, so no
//! callsite is ever cached `never`. Capturing tests still layer their own
//! thread-local subscriber on top; the global default only discards. This
//! is exactly the "a program should set a global default" idiom the
//! `tracing` docs describe.

use std::sync::Once;

/// A global default subscriber that reports interest in every callsite but
/// discards all spans and events. Its only job is to keep the process-global
/// interest cache from ever resolving a callsite to `never` on a thread that
/// has no thread-local subscriber.
struct InterestedDiscard;

impl tracing::Subscriber for InterestedDiscard {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

static INIT: Once = Once::new();

/// Install [`InterestedDiscard`] as the process-global default `tracing`
/// subscriber exactly once. Every log-capturing test MUST call this before
/// installing its thread-local capturing subscriber (the per-module
/// `set_capturing_default` helper does so). Idempotent and cheap after the
/// first call.
pub(crate) fn ensure_global_interested_default() {
    INIT.call_once(|| {
        // Ignore the result: another test binary path or a prior init may
        // already have set a global default, and that is fine.
        let _ = tracing::subscriber::set_global_default(InterestedDiscard);
    });
}
