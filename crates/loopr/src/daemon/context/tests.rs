//! Unit tests for `daemon/context.rs` — Phase 2 sidecar-map RAII guard.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use domain::WorkId;

use super::ScopedIdGuard;

#[test]
fn scoped_id_guard_inserts_on_construction() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let key = WorkId::new();
    let _guard = ScopedIdGuard::new(Arc::clone(&map), key.clone());

    let snapshot = map.read().unwrap();
    assert_eq!(snapshot.len(), 1, "guard should insert exactly one entry");
    assert!(snapshot.contains_key(&key), "inserted key must be present");
}

#[test]
fn scoped_id_guard_removes_on_normal_drop() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let key = WorkId::new();
    {
        let _guard = ScopedIdGuard::new(Arc::clone(&map), key.clone());
        assert_eq!(map.read().unwrap().len(), 1, "live during scope");
    }
    assert!(
        map.read().unwrap().is_empty(),
        "guard Drop must remove the entry on normal scope exit"
    );
}

#[test]
fn scoped_id_guard_removes_on_panic_unwind() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let key = WorkId::new();
    let map_for_thread = Arc::clone(&map);
    let key_for_thread = key.clone();

    // Run the panic-prone code on a thread so the test process survives.
    let join = std::thread::spawn(move || {
        let _guard = ScopedIdGuard::new(Arc::clone(&map_for_thread), key_for_thread);
        // The thread panics while the guard is in scope; Rust's unwind
        // must invoke ScopedIdGuard::drop on the way out, removing the
        // entry. This is the panic-unwind correctness guarantee.
        panic!("simulated task panic");
    });
    let result = join.join();
    assert!(result.is_err(), "thread should have panicked");

    assert!(
        map.read().unwrap().is_empty(),
        "guard Drop must remove the entry even when the surrounding scope panics"
    );
}

#[test]
fn scoped_id_guards_keep_independent_keys() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let k1 = WorkId::new();
    let k2 = WorkId::new();
    let k3 = WorkId::new();

    let g1 = ScopedIdGuard::new(Arc::clone(&map), k1.clone());
    let _g2 = ScopedIdGuard::new(Arc::clone(&map), k2.clone());
    let g3 = ScopedIdGuard::new(Arc::clone(&map), k3.clone());

    {
        let snap = map.read().unwrap();
        assert_eq!(snap.len(), 3);
        assert!(snap.contains_key(&k1));
        assert!(snap.contains_key(&k2));
        assert!(snap.contains_key(&k3));
    }

    drop(g1);
    drop(g3);

    let snap = map.read().unwrap();
    assert_eq!(snap.len(), 1, "g2 alone remains live");
    assert!(snap.contains_key(&k2));
    assert!(!snap.contains_key(&k1));
    assert!(!snap.contains_key(&k3));
}
