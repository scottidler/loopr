//! Phase 6 acceptance test for the instrumentation sweep.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use context::{ContextBuilder, InlineContextBuilder, StateSummary};
use domain::{AcceptanceCriteria, Bundle, Plan, Work};

#[derive(Debug, Default, Clone)]
struct CapturedSpan {
    name: String,
    fields: BTreeMap<String, String>,
}

#[derive(Default)]
struct SpanCapture {
    spans: Mutex<Vec<CapturedSpan>>,
}

impl SpanCapture {
    fn find(&self, name: &str) -> Option<CapturedSpan> {
        self.spans.lock().unwrap().iter().find(|s| s.name == name).cloned()
    }
}

#[derive(Default)]
struct StringFieldVisitor(BTreeMap<String, String>);

impl Visit for StringFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

struct CaptureLayer {
    capture: Arc<SpanCapture>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, _id: &span::Id, _ctx: Context<'_, S>) {
        let mut visitor = StringFieldVisitor::default();
        attrs.record(&mut visitor);
        self.capture.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields: visitor.0,
        });
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let span = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };
        let name = span.metadata().name().to_string();
        let mut visitor = StringFieldVisitor::default();
        values.record(&mut visitor);
        let mut spans = self.capture.spans.lock().unwrap();
        if let Some(rec) = spans.iter_mut().rfind(|s| s.name == name) {
            rec.fields.extend(visitor.0);
        }
    }
}

#[test]
fn context_smoke_spans_implementer_and_reviewer() {
    let cap = Arc::new(SpanCapture::default());
    let layer = CaptureLayer { capture: cap.clone() };
    let sub = Registry::default().with(EnvFilter::new("trace")).with(layer);
    let _g = tracing::subscriber::set_default(sub);

    let plan = Plan::new("instrumentation".to_string());
    let mut work = Work::new(plan.id.clone(), "deliver feature".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["it ships".to_string()]);

    let builder = InlineContextBuilder::new();
    let _ = builder
        .build_for_implementer(&work, &PathBuf::from("/tmp/wt"), &[], &[], &StateSummary::default(), 1)
        .unwrap();

    let mut bundle = Bundle::new(work.id.clone(), "feat/x".to_string(), vec!["claim".to_string()]);
    bundle.head_commit = Some("abc1234".to_string());
    let _ = builder
        .build_for_reviewer(&bundle, &work, "diff --git a/x b/x", None)
        .unwrap();

    let imp = cap
        .find("context.build_for_implementer")
        .expect("context.build_for_implementer span");
    assert_eq!(imp.fields.get("role").map(String::as_str), Some("implementer"));
    assert_eq!(
        imp.fields.get("work_id").map(String::as_str),
        Some(&*work.id.to_string())
    );
    assert_eq!(imp.fields.get("iteration").map(String::as_str), Some("1"));
    assert!(
        imp.fields.contains_key("system_chars"),
        "missing system_chars: {:?}",
        imp.fields
    );
    assert!(imp.fields.contains_key("user_chars"));
    assert!(imp.fields.contains_key("token_estimate"));

    let rev = cap
        .find("context.build_for_reviewer")
        .expect("context.build_for_reviewer span");
    assert_eq!(rev.fields.get("role").map(String::as_str), Some("reviewer"));
    assert_eq!(
        rev.fields.get("bundle_id").map(String::as_str),
        Some(&*bundle.id.to_string())
    );
    assert!(rev.fields.contains_key("diff_chars"));
    assert!(rev.fields.contains_key("system_chars"));
}
