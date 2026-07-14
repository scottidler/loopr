#![allow(clippy::unwrap_used)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tracing::info_span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer};

use super::*;

// ---------- safe_id_segment (finding 11) ----------

#[test]
fn safe_id_segment_accepts_real_ids() {
    assert!(safe_id_segment("20260609-141500"));
    assert!(safe_id_segment("20260609-141500-2"));
    assert!(safe_id_segment("wk-abc12"));
    assert!(safe_id_segment("pc-9f3a1b"));
}

#[test]
fn safe_id_segment_rejects_path_escapes() {
    assert!(!safe_id_segment(""));
    assert!(!safe_id_segment("../../etc/passwd"));
    assert!(!safe_id_segment("a/b"));
    assert!(!safe_id_segment("a\\b"));
    assert!(!safe_id_segment(".."));
    assert!(!safe_id_segment(".hidden"));
    assert!(!safe_id_segment("a\0b"));
}

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
fn sessionid_parse_multibyte_does_not_panic() {
    // Phase 1 remediation: `query.rs` feeds raw directory names to
    // `parse`; a multibyte name whose byte 15 is mid-codepoint must
    // yield `Malformed`, not panic the byte-slice. '€' is 3 bytes, so
    // a 6-char string is 18 bytes with byte 15 mid-char.
    assert!(SessionId::parse("€€€€€€").is_err());
    assert!(SessionId::parse("2026041€-143012").is_err());
    // A 16+ byte name that is otherwise non-numeric must also be Err.
    assert!(SessionId::parse("日本語日本語日本語").is_err());
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

// ---------- ProcessId ----------

#[test]
fn processid_allocate_produces_valid_slug() {
    let td = TempDir::new().unwrap();
    let id = ProcessId::allocate(td.path()).unwrap();
    let s = id.as_str();
    assert!(s.starts_with("pc-"), "prefix: {s}");
    assert_eq!(s.len(), 9, "pc- + 6 char slug: {s}");
    assert!(td.path().join(s).is_dir());
}

#[test]
fn processid_parse_valid() {
    let id = ProcessId::parse("pc-k3m9f2").unwrap();
    assert_eq!(id.as_str(), "pc-k3m9f2");
}

#[test]
fn processid_parse_rejects_malformed() {
    assert!(ProcessId::parse("").is_err());
    assert!(ProcessId::parse("k3m9f2").is_err(), "missing prefix");
    assert!(ProcessId::parse("pc-k3m9f").is_err(), "short slug");
    assert!(ProcessId::parse("pc-k3m9f22").is_err(), "long slug");
    assert!(ProcessId::parse("pc-K3M9F2").is_err(), "uppercase rejected");
    assert!(ProcessId::parse("pc-k3m9f!").is_err(), "non-alnum rejected");
    assert!(ProcessId::parse("pd-k3m9f2").is_err(), "wrong prefix");
}

#[test]
fn processid_serde_roundtrip() {
    let id = ProcessId::parse("pc-k3m9f2").unwrap();
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, "\"pc-k3m9f2\"");
    let back: ProcessId = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_str(), "pc-k3m9f2");
}

#[test]
fn processid_10k_allocations_no_collisions() {
    let td = TempDir::new().unwrap();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        let id = ProcessId::allocate(td.path()).unwrap();
        assert!(seen.insert(id.as_str().to_string()), "duplicate id: {id}");
    }
    assert_eq!(seen.len(), 10_000);
}

#[test]
fn processid_display_matches_as_str() {
    let id = ProcessId::parse("pc-k3m9f2").unwrap();
    assert_eq!(format!("{id}"), "pc-k3m9f2");
}

// ---------- target_slug ----------

#[test]
fn target_slug_basic() {
    let p = std::path::Path::new("/home/saidler/repos/rust-version");
    assert_eq!(target_slug(p).unwrap(), "-home-saidler-repos-rust-version");
}

#[test]
fn target_slug_root() {
    assert_eq!(target_slug(std::path::Path::new("/")).unwrap(), "-");
}

#[test]
fn target_slug_strips_trailing_slash() {
    let p = std::path::Path::new("/tmp/a/b/");
    assert_eq!(target_slug(p).unwrap(), "-tmp-a-b");
}

#[test]
fn target_slug_rejects_relative() {
    let err = target_slug(std::path::Path::new("relative/path")).unwrap_err();
    assert!(matches!(err, TargetSlugError::NotAbsolute(_)));
}

#[test]
fn target_slug_rejects_empty() {
    let err = target_slug(std::path::Path::new("")).unwrap_err();
    assert_eq!(err, TargetSlugError::Empty);
}

#[test]
fn target_slug_rejects_parent_dir_component() {
    let err = target_slug(std::path::Path::new("/a/../b")).unwrap_err();
    assert!(matches!(err, TargetSlugError::NonCanonical(_)));
}

#[test]
fn target_slug_cur_dir_normalized_away() {
    // `Path::components()` normalizes `.` out, so `/a/./b` slugs identically
    // to `/a/b`. This is stdlib behavior, not something this crate controls.
    let with_dot = target_slug(std::path::Path::new("/a/./b")).unwrap();
    let without_dot = target_slug(std::path::Path::new("/a/b")).unwrap();
    assert_eq!(with_dot, without_dot);
}

#[test]
fn target_slug_deterministic_across_calls() {
    let p = std::path::Path::new("/opt/work/project");
    let a = target_slug(p).unwrap();
    let b = target_slug(p).unwrap();
    assert_eq!(a, b);
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

    crate::testing::ensure_global_interested_default();
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
    crate::testing::ensure_global_interested_default();
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
    crate::testing::ensure_global_interested_default();
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
    crate::testing::ensure_global_interested_default();
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
    crate::testing::ensure_global_interested_default();
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
//
// `list_sessions` and `tail_latest_session` depend on `$XDG_DATA_HOME`
// via `xdg::xdg_root()`. End-to-end coverage lives in the loopr smoke
// test suite (crates/loopr/tests/smoke.rs) where tests spawn the binary
// with a per-test `XDG_DATA_HOME` override. Unit-level tests here would
// either (a) mutate process-global env (race-prone in parallel test
// runs) or (b) introduce an injection parameter that has no production
// use. Prefer the integration coverage.

// ---------- SessionFanoutLayer ----------
//
// The layer composes a `SessionFanoutLayer` with a local subscriber via
// `with_default`, mirroring the WorkFanoutLayer test strategy so these
// tests never call `telemetry::init` (global subscriber state).

fn compose_session_fanout(
    xdg_root: &Path,
    target_slug: &str,
) -> (
    impl tracing::Subscriber,
    std::sync::Arc<std::sync::Mutex<lru::LruCache<String, SharedWriter>>>,
) {
    compose_session_fanout_with_cap(xdg_root, target_slug, 16)
}

fn compose_session_fanout_with_cap(
    xdg_root: &Path,
    target_slug: &str,
    cap: usize,
) -> (
    impl tracing::Subscriber,
    std::sync::Arc<std::sync::Mutex<lru::LruCache<String, SharedWriter>>>,
) {
    let layer = SessionFanoutLayer::with_capacity(xdg_root.to_path_buf(), target_slug.to_string(), cap);
    let cache = layer.cache_handle();
    let subscriber = tracing_subscriber::registry().with(layer.with_filter(EnvFilter::new("info")));
    (subscriber, cache)
}

fn flush_session_cache(cache: &std::sync::Arc<std::sync::Mutex<lru::LruCache<String, SharedWriter>>>) {
    let cache = cache.lock().unwrap();
    for (_id, writer) in cache.iter() {
        let _ = writer.0.lock().unwrap().flush();
    }
}

fn session_fanout_path(xdg_root: &Path, target_slug: &str, session_id: &str) -> std::path::PathBuf {
    xdg_root
        .join("sessions")
        .join(session_id)
        .join("targets")
        .join(target_slug)
        .join("session-fanout.log")
}

#[test]
fn session_fanout_writes_per_session_file_on_session_id() {
    let td = TempDir::new().unwrap();
    let (sub, cache) = compose_session_fanout(td.path(), "-home-test-repo");
    crate::testing::ensure_global_interested_default();
    tracing::subscriber::with_default(sub, || {
        let span = info_span!("loopr.invocation", session_id = "20260424-150000");
        let _enter = span.enter();
        tracing::info!("hello session");
    });
    flush_session_cache(&cache);
    let log = session_fanout_path(td.path(), "-home-test-repo", "20260424-150000");
    assert!(log.is_file(), "per-session log at {}", log.display());
    let body = fs::read_to_string(&log).unwrap();
    assert!(body.contains("hello session"), "body: {body}");
}

#[test]
fn session_fanout_writes_per_session_file_on_client_session_id_recorded_post_creation() {
    // Mirrors the daemon's `ipc.connection` span: created without a
    // session, `client_session_id` recorded after handshake. Requires
    // the layer's `on_record` hook.
    let td = TempDir::new().unwrap();
    let (sub, cache) = compose_session_fanout(td.path(), "-home-daemon-repo");
    crate::testing::ensure_global_interested_default();
    tracing::subscriber::with_default(sub, || {
        let span = info_span!(
            "ipc.connection",
            conn_id = "00000000-0000-0000-0000-000000000000",
            client_session_id = tracing::field::Empty,
        );
        let _enter = span.enter();
        // No session_id carried yet -> the event's routing falls through.
        tracing::info!("pre-handshake");
        span.record("client_session_id", "20260424-160000");
        tracing::info!("post-handshake");
    });
    flush_session_cache(&cache);
    let log = session_fanout_path(td.path(), "-home-daemon-repo", "20260424-160000");
    assert!(log.is_file(), "per-session log at {}", log.display());
    let body = fs::read_to_string(&log).unwrap();
    assert!(body.contains("post-handshake"), "post-handshake event recorded: {body}");
    assert!(
        !body.contains("pre-handshake"),
        "pre-handshake event should not be attributed (no session yet): {body}"
    );
}

#[test]
fn session_fanout_no_session_id_creates_no_file() {
    let td = TempDir::new().unwrap();
    let (sub, _cache) = compose_session_fanout(td.path(), "-home-test-repo");
    crate::testing::ensure_global_interested_default();
    tracing::subscriber::with_default(sub, || {
        tracing::info!("no session id");
    });
    assert!(
        !td.path().join("sessions").exists(),
        "no sessions dir without session_id: {}",
        td.path().display()
    );
}

#[test]
fn session_fanout_two_session_ids_get_two_files() {
    let td = TempDir::new().unwrap();
    let (sub, cache) = compose_session_fanout(td.path(), "-home-test-repo");
    crate::testing::ensure_global_interested_default();
    tracing::subscriber::with_default(sub, || {
        let s1 = info_span!("loopr.invocation", session_id = "20260424-150000");
        s1.in_scope(|| tracing::info!("for A"));
        let s2 = info_span!("loopr.invocation", session_id = "20260424-170000");
        s2.in_scope(|| tracing::info!("for B"));
    });
    flush_session_cache(&cache);
    let a = session_fanout_path(td.path(), "-home-test-repo", "20260424-150000");
    let b = session_fanout_path(td.path(), "-home-test-repo", "20260424-170000");
    assert!(a.is_file(), "A: {}", a.display());
    assert!(b.is_file(), "B: {}", b.display());
    assert!(fs::read_to_string(&a).unwrap().contains("for A"));
    assert!(fs::read_to_string(&b).unwrap().contains("for B"));
}

#[test]
fn session_fanout_same_session_id_reuses_writer() {
    let td = TempDir::new().unwrap();
    let (sub, cache) = compose_session_fanout(td.path(), "-home-test-repo");
    crate::testing::ensure_global_interested_default();
    tracing::subscriber::with_default(sub, || {
        let span = info_span!("loopr.invocation", session_id = "20260424-150000");
        span.in_scope(|| {
            tracing::info!("first");
            tracing::info!("second");
        });
    });
    flush_session_cache(&cache);
    let cache_size = cache.lock().unwrap().len();
    assert_eq!(cache_size, 1, "cache has exactly one writer");
    let log = session_fanout_path(td.path(), "-home-test-repo", "20260424-150000");
    let body = fs::read_to_string(&log).unwrap();
    let count = body.matches("first").count() + body.matches("second").count();
    assert_eq!(count, 2, "both events in one file: {body}");
}

#[test]
fn session_fanout_lru_evicts_oldest_when_cap_exceeded() {
    let td = TempDir::new().unwrap();
    // Cap of 2 so the third session evicts the first.
    let (sub, cache) = compose_session_fanout_with_cap(td.path(), "-home-test-repo", 2);
    crate::testing::ensure_global_interested_default();
    tracing::subscriber::with_default(sub, || {
        for i in 1..=3 {
            let sid = format!("20260424-15000{i}");
            let span = info_span!("loopr.invocation", session_id = sid.as_str());
            span.in_scope(|| tracing::info!("event for {}", sid));
        }
    });
    flush_session_cache(&cache);
    // Cache capped at 2; oldest (first session) evicted but file still exists.
    let cache_size = cache.lock().unwrap().len();
    assert_eq!(cache_size, 2, "cache capped at 2: size={cache_size}");
    for i in 1..=3 {
        let sid = format!("20260424-15000{i}");
        let log = session_fanout_path(td.path(), "-home-test-repo", &sid);
        assert!(log.is_file(), "log for {sid} at {}", log.display());
    }
}

#[test]
fn session_fanout_evicted_session_reopens_on_next_event() {
    let td = TempDir::new().unwrap();
    let (sub, cache) = compose_session_fanout_with_cap(td.path(), "-home-test-repo", 1);
    crate::testing::ensure_global_interested_default();
    tracing::subscriber::with_default(sub, || {
        let s1 = info_span!("loopr.invocation", session_id = "20260424-150001");
        s1.in_scope(|| tracing::info!("first for 1"));
        let s2 = info_span!("loopr.invocation", session_id = "20260424-150002");
        s2.in_scope(|| tracing::info!("first for 2"));
        // session 1 evicted. Emit again under it; must re-open and append.
        let s1_again = info_span!("loopr.invocation", session_id = "20260424-150001");
        s1_again.in_scope(|| tracing::info!("second for 1"));
    });
    flush_session_cache(&cache);
    let log1 = session_fanout_path(td.path(), "-home-test-repo", "20260424-150001");
    let body1 = fs::read_to_string(&log1).unwrap();
    assert!(body1.contains("first for 1"), "first-1 survives: {body1}");
    assert!(body1.contains("second for 1"), "second-1 appended: {body1}");
}
