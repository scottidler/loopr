use std::fs::OpenOptions;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::subscriber::SharedWriter;

/// Span field names the layer routes on.
///
/// Client-side invocations set `session_id` at span creation (see
/// `loopr.invocation` in `loopr::run`). The daemon's `ipc.connection` span
/// is created without a session and `record`s `client_session_id` after
/// `system.handshake` completes — two different concepts (this process's
/// session vs. the caller's session) that share a routing scheme. The
/// layer accepts either field name, picking up whichever the emitter
/// carries.
const SESSION_ID_FIELDS: &[&str] = &["session_id", "client_session_id"];

/// LRU cap on open session log writers. Sessions are long-lived, so a
/// small bound is sufficient to prevent file-handle exhaustion under
/// pathological daemon-lifetime session churn. Eviction closes the file
/// handle; a subsequent event for the evicted session re-opens in append
/// mode, preserving log continuity.
const DEFAULT_CACHE_CAP: usize = 16;

/// Per-Session log splitter. When an event (directly or via enclosing
/// spans) carries a `session_id` / `client_session_id` field, append it
/// to `<xdg_root>/sessions/<id>/targets/<target_slug>/session-fanout.log`.
///
/// Parallels `WorkFanoutLayer`, with two structural differences:
///
///   - Writers live behind an `LruCache` so long-running daemons handling
///     many sessions do not accumulate unbounded file handles. Handles
///     are re-opened (in append mode) on demand after eviction.
///   - `on_record` is implemented in addition to `on_new_span`. The
///     daemon's handshake path sets `client_session_id` via
///     `Span::record` AFTER span creation, which fires `Layer::on_record`
///     and not `Layer::on_new_span`; without this hook, connection-scoped
///     events after a handshake would appear as unattributed.
///
/// Concurrency note: multiple processes (daemon + every client that
/// attaches to the session) open `session-fanout.log` in append mode.
/// POSIX `O_APPEND` is atomic up to `PIPE_BUF` (4 KiB on Linux); pretty-
/// formatted tracing events are well under that, so interleaved writes
/// arrive line-aligned in practice. Best-effort for first-gate.
pub struct SessionFanoutLayer {
    xdg_root: PathBuf,
    target_slug: String,
    cache: Arc<Mutex<LruCache<String, SharedWriter>>>,
}

impl SessionFanoutLayer {
    /// New layer with the default cache cap (16).
    pub fn new(xdg_root: PathBuf, target_slug: String) -> Self {
        Self::with_capacity(xdg_root, target_slug, DEFAULT_CACHE_CAP)
    }

    /// New layer with an explicit cache cap. `cap` is clamped to at least 1.
    pub fn with_capacity(xdg_root: PathBuf, target_slug: String, cap: usize) -> Self {
        let cap = NonZeroUsize::new(cap.max(1)).expect("cap clamped above zero");
        SessionFanoutLayer {
            xdg_root,
            target_slug,
            cache: Arc::new(Mutex::new(LruCache::new(cap))),
        }
    }

    pub(crate) fn cache_handle(&self) -> Arc<Mutex<LruCache<String, SharedWriter>>> {
        Arc::clone(&self.cache)
    }

    fn writer_for(&self, session_id: &str) -> Option<SharedWriter> {
        let mut cache = self.cache.lock().ok()?;
        if let Some(existing) = cache.get(session_id) {
            return Some(existing.clone());
        }
        let dir = self
            .xdg_root
            .join("sessions")
            .join(session_id)
            .join("targets")
            .join(&self.target_slug);
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("session-fanout.log");
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        let writer = SharedWriter::new(file);
        cache.put(session_id.to_string(), writer.clone());
        Some(writer)
    }
}

impl<S> Layer<S> for SessionFanoutLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = SessionIdVisitor::default();
        attrs.record(&mut visitor);
        if let Some(session_id) = visitor.session_id
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(SessionIdMarker(session_id));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let mut visitor = SessionIdVisitor::default();
        values.record(&mut visitor);
        let Some(session_id) = visitor.session_id else {
            return;
        };
        let Some(span) = ctx.span(id) else {
            return;
        };
        // Replace any previous marker. Only one session-id routes a given
        // span; a later record call authoritatively overwrites.
        span.extensions_mut().insert(SessionIdMarker(session_id));
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(session_id) = find_session_id(event, &ctx) else {
            return;
        };
        let Some(writer) = self.writer_for(&session_id) else {
            return;
        };
        let Ok(mut guard) = writer.0.lock() else {
            return;
        };
        let mut visitor = PrettyEventVisitor::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        let _ = writeln!(
            guard,
            "{} [{}] {}: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            metadata.level(),
            metadata.target(),
            visitor.message,
        );
    }
}

fn find_session_id<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> Option<String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut event_visitor = SessionIdVisitor::default();
    event.record(&mut event_visitor);
    if let Some(id) = event_visitor.session_id {
        return Some(id);
    }
    let scope = ctx.event_scope(event)?;
    for span in scope.from_root() {
        if let Some(marker) = span.extensions().get::<SessionIdMarker>() {
            return Some(marker.0.clone());
        }
    }
    None
}

#[derive(Default)]
struct SessionIdVisitor {
    session_id: Option<String>,
}

impl Visit for SessionIdVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if SESSION_ID_FIELDS.contains(&field.name()) && !value.is_empty() {
            self.session_id = Some(value.to_string());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if SESSION_ID_FIELDS.contains(&field.name()) {
            let rendered = format!("{value:?}");
            let trimmed = rendered
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&rendered);
            if !trimmed.is_empty() {
                self.session_id = Some(trimmed.to_string());
            }
        }
    }
}

#[derive(Default)]
struct PrettyEventVisitor {
    message: String,
}

impl Visit for PrettyEventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.message.push_str(&format!(" {}={value}", field.name()));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.message.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
}

struct SessionIdMarker(String);
