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

/// Serialize env-mutating tests. Delegates to the crate-shared mutex
/// so this module's tests interleave-proof with `commands/sessions/tests.rs`.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    crate::session::test_env_mutex()
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
fn resolve_explicit_flag_rejects_session_without_manifest() {
    // Phase 8: --session pointing at a never-allocated (manifest-less) id
    // is an error, not a silent attach to a phantom.
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        // Syntactically valid SessionId, but never allocated -> no manifest.
        let err = resolve_session_id(target.path(), Some("20200101-000000")).unwrap_err();
        match err {
            LooprError::SessionResolve(msg) => assert!(msg.contains("no manifest"), "msg: {msg}"),
            other => panic!("expected SessionResolve no-manifest, got {other:?}"),
        }
    });
}

#[test]
fn readonly_returns_ephemeral_without_claiming_pointer() {
    // Phase 8: the sessions-verb resolver must NOT claim the pointer when
    // none exists (otherwise `sessions new` would create two sessions).
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let id = resolve_session_id_readonly(target.path(), None).unwrap();
        assert!(!pointer_path(target.path()).exists(), "readonly must not claim the pointer");
        // The ephemeral session has no manifest, so it is invisible to list_all.
        assert!(!session_manifest_exists(&id).unwrap(), "ephemeral session must have no manifest");
    });
}

#[test]
fn readonly_reuses_live_pointer() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let claimed = resolve_session_id(target.path(), None).unwrap();
        let before = std::fs::read_to_string(pointer_path(target.path())).unwrap();
        let seen = resolve_session_id_readonly(target.path(), None).unwrap();
        let after = std::fs::read_to_string(pointer_path(target.path())).unwrap();
        assert_eq!(seen.as_str(), claimed.as_str(), "readonly reuses the live pointer");
        assert_eq!(before, after, "readonly must not rewrite the pointer");
    });
}

#[test]
fn remove_pointer_if_matches_respects_concurrent_claim() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        std::fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let pointer = pointer_path(target.path());
        std::fs::write(&pointer, "20200101-000000\n").unwrap();
        // A non-matching expected (a concurrent process claimed a new id)
        // must leave the pointer intact.
        remove_pointer_if_matches(&pointer, "20990101-000000").unwrap();
        assert!(pointer.exists(), "non-matching content must not be removed");
        // A matching expected removes it.
        remove_pointer_if_matches(&pointer, "20200101-000000").unwrap();
        assert!(!pointer.exists(), "matching content must be removed");
        // NotFound is a no-op success.
        remove_pointer_if_matches(&pointer, "20200101-000000").unwrap();
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
