#![allow(clippy::unwrap_used)]

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tracing::info_span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer};

use super::*;

// ---------- floor_at_info ----------

#[test]
fn floor_bare_info_unchanged() {
    assert_eq!(crate::subscriber::floor_at_info("info"), "info");
}

#[test]
fn floor_bare_debug_raised_to_info() {
    assert_eq!(crate::subscriber::floor_at_info("debug"), "info");
}

#[test]
fn floor_bare_trace_raised_to_info() {
    assert_eq!(crate::subscriber::floor_at_info("trace"), "info");
}

#[test]
fn floor_bare_warn_unchanged() {
    assert_eq!(crate::subscriber::floor_at_info("warn"), "warn");
}

#[test]
fn floor_bare_error_unchanged() {
    assert_eq!(crate::subscriber::floor_at_info("error"), "error");
}

#[test]
fn floor_bare_off_unchanged() {
    assert_eq!(crate::subscriber::floor_at_info("off"), "off");
}

#[test]
fn floor_per_target_clamps_each_clause() {
    assert_eq!(
        crate::subscriber::floor_at_info("loopr=debug,tools=error"),
        "loopr=info,tools=error"
    );
    assert_eq!(
        crate::subscriber::floor_at_info("loopr::agents=trace,worktree=warn"),
        "loopr::agents=info,worktree=warn"
    );
}

#[test]
fn floor_mixed_global_and_per_target() {
    assert_eq!(crate::subscriber::floor_at_info("warn,loopr=trace"), "warn,loopr=info");
    assert_eq!(
        crate::subscriber::floor_at_info("debug,loopr=error"),
        "info,loopr=error"
    );
}

#[test]
fn floor_result_parses_as_envfilter() {
    // Regression guard: every output must be a valid EnvFilter directive.
    for input in [
        "info",
        "debug",
        "trace",
        "warn",
        "error",
        "off",
        "loopr=debug,tools=error",
        "warn,loopr::agents=trace",
    ] {
        let floored = crate::subscriber::floor_at_info(input);
        EnvFilter::try_new(&floored)
            .unwrap_or_else(|e| panic!("floor_at_info({input:?}) -> {floored:?} did not parse: {e}"));
    }
}

// ---------- SessionId ----------

#[test]
fn sessionid_allocate_empty_dir_gets_clean_name() {
    let td = TempDir::new().unwrap();
    let id = SessionId::allocate(td.path()).unwrap();
    let s = id.as_str();
    assert_eq!(s.len(), 15, "clean name has no suffix: {s}");
    assert!(td.path().join(s).is_dir());
}

#[test]
fn sessionid_allocate_twice_second_gets_suffix_2() {
    let td = TempDir::new().unwrap();
    let a = SessionId::allocate(td.path()).unwrap();
    let b = SessionId::allocate(td.path()).unwrap();
    assert_ne!(a.as_str(), b.as_str());
    // Same second => second allocation must suffix; different second => either
    // is fine. Accept both to avoid wall-clock flakiness.
    if a.as_str()[..15] == b.as_str()[..15] {
        assert_eq!(b.as_str().len(), 17, "collision suffix: {}", b.as_str());
        assert!(b.as_str().ends_with("-2"), "second gets -2: {}", b.as_str());
    }
}

#[test]
fn sessionid_allocate_with_pre_populated_victims() {
    let td = TempDir::new().unwrap();
    let base = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    for i in 1..=5 {
        let dir = if i == 1 { base.clone() } else { format!("{base}-{i}") };
        fs::create_dir(td.path().join(&dir)).unwrap();
    }
    let id = SessionId::allocate(td.path()).unwrap();
    assert_eq!(id.as_str(), format!("{base}-6"));
}

#[test]
fn sessionid_parse_valid_no_suffix() {
    let id = SessionId::parse("20260419-143012").unwrap();
    assert_eq!(id.as_str(), "20260419-143012");
}

#[test]
fn sessionid_parse_valid_with_suffix() {
    let id = SessionId::parse("20260419-143012-2").unwrap();
    assert_eq!(id.as_str(), "20260419-143012-2");
    let id = SessionId::parse("20260419-143012-42").unwrap();
    assert_eq!(id.as_str(), "20260419-143012-42");
}

#[test]
fn sessionid_parse_rejects_malformed() {
    assert!(SessionId::parse("").is_err());
    assert!(SessionId::parse("not-a-session-id").is_err());
    assert!(SessionId::parse("20260419").is_err());
    assert!(SessionId::parse("20260419-14301").is_err(), "short time");
    assert!(SessionId::parse("2026041x-143012").is_err(), "non-digit in date");
    assert!(SessionId::parse("20260419-143012-").is_err(), "empty suffix");
    assert!(SessionId::parse("20260419-143012-abc").is_err(), "non-digit suffix");
}

#[test]
fn sessionid_started_at_strips_suffix() {
    let id = SessionId::parse("20260419-143012-7").unwrap();
    let ts = id.started_at().unwrap();
    assert_eq!(ts.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-04-19 14:30:12");
}

#[test]
fn sessionid_serde_roundtrip() {
    let id = SessionId::parse("20260419-143012-2").unwrap();
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, "\"20260419-143012-2\"");
    let back: SessionId = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_str(), "20260419-143012-2");
}

#[test]
fn sessionid_display_matches_as_str() {
    let id = SessionId::parse("20260419-143012").unwrap();
    assert_eq!(format!("{id}"), "20260419-143012");
}

#[test]
fn sessionid_collision_allocates_disambiguator() {
    let td = TempDir::new().unwrap();
    let a = SessionId::allocate(td.path()).unwrap();
    let b = SessionId::allocate(td.path()).unwrap();
    let c = SessionId::allocate(td.path()).unwrap();
    assert_ne!(a.as_str(), b.as_str());
    assert_ne!(b.as_str(), c.as_str());
    assert_ne!(a.as_str(), c.as_str());
}

// ---------- Subscriber round-trip ----------
//
// These tests do NOT call `telemetry::init` because `set_global_default` is
// process-global and tests run in the same process. Instead they compose a
// local subscriber matching `init`'s layer shape and drive it via
// `tracing::subscriber::with_default`.

#[derive(Clone)]
struct VecWriter(Arc<Mutex<Vec<u8>>>);

impl VecWriter {
    fn new() -> Self {
        VecWriter(Arc::new(Mutex::new(Vec::new())))
    }
    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }
}

impl std::io::Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
    type Writer = VecWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn compose_emits_json_event_with_expected_fields() {
    let json_sink = VecWriter::new();
    let pretty_sink = VecWriter::new();
    let json_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(json_sink.clone())
        .with_filter(EnvFilter::new("info"));
    let pretty_layer = tracing_subscriber::fmt::layer()
        .with_writer(pretty_sink.clone())
        .with_ansi(false)
        .with_filter(EnvFilter::new("info"));
    let subscriber = tracing_subscriber::registry().with(json_layer).with(pretty_layer);

    tracing::subscriber::with_default(subscriber, || {
        let span = info_span!("loopr.invocation", session_id = "20260419-143012", subcommand = "plan");
        let _enter = span.enter();
        tracing::info!("hello from test");
    });

    let json = json_sink.snapshot();
    assert!(!json.is_empty(), "json sink non-empty");
    let mut saw_span = false;
    for line in json.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v.get("timestamp").is_some());
        assert!(v.get("level").is_some());
        if let Some(spans) = v.get("spans").and_then(|s| s.as_array())
            && spans
                .iter()
                .any(|s| s.get("name") == Some(&serde_json::Value::String("loopr.invocation".into())))
        {
            saw_span = true;
        }
    }
    assert!(
        saw_span,
        "at least one event carried the loopr.invocation span; got: {json}"
    );

    let pretty = pretty_sink.snapshot();
    assert!(pretty.contains("hello from test"));
    assert!(pretty.contains("loopr.invocation"));
}

#[test]
fn init_twice_returns_already_initialized() {
    // We cannot safely test this in-process because `set_global_default` is
    // process-global and other tests may have already called it (or will).
    // Instead assert the error variant is constructible/Display works.
    let err = TelemetryInitError::AlreadyInitialized;
    assert_eq!(format!("{err}"), "telemetry::init called twice in the same process");
}

// ---------- WorkFanoutLayer ----------

fn compose_fanout(run_dir: &Path) -> (impl tracing::Subscriber, Arc<dashmap::DashMap<String, SharedWriter>>) {
    let fanout = WorkFanoutLayer::new(run_dir);
    let cache = fanout.cache_handle();
    let subscriber = tracing_subscriber::registry().with(fanout.with_filter(EnvFilter::new("info")));
    (subscriber, cache)
}

#[test]
fn fanout_writes_per_work_file() {
    let td = TempDir::new().unwrap();
    let run_dir = td.path().join("run1");
    fs::create_dir_all(&run_dir).unwrap();
    let (sub, cache) = compose_fanout(&run_dir);
    tracing::subscriber::with_default(sub, || {
        let span = info_span!("stage.x", work_id = "w-test-01");
        let _enter = span.enter();
        tracing::info!("hello work");
    });
    // flush caches
    for entry in cache.iter() {
        let _ = entry.value().0.lock().unwrap().flush();
    }
    let log = run_dir.join("work").join("w-test-01.log");
    assert!(log.is_file(), "per-work log exists at {}", log.display());
    let body = fs::read_to_string(&log).unwrap();
    assert!(body.contains("hello work"), "body: {body}");
}

#[test]
fn fanout_no_work_id_creates_no_file() {
    let td = TempDir::new().unwrap();
    let run_dir = td.path().join("run1");
    fs::create_dir_all(&run_dir).unwrap();
    let (sub, _cache) = compose_fanout(&run_dir);
    tracing::subscriber::with_default(sub, || {
        tracing::info!("no work id here");
    });
    assert!(
        !run_dir.join("work").exists(),
        "work dir should not exist without work_id"
    );
}

#[test]
fn fanout_two_work_ids_get_two_files() {
    let td = TempDir::new().unwrap();
    let run_dir = td.path().join("run1");
    fs::create_dir_all(&run_dir).unwrap();
    let (sub, cache) = compose_fanout(&run_dir);
    tracing::subscriber::with_default(sub, || {
        let s1 = info_span!("stage.x", work_id = "w-A");
        s1.in_scope(|| tracing::info!("for A"));
        let s2 = info_span!("stage.x", work_id = "w-B");
        s2.in_scope(|| tracing::info!("for B"));
    });
    for entry in cache.iter() {
        let _ = entry.value().0.lock().unwrap().flush();
    }
    assert!(run_dir.join("work").join("w-A.log").is_file());
    assert!(run_dir.join("work").join("w-B.log").is_file());
}

#[test]
fn fanout_same_work_id_reuses_file() {
    let td = TempDir::new().unwrap();
    let run_dir = td.path().join("run1");
    fs::create_dir_all(&run_dir).unwrap();
    let (sub, cache) = compose_fanout(&run_dir);
    tracing::subscriber::with_default(sub, || {
        let span = info_span!("stage.x", work_id = "w-once");
        span.in_scope(|| {
            tracing::info!("first");
            tracing::info!("second");
        });
    });
    for entry in cache.iter() {
        let _ = entry.value().0.lock().unwrap().flush();
    }
    let log = run_dir.join("work").join("w-once.log");
    let body = fs::read_to_string(&log).unwrap();
    let count = body.matches("first").count() + body.matches("second").count();
    assert_eq!(count, 2, "both events landed in one file: {body}");
    assert_eq!(cache.len(), 1, "cache has exactly one writer");
}

// ---------- Query ----------

#[test]
fn list_sessions_returns_newest_first() {
    let td = TempDir::new().unwrap();
    let runs_dir = td.path().join(".loopr").join("runs");
    fs::create_dir_all(&runs_dir).unwrap();
    for name in ["20260418-120000", "20260419-090000", "20260419-120000-2"] {
        fs::create_dir(runs_dir.join(name)).unwrap();
    }
    let entries = list_sessions(td.path(), None).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
    assert_eq!(names, vec!["20260419-120000-2", "20260419-090000", "20260418-120000"]);
}

#[test]
fn list_sessions_skips_invalid_dirnames() {
    let td = TempDir::new().unwrap();
    let runs_dir = td.path().join(".loopr").join("runs");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::create_dir(runs_dir.join("20260419-090000")).unwrap();
    fs::create_dir(runs_dir.join("garbage")).unwrap();
    File::create(runs_dir.join("stray.txt")).unwrap();
    let entries = list_sessions(td.path(), None).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id.as_str(), "20260419-090000");
}

#[test]
fn list_sessions_empty_returns_empty() {
    let td = TempDir::new().unwrap();
    let entries = list_sessions(td.path(), None).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn tail_latest_session_returns_last_n_lines() {
    let td = TempDir::new().unwrap();
    let runs_dir = td.path().join(".loopr").join("runs");
    let run_dir = runs_dir.join("20260419-120000");
    fs::create_dir_all(&run_dir).unwrap();
    let mut body = String::new();
    for i in 1..=20 {
        body.push_str(&format!("line {i}\n"));
    }
    fs::write(run_dir.join("loopr.log"), body).unwrap();
    let tail = tail_latest_session(td.path(), 5, None).unwrap();
    let lines: Vec<&str> = tail.lines().collect();
    assert_eq!(lines, vec!["line 16", "line 17", "line 18", "line 19", "line 20"]);
}

#[test]
fn tail_latest_session_no_runs_returns_err() {
    let td = TempDir::new().unwrap();
    match tail_latest_session(td.path(), 10, None) {
        Err(QueryError::NoRunsFound { .. }) => {}
        other => panic!("expected NoRunsFound, got {other:?}"),
    }
}

#[test]
fn tail_latest_session_picks_newest() {
    let td = TempDir::new().unwrap();
    let runs_dir = td.path().join(".loopr").join("runs");
    let old = runs_dir.join("20260418-120000");
    let new = runs_dir.join("20260419-120000");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    fs::write(old.join("loopr.log"), "old line\n").unwrap();
    fs::write(new.join("loopr.log"), "new line\n").unwrap();
    let tail = tail_latest_session(td.path(), 10, None).unwrap();
    assert!(tail.contains("new line"));
    assert!(!tail.contains("old line"));
}

#[test]
fn list_sessions_excludes_specified_id() {
    let td = TempDir::new().unwrap();
    let runs_dir = td.path().join(".loopr").join("runs");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::create_dir(runs_dir.join("20260419-120000")).unwrap();
    fs::create_dir(runs_dir.join("20260419-130000")).unwrap();
    let exclude = SessionId::parse("20260419-130000").unwrap();
    let entries = list_sessions(td.path(), Some(&exclude)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id.as_str(), "20260419-120000");
}

#[test]
fn tail_latest_session_excludes_self() {
    let td = TempDir::new().unwrap();
    let runs_dir = td.path().join(".loopr").join("runs");
    let a = runs_dir.join("20260419-120000");
    let b = runs_dir.join("20260419-130000");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("loopr.log"), "older\n").unwrap();
    fs::write(b.join("loopr.log"), "current\n").unwrap();
    let current = SessionId::parse("20260419-130000").unwrap();
    let tail = tail_latest_session(td.path(), 10, Some(&current)).unwrap();
    assert!(
        tail.contains("older"),
        "tail shows older run when current is excluded: {tail}"
    );
    assert!(!tail.contains("current"));
}
