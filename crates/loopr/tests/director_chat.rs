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

/// Test-local XDG root so a forked daemon's session/run dirs (where
/// `events.log` lands) are discoverable from the test and never pollute
/// the real `~/.local/share/loopr/`. Mirrors `tests/daemon.rs`.
fn xdg_home_for(target: &std::path::Path) -> std::path::PathBuf {
    target.join(".xdg")
}

/// `loopr` with a pinned XDG root AND `LOOPR_LOG_LEVEL=debug`. The
/// restart-pickup test asserts on a `debug!`-level Director event
/// (`"director: operator note observed"`), which the daemon's default
/// `info` filter drops; the forked daemon inherits this env var and
/// re-parses its own `EnvFilter` from it at telemetry init.
fn loopr_dbg(target: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("loopr").unwrap();
    cmd.env("XDG_DATA_HOME", xdg_home_for(target));
    cmd.env("XDG_CONFIG_HOME", xdg_home_for(target));
    cmd.env("LOOPR_LOG_LEVEL", "debug");
    cmd
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
        // Phase 12 (validation-by-default): `integrator.require-validation`
        // now defaults to `true`, so an empty `validation-commands` list
        // refuses daemon startup. This file tests `director chat`, not
        // Integrator validation; opt out explicitly.
        "transport:\n  client-request-secs: 60\n  daemon-startup-secs: 60\n\
         integrator:\n  require-validation: false\n",
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

/// Seed a Plan plus one `Blocked` child Work directly through the Store.
/// Two reasons the child Work matters for the restart-pickup path:
///   1. `startup_reconcile_directors` RE-DECOMPOSES (rather than spawning a
///      Director) for an Active Plan with ZERO Works. On a credential-less
///      box that decompose hits a real Anthropic 401 and drives the Plan to
///      `Stalled` before a note is ever seeded; daemon #2 then filters the
///      Stalled Plan out and never spawns a Director. A single child Work
///      takes the direct Director-spawn branch instead (`startup.rs:477`).
///   2. `Blocked` is non-terminal and is not auto-dispatched at startup, so
///      the Plan stays `Active` across daemon #1's brief life — no cascading
///      implementer failure can terminalize it.
fn seed_plan_with_work(target: &std::path::Path, goal: &str) -> domain::PlanId {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let store = store::Store::open(target).await.expect("Store::open");
        let plan = domain::Plan::new(goal.to_string());
        let plan_id = plan.id.clone();
        store.plans().create(plan).await.expect("plans().create");
        let mut work = domain::Work::new(plan_id.clone(), "restart-pickup placeholder work".to_string());
        work.status = domain::WorkStatus::Blocked;
        work.blocked_reason = Some("seeded Blocked so the Plan stays Active without dispatch".to_string());
        store.works().create(work).await.expect("works().create");
        store.close().await.expect("store.close");
        plan_id
    })
}

/// Restart-pickup proof (`docs/design/2026-05-12-director-phase-2-followups.md`
/// Phase 1): a note that arrives while the daemon is DOWN must be ingested by
/// the post-restart Director's first iteration. The cold-boot boundary unit
/// tests cannot reach is `startup_reconcile_directors` spawning a fresh
/// Director from persisted state whose first `list_unread_notes_for_plan`
/// call finds the note.
///
/// We assert the credential-independent, load-bearing signal: the fresh
/// Director spawns and OBSERVES the seeded note in its first iteration (the
/// Phase 9 note-observation event, carrying the plan id). We do NOT assert it
/// marks the note read — `mark_notes_read` fires only AFTER a genuine
/// authenticated Anthropic response parses, which cannot happen in `otto ci`:
/// no `ANTHROPIC_API_KEY` exists on the box (and none under that env-var name
/// is ever provisioned), so the placeholder key makes every real call a
/// `Fatal(Auth)` 401. This test drives the compiled binary end-to-end with no
/// fake-LLM seam, so `read_at` is unreachable here; note-observation, which
/// happens before the LLM call, is the invariant actually under test.
#[test]
fn note_persists_across_daemon_restart() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_target(target);
    std::fs::create_dir_all(xdg_home_for(target)).unwrap();

    // 1. Seed an Active Plan WITH one Blocked Work (see `seed_plan_with_work`
    //    for why the child Work is load-bearing on a credential-less box).
    let plan_id = seed_plan_with_work(target, "restart-pickup-target");

    // 2. Fork daemon #1 (debug-level, pinned XDG), then bring it down so the
    //    Store's SQLite lock releases before the offline note seed.
    {
        let _stop = DaemonAutoStop::for_target(target);
        loopr_dbg(target)
            .args(["-C", target.to_str().unwrap(), "daemon", "start"])
            .assert()
            .success();
        stop_daemon_for(target);
    }

    // 3. Seed an unread OperatorNote while the daemon is offline. JSONL is
    //    append-only and the SQLite cache is rebuilt on next boot, so there
    //    is no read-after-write race against the cache.
    let chat_msg = "post-restart pickup probe";
    let _note_id = seed_note(target, &plan_id, chat_msg);

    // 4. Fork daemon #2. `startup_reconcile_directors` finds the still-Active
    //    Plan, spawns a fresh Director, whose first iteration lists (and thus
    //    observes) the seeded note before it ever reaches the LLM call.
    let _stop2 = DaemonAutoStop::for_target(target);
    loopr_dbg(target)
        .args(["-C", target.to_str().unwrap(), "daemon", "start"])
        .assert()
        .success();

    // 5. Poll daemon #2's own events.log (resolved via its `daemon.process-id`
    //    pointer under the pinned XDG session tree) for the fresh Director's
    //    first iteration observing the note. Both branches of the Phase 9
    //    note-observation block — `director.mode_change` (info) and the
    //    idempotent-edge `"director: operator note observed"` (debug) — carry
    //    the plan id and fire ONLY when the unread-note set is non-empty, i.e.
    //    only if the cross-restart note was picked up. That is the invariant.
    let plan_id_str = plan_id.to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut last_events = String::new();
    let observed = loop {
        if let Some(events) = daemon_events_log(target)
            && events.is_file()
        {
            last_events = fs::read_to_string(&events).unwrap_or_default();
            let spawned = last_events.contains("director iteration start");
            let picked_up = last_events.lines().any(|l| {
                l.contains(&plan_id_str)
                    && (l.contains("director: operator note observed") || l.contains("director.mode_change"))
            });
            if spawned && picked_up {
                break true;
            }
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    assert!(
        observed,
        "post-restart Director never spawned + observed the seeded note for {plan_id_str} within 45s; \
         daemon #2 events.log:\n{last_events}"
    );
}

/// Resolve the CURRENT daemon's `events.log` from its on-disk pointers
/// (`active-session` + `daemon.process-id`) under the pinned XDG session
/// tree. `None` until both pointers exist. Mirrors the run-dir resolution in
/// `tests/daemon.rs`.
fn daemon_events_log(target: &std::path::Path) -> Option<std::path::PathBuf> {
    let session_id = fs::read_to_string(target.join(".loopr").join("active-session")).ok()?;
    let process_id = fs::read_to_string(target.join(".loopr").join("daemon.process-id")).ok()?;
    let slug = target.to_str().unwrap().replace('/', "-");
    Some(
        xdg_home_for(target)
            .join("loopr")
            .join("sessions")
            .join(session_id.trim())
            .join("targets")
            .join(&slug)
            .join("runs")
            .join(process_id.trim())
            .join("events.log"),
    )
}
