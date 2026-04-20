use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::subscriber::SharedWriter;

const WORK_ID_FIELD: &str = "work_id";

/// Per-Work log splitter. On every event, walks the enclosing span hierarchy
/// looking for a `work_id` attribute cached on span creation. If found,
/// ensures a file at `<run-dir>/work/<work_id>.log` is open and appends the
/// pretty-formatted event to it. File handles are cached in a
/// `DashMap<String, SharedWriter>` so each `work_id` opens exactly once
/// per run.
///
/// In Stage 2 this runs harmlessly - no code emits `work_id`-bearing spans
/// yet. Stage 7's first Implementer agent produces `work_id` spans and the
/// layer starts materializing files automatically.
pub struct WorkFanoutLayer {
    work_dir: PathBuf,
    cache: Arc<DashMap<String, SharedWriter>>,
}

impl WorkFanoutLayer {
    pub fn new(run_dir: &Path) -> Self {
        WorkFanoutLayer {
            work_dir: run_dir.join("work"),
            cache: Arc::new(DashMap::new()),
        }
    }

    pub(crate) fn cache_handle(&self) -> Arc<DashMap<String, SharedWriter>> {
        Arc::clone(&self.cache)
    }

    fn writer_for(&self, work_id: &str) -> Option<SharedWriter> {
        if let Some(existing) = self.cache.get(work_id) {
            return Some(existing.clone());
        }
        std::fs::create_dir_all(&self.work_dir).ok()?;
        let path = self.work_dir.join(format!("{work_id}.log"));
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        let writer = SharedWriter::new(file);
        self.cache.insert(work_id.to_string(), writer.clone());
        Some(writer)
    }
}

impl<S> Layer<S> for WorkFanoutLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = WorkIdVisitor::default();
        attrs.record(&mut visitor);
        if let Some(work_id) = visitor.work_id
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(WorkIdMarker(work_id));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(work_id) = find_work_id(event, &ctx) else {
            return;
        };
        let Some(writer) = self.writer_for(&work_id) else {
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

fn find_work_id<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> Option<String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut event_visitor = WorkIdVisitor::default();
    event.record(&mut event_visitor);
    if let Some(id) = event_visitor.work_id {
        return Some(id);
    }
    let scope = ctx.event_scope(event)?;
    for span in scope.from_root() {
        if let Some(marker) = span.extensions().get::<WorkIdMarker>() {
            return Some(marker.0.clone());
        }
    }
    None
}

#[derive(Default)]
struct WorkIdVisitor {
    work_id: Option<String>,
}

impl Visit for WorkIdVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == WORK_ID_FIELD {
            self.work_id = Some(value.to_string());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == WORK_ID_FIELD {
            let rendered = format!("{value:?}");
            let trimmed = rendered
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&rendered);
            self.work_id = Some(trimmed.to_string());
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

struct WorkIdMarker(String);
