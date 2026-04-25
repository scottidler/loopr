//! Phase 3 acceptance test for the instrumentation sweep.
//!
//! Opens a Store at a tempdir, exercises one create+get on each
//! collection, and asserts the per-method spans appear with required
//! scope fields (`record_kind`, `record_id`, `op`).

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use domain::{Bundle, Plan, Tick, Work};
use store::Store;

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

fn capture_subscriber() -> (Arc<SpanCapture>, impl tracing::Subscriber) {
    let cap = Arc::new(SpanCapture::default());
    let layer = CaptureLayer { capture: cap.clone() };
    let sub = Registry::default().with(EnvFilter::new("trace")).with(layer);
    (cap, sub)
}

#[tokio::test(flavor = "current_thread")]
async fn store_smoke_spans_each_collection() {
    let td = TempDir::new().unwrap();
    let (cap, sub) = capture_subscriber();
    let _g = tracing::subscriber::set_default(sub);

    let store = Store::open(td.path()).await.unwrap();

    let plan = Plan::new("smoke".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let _ = store.plans().get(&plan.id).await.unwrap();
    let _ = store.plans().list().await.unwrap();

    let work = Work::new(plan.id.clone(), "the work".to_string());
    store.works().create(work.clone()).await.unwrap();
    let _ = store.works().get(&work.id).await.unwrap();
    let _ = store.works().list_by_parent_id(&plan.id).await.unwrap();

    let bundle = Bundle::new(work.id.clone(), "feat/x".to_string(), vec!["claim".to_string()]);
    store.bundles().create(bundle.clone()).await.unwrap();
    let _ = store.bundles().get(&bundle.id).await.unwrap();
    let _ = store.bundles().list_by_work_id(&work.id).await.unwrap();

    let tick = Tick::new(
        plan.id.clone(),
        vec![bundle.id.clone()],
        "integration".to_string(),
        "abc1234".to_string(),
        vec!["abc1234".to_string()],
    );
    store.ticks().create(tick.clone()).await.unwrap();
    let _ = store.ticks().get(&tick.id).await.unwrap();
    let _ = store.ticks().list_by_plan_id(&plan.id).await.unwrap();

    store.close().await.unwrap();

    // Open / close span fields.
    let open = cap.find("store.open").expect("store.open");
    assert!(open.fields.contains_key("target"), "open: {:?}", open.fields);
    cap.find("store.close").expect("store.close");

    // One representative span per collection's create with required scope fields.
    let plans_create = cap.find("plans.create").expect("plans.create");
    assert_eq!(plans_create.fields.get("record_kind").map(String::as_str), Some("plan"));
    assert_eq!(plans_create.fields.get("op").map(String::as_str), Some("create"));
    assert!(
        plans_create.fields.contains_key("record_id"),
        "plans.create: {:?}",
        plans_create.fields
    );

    let works_create = cap.find("works.create").expect("works.create");
    assert_eq!(works_create.fields.get("record_kind").map(String::as_str), Some("work"));
    assert!(works_create.fields.contains_key("parent_id"));

    let bundles_create = cap.find("bundles.create").expect("bundles.create");
    assert_eq!(
        bundles_create.fields.get("record_kind").map(String::as_str),
        Some("bundle")
    );
    assert!(bundles_create.fields.contains_key("work_id"));

    let ticks_create = cap.find("ticks.create").expect("ticks.create");
    assert_eq!(ticks_create.fields.get("record_kind").map(String::as_str), Some("tick"));
    assert!(ticks_create.fields.contains_key("plan_id"));
    assert_eq!(ticks_create.fields.get("bundle_count").map(String::as_str), Some("1"));

    // Get/list spans should also fire.
    cap.find("plans.get").expect("plans.get");
    cap.find("plans.list").expect("plans.list");
    cap.find("works.list_by_parent_id").expect("works.list_by_parent_id");
    cap.find("bundles.list_by_work_id").expect("bundles.list_by_work_id");
    cap.find("ticks.list_by_plan_id").expect("ticks.list_by_plan_id");
}
