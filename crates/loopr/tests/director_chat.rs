//! End-to-end integration test for `loopr director chat`. Phase 8 of
//! `docs/design/2026-05-09-director-phase-2.md` mandated a CLI -> daemon
//! round-trip test under `crates/loopr/tests/director_chat.rs`; the
//! initial ship landed truncation + handler unit tests in
//! `transport/handler/tests.rs` only. This file closes the gap.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::process::Command as StdCommand;
use std::str::FromStr;

use assert_cmd::Command;
use tempfile::TempDir;
use tokio::runtime::Runtime;

use common::{DaemonAutoStop, stop_daemon_for};

fn loopr() -> Command {
    Command::cargo_bin("loopr").unwrap()
}

/// Pre-seed a Plan via the in-process `store::Store` so the test does
/// not depend on the decomposer's real-LLM slow path. The Store is
/// closed before the daemon starts so its SQLite cache is committed
/// to disk; the daemon's own `Store::open` then reads the same data.
fn seed_plan(target: &std::path::Path, goal: &str) -> domain::PlanId {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let store = store::Store::open(target).await.expect("Store::open");
        let plan = domain::Plan::new(goal.to_string());
        let plan_id = plan.id.clone();
        store.plans().create(plan).await.expect("plans().create");
        store.close().await.expect("store.close");
        plan_id
    })
}

/// Minimal git repo seed so the auto-forked daemon can build worktree
/// state without errors during a no-op plan + chat round-trip.
fn init_target(target: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(target)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["config", "commit.gpgsign", "false"]);
    fs::write(target.join("README.md"), "seed\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "seed", "--no-gpg-sign"]);

    // Decompose without an API key can take 10-15s before failing back
    // to the deterministic fallback, exceeding the daemon's default 10s
    // `client-request-secs`. Bump the client cap to 60s here so the
    // round-trip test rides through the real-LLM slow path instead of
    // racing it.
    let loopr_dir = target.join(".loopr");
    fs::create_dir_all(&loopr_dir).unwrap();
    fs::write(
        loopr_dir.join("config.yml"),
        "transport:\n  client-request-secs: 60\n  daemon-startup-secs: 60\n",
    )
    .unwrap();
}

/// Round-trip: pre-seed a Plan via the in-process Store (bypassing
/// the decomposer's real-LLM slow path), then `loopr director chat
/// <plan> "..."` must succeed and persist the message into the
/// target's NotesStore. The CLI prints `note: <note-id>` on stdout;
/// the persisted note's body must match the operator's message.
#[test]
fn director_chat_round_trips_through_daemon_and_persists_note() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_target(target);

    // 1. Seed a Plan in-process via the Store. The daemon's own
    //    Store::open will find it on first IPC dispatch.
    let plan_id = seed_plan(target, "operator-chat-target");

    // 2. The auto-fork happens on the first `loopr` invocation against
    //    this target — there is no decompose path because we never
    //    call `plan create`.
    let _stop = DaemonAutoStop::for_target(target);

    let chat_msg = "investigate the failing build";
    let chat_out = loopr()
        .args([
            "-C",
            target.to_str().unwrap(),
            "director",
            "chat",
            plan_id.as_ref(),
            chat_msg,
        ])
        .assert()
        .success();
    let chat_stdout = String::from_utf8_lossy(&chat_out.get_output().stdout).to_string();
    let note_id = chat_stdout
        .lines()
        .find_map(|l| l.strip_prefix("note:").map(str::trim))
        .expect("note: line must be present in director chat stdout");
    assert!(note_id.starts_with("nt-"), "expected nt-* id, got {note_id}");

    // 3. Stop the daemon so its writes flush to disk, then read the
    //    NotesStore JSONL directly. This avoids re-opening the
    //    SQLite cache (which the daemon owns) and proves the JSONL
    //    side of the truth.
    stop_daemon_for(target);

    let notes_jsonl = target.join(".loopr").join("taskstore").join("operatornotes.jsonl");
    assert!(
        notes_jsonl.is_file(),
        "operatornotes.jsonl must exist after director chat at {}",
        notes_jsonl.display()
    );
    let body = fs::read_to_string(&notes_jsonl).unwrap();
    assert!(
        body.contains(chat_msg),
        "operatornotes.jsonl must contain the operator message; got: {body}"
    );
    assert!(
        body.contains(note_id),
        "operatornotes.jsonl must contain the returned note id; got: {body}"
    );
    assert!(
        body.contains(&plan_id.to_string()),
        "operatornotes.jsonl must record the parent plan_id; got: {body}"
    );

    // 4. Sanity: the note id parses through the typed-id surface.
    let _: domain::NoteId = domain::NoteId::from_str(note_id).expect("note id must parse");
}

/// Negative round-trip: `loopr director chat` against a nonexistent
/// Plan must fail at the daemon's foreign-key check and surface a
/// non-zero exit on the client.
#[test]
fn director_chat_nonexistent_plan_fails_cleanly() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_target(target);

    // Seed any Plan so the Store is initialized; the chat below uses a
    // different (nonexistent) id.
    let _seed_plan_id = seed_plan(target, "warm-up");
    let _stop = DaemonAutoStop::for_target(target);

    loopr()
        .args([
            "-C",
            target.to_str().unwrap(),
            "director",
            "chat",
            "pl-doesnotexist",
            "hello?",
        ])
        .assert()
        .failure();

    stop_daemon_for(target);
}

/// Persist an `OperatorNote` for `plan_id` directly through the
/// in-process Store. Used to seed a note WHILE the daemon is down so
/// the cold-boot path's first Director iteration picks it up. Returns
/// the new `NoteId`.
fn seed_note(target: &std::path::Path, plan_id: &domain::PlanId, message: &str) -> domain::NoteId {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let store = store::Store::open(target).await.expect("Store::open");
        let note = domain::OperatorNote::new(plan_id.clone(), "test-operator".to_string(), message.to_string());
        let note_id = note.id.clone();
        store.notes().create(note).await.expect("notes().create");
        store.close().await.expect("store.close");
        note_id
    })
}

/// Restart-pickup proof. The design (`docs/design/2026-05-12-director-phase-2-followups.md`
/// Phase 1) requires that a note arriving while the daemon is down
/// gets ingested by the post-restart Director's first iteration. The
/// gap this closes is the daemon cold-boot boundary that unit tests
/// cannot exercise: `startup_reconcile_directors` spawns Directors
/// from persisted state, and the freshly spawned Director's first
/// `list_unread_notes_for_plan` call must find the note. A
/// successful LLM round-trip stamps `read_at`, which is the
/// observable signal we assert on.
#[test]
fn note_persists_across_daemon_restart() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_target(target);

    // 1. Seed a Plan that the daemon will pick up via
    //    `startup_reconcile_directors` on both forks.
    let plan_id = seed_plan(target, "restart-pickup-target");

    // 2. Auto-fork daemon #1. The DaemonAutoStop guard is panic-safe
    //    cleanup of last resort; we stop the daemon explicitly below
    //    so the seed-note write happens with no concurrent writer.
    {
        let _stop = DaemonAutoStop::for_target(target);
        loopr()
            .args(["-C", target.to_str().unwrap(), "plans"])
            .assert()
            .success();

        // 3. Bring daemon #1 down deterministically. The Store's
        //    SQLite lock must release before step 4 opens its own
        //    handle. `stop_daemon_for` waits up to 5s for process exit.
        stop_daemon_for(target);
    }

    // 4. Seed an unread `OperatorNote` while the daemon is offline.
    //    JSONL is append-only and SQLite cache is rebuilt on next
    //    daemon boot, so no read-after-write race against the cache.
    let chat_msg = "post-restart pickup probe";
    let note_id = seed_note(target, &plan_id, chat_msg);

    // 5. Auto-fork daemon #2. `startup_reconcile_directors` finds the
    //    Active Plan, spawns a fresh Director, whose first iteration
    //    calls `list_unread_notes_for_plan` and (after a successful
    //    LLM round-trip) `mark_notes_read`.
    let _stop2 = DaemonAutoStop::for_target(target);
    loopr()
        .args(["-C", target.to_str().unwrap(), "plans"])
        .assert()
        .success();

    // 6. Poll the notes JSONL for a line that marks the seeded note
    //    read. `mark_read` appends a fresh full-record line with
    //    `read_at` populated (numeric, not `null`). Generous deadline
    //    covers the Director's restart, context build, and one
    //    Anthropic round-trip. Reading the JSONL directly avoids
    //    opening a competing SQLite handle.
    let notes_jsonl = target.join(".loopr").join("taskstore").join("operatornotes.jsonl");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let note_id_str = note_id.to_string();
    let mut last_body = String::new();
    let read = loop {
        if notes_jsonl.is_file() {
            last_body = fs::read_to_string(&notes_jsonl).unwrap_or_default();
            if last_body.lines().any(|line| {
                line.contains(&note_id_str) && !line.contains("\"read_at\":null") && line.contains("\"read_at\":")
            }) {
                break true;
            }
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    assert!(
        read,
        "post-restart Director never marked seeded note {note_id_str} as read within 90s; jsonl body:\n{last_body}"
    );
}
