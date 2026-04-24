#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Barrier};
use std::thread;

use tempfile::TempDir;

use super::*;

/// Each test needs a private XDG tree so concurrent test runs don't
/// cross-contaminate session manifests. Point `XDG_DATA_HOME` at a
/// per-test TempDir by setting the env var before the resolver runs.
///
/// SAFETY: `env::set_var` is unsafe as of Rust 1.85 due to multi-thread
/// data-race concerns. Per-test env isolation via `TempDir` + local
/// override is the standard pattern; tests run serially within this
/// integration suite (no parallel test threads touching the same env).
fn with_xdg_home<F: FnOnce(&std::path::Path)>(f: F) {
    let td = TempDir::new().unwrap();
    let prev = std::env::var("XDG_DATA_HOME").ok();
    // SAFETY: tests acquire the env serially via the shared mutex below
    // so no data race on the process's env table.
    unsafe { std::env::set_var("XDG_DATA_HOME", td.path()) };
    f(td.path());
    match prev {
        Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }
}

/// Serialize env-mutating tests. Env is process-global; `#[test]` runs
/// threads in parallel by default. A std::sync::Mutex is enough — the
/// resolver itself does no async.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn resolve_allocates_new_session_when_pointer_absent() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let id = resolve_session_id(target.path(), None).unwrap();
        // Pointer written.
        assert!(pointer_path(target.path()).exists());
        // Pointer contains the allocated id.
        let content = std::fs::read_to_string(pointer_path(target.path())).unwrap();
        assert_eq!(content.trim(), id.as_str());
    });
}

#[test]
fn resolve_reuses_pointer_when_live() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let first = resolve_session_id(target.path(), None).unwrap();
        let second = resolve_session_id(target.path(), None).unwrap();
        assert_eq!(first.as_str(), second.as_str(), "same session id reused");
    });
}

#[test]
fn resolve_allocates_new_when_pointer_points_at_ended() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        // Allocate + mark ended.
        let first = resolve_session_id(target.path(), None).unwrap();
        let dir = telemetry::session_dir(&first).unwrap();
        let body = std::fs::read_to_string(dir.join("manifest.yml")).unwrap();
        let mut manifest: SessionManifest = serde_yaml::from_str(&body).unwrap();
        manifest.ended_at = Some(chrono::Local::now());
        std::fs::write(dir.join("manifest.yml"), serde_yaml::to_string(&manifest).unwrap()).unwrap();
        // Next resolve must allocate a different session.
        let second = resolve_session_id(target.path(), None).unwrap();
        assert_ne!(first.as_str(), second.as_str());
    });
}

#[test]
fn resolve_treats_corrupt_pointer_as_absent() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        std::fs::write(pointer_path(target.path()), "not-a-session-id\n").unwrap();
        let id = resolve_session_id(target.path(), None).unwrap();
        // Fresh id allocated; pointer overwritten.
        let content = std::fs::read_to_string(pointer_path(target.path())).unwrap();
        assert_eq!(content.trim(), id.as_str());
    });
}

#[test]
fn resolve_explicit_flag_attaches_existing_live_session() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let first = resolve_session_id(target.path(), None).unwrap();
        // Remove pointer to verify --session attaches.
        let _ = std::fs::remove_file(pointer_path(target.path()));
        let second = resolve_session_id(target.path(), Some(first.as_str())).unwrap();
        assert_eq!(first.as_str(), second.as_str());
        // Pointer now points at first.
        let content = std::fs::read_to_string(pointer_path(target.path())).unwrap();
        assert_eq!(content.trim(), first.as_str());
    });
}

#[test]
fn resolve_explicit_flag_rejects_ended_session() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let first = resolve_session_id(target.path(), None).unwrap();
        let dir = telemetry::session_dir(&first).unwrap();
        let mut manifest: SessionManifest =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("manifest.yml")).unwrap()).unwrap();
        manifest.ended_at = Some(chrono::Local::now());
        std::fs::write(dir.join("manifest.yml"), serde_yaml::to_string(&manifest).unwrap()).unwrap();
        let err = resolve_session_id(target.path(), Some(first.as_str())).unwrap_err();
        match err {
            LooprError::SessionResolve(msg) => assert!(msg.contains("ended"), "msg: {msg}"),
            other => panic!("expected SessionResolve, got {other:?}"),
        }
    });
}

#[test]
fn stress_50_concurrent_converge_on_single_session() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let target_path = target.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(50));
        let mut handles = Vec::new();
        for _ in 0..50 {
            let tp = target_path.clone();
            let b = barrier.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                resolve_session_id(&tp, None).unwrap()
            }));
        }
        let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for id in &ids {
            *counts.entry(id.as_str().to_string()).or_insert(0) += 1;
        }
        let pointer_content = std::fs::read_to_string(pointer_path(target.path())).unwrap();
        assert_eq!(
            counts.len(),
            1,
            "all resolvers converge on one id; got distribution {counts:?}; pointer content: {pointer_content:?}"
        );
    });
}
