# Design Document: Loopr Layout - Sessions + XDG-Externalized Telemetry

**Author:** Scott A. Idler
**Date:** 2026-04-24
**Status:** Implemented
**Review Passes Completed:** 5/5
**Crates touched:** telemetry, loopr, ipc

## Summary

Commits a layout for loopr's on-disk state. Two structural changes: (1) promote the existing `run-id` to `session-id` as a first-class user-facing concept analogous to Claude's session handle, keeping its timestamp format (`YYYYMMDD-HHMMSS[-N]`) because that format is already session-shaped (meaningful at a glance, one per daemon boot) and was only misnamed. Introduce a new `process-id` type (`pc-<6char>` short slug) for genuinely per-process handles. (2) Externalize process-level telemetry from `<target>/.loopr/` to the user's XDG data directory, keyed by session-id and target-slug, so target repos stay uncluttered and sessions can (later) aggregate activity across multiple targets. Target-local `.loopr/` retains exactly the things a human would open, edit, or commit: config, prompts, the typed record truth, in-flight worktrees, runtime handles, and a small derived-records tree for local debugging. Everything else moves to XDG.

## Problem Statement

### Background

The current layout has `<target>/.loopr/runs/<run-id>/` where `run-id` is allocated by every loopr process (one per daemon boot, one per client CLI invocation). `RunId::allocate` at `telemetry/src/runid.rs:28` races on `create_dir` inside `.loopr/runs/`; the winning process claims that timestamp dir as its subscriber output location. The name `run-id` suggests a user-facing concept, but the actual semantics are purely process-level plumbing.

In parallel, the project is building toward a TUI-forward UX (per `docs/vision.md` and `docs/roadmap.md` "Beyond First Gate"). When the TUI lands, the user will need a stable, referenceable handle for "a loopr work session" - something they can observe, detach from, and resume via an explicit id, analogous to `claude --resume <session-id>`. No such concept exists in the codebase today.

Separately, an instrumentation sweep (`docs/design/2026-04-24-instrumentation-sweep.md`) proposed adding derived record artifacts (summaries, Ralph-loop transcripts) under a nested layout. During review, the question surfaced: should these record artifacts go under each run, or accumulate across runs at the record level? And then: should they nest under a session, and where does a session live? That review revealed the layout had not been committed as a schema, was being re-derived ad-hoc per doc, and needed a single authoritative definition before further artifact-producing features land.

### Problem

1. **`run-id` is misnamed.** It's a per-process telemetry scope, not a user-facing run. Operators asking "what's the id of this loopr run?" get a different answer than the code's `run-id`. When the TUI lands and users expect a Claude-like session handle, this name will actively mislead.
2. **No session concept.** The TUI needs an anchor. Record artifacts need a group to belong to. Cross-target work (future) needs a correlation id. None exists.
3. **`.loopr/runs/` clutter.** Every client CLI invocation (`loopr works`, `loopr show X`, `loopr logs tail`, etc.) allocates its own run-dir. After an hour of operational use, the directory contains one daemon run plus dozens of throwaway client runs, most 4 events long. Signal-to-noise is poor.
4. **No canonical schema.** Four separate docs touch the layout (`2026-04-19-telemetry-stage-2.md`, `2026-04-23-cli-plumbing-shape.md`, the Stage 5 carry-over memo for `init`, the recent instrumentation-sweep doc). Each describes a slice; none claims the whole. The result is drift - the agent writing the next feature has to walk four docs to know where its artifacts go.
5. **Ambiguous scope for future artifacts.** Should a Work's transcript live under `runs/<run-id>/` (run-scoped) or somewhere stable (record-scoped or session-scoped)? Without a committed schema, each new doc re-litigates this.

### Goals

- Commit a single authoritative directory schema for everything loopr writes to disk, across both the target repo's `.loopr/` and the user's XDG data dir.
- Introduce `session-id` as a typed, user-facing, resumable handle. Allocation happens on user-initiated entry points (bare `loopr`, `loopr plan`, TUI launch); resumption via `--resume <id>`; termination explicit or implicit.
- Rename the existing `run-id` to `session-id` throughout the codebase and user-facing surfaces; introduce a new `process-id` type (`pc-<6char>`) for genuinely per-process handles. Update `loopr logs runs` to `loopr logs sessions` (with process-level filtering as a sub-query). Clear up the overload so "session" and "process" are orthogonal concepts.
- Move process-level telemetry out of `<target>/.loopr/` and into XDG at `~/.local/share/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/`. Target dir stays small; telemetry is pruneable globally; sessions can span targets in principle (lift in TUI era).
- Keep exactly four things in `<target>/.loopr/` that the user should see: `config.yml` + `prompts/` (editable), `taskstore/` (committed truth), `worktrees/` + daemon-runtime handles (operational state), and `records/` (per-target derived views of what happened to records in THIS repo).
- Make every event in the telemetry stream self-identify by carrying `session_id`, `process_id`, and `target_slug` as span fields, so a JSON log read out of file-path context is still attributable.

### Non-Goals

- **Building the TUI.** The TUI is deferred to post-first-gate per `docs/roadmap.md`. This doc commits the session concept and the layout that will support the TUI when it ships; the TUI itself is not built here.
- **Cross-target session workflows in first gate.** Architecture supports sessions crossing targets (they live in XDG above the target-slug); the UX flows for multi-target work (which TUI will drive) are not built or designed here. First-gate sessions are single-target in practice.
- **Session retention/pruning automation.** CLI surface (`loopr sessions prune`) and the retention policy knob in config ship later, earned when XDG-dir size becomes a felt problem.
- **User-level config overrides across targets.** The XDG dir here is for STATE (telemetry, session manifests), not for user-global CONFIG or PROMPTS. Per-user config is an earned feature, tracked separately in the vision's prompt-override chain.
- **LLM response cache.** Mentioned in `docs/vision.md` "Beyond First Gate" as `~/.local/share/loopr/llm-cache/`; not in this doc's scope.
- **Global runs-index.** Also vision deferred enhancement; not here.
- **Supporting multiple users on the same target.** Single-user design; XDG is per-user by definition. Shared-NFS target scenarios are out of scope.

## Proposed Solution

### Overview

Split loopr's on-disk state into two trees:

- **Target-local** (`<target>/.loopr/`): human-facing config, typed truth, runtime handles, in-flight worktree state, and a local derived-records view.
- **User-global** (`~/.local/share/loopr/`): session manifests, raw process-level telemetry keyed by session and target, session-level digests.

A thin pointer at `<target>/.loopr/active-session` ties the target to its current session. Every process resolves session-id at startup (from the pointer, the `--session <id>` flag, or by allocating a fresh one) before the telemetry subscriber initializes, so the subscriber's output path is always `~/.local/share/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/`.

### Architecture

#### Target-Local Tree

```
<target>/.loopr/
  # [process-singleton] daemon runtime handles
  daemon.pid                        # PID of the currently-live daemon (or stale-and-cleaned-up)
  daemon.process-id                 # renamed from daemon.run-id; the daemon's own telemetry process-id
  daemon.version                    # daemon binary version (reconcile gate)
  socket                            # UNIX domain socket for IPC

  # [session pointer]
  active-session                    # file: current session-id for this target (or absent)

  # [user-editable, init-seeded]
  config.yml                        # written by `loopr init` from resources/config/default.yml
  prompts/                          # written by `loopr init` via include_dir!()
    decomposer/...
    implementer/...
    reviewer/...
    partials/...

  # [git-committed typed truth]
  taskstore/
    plans.jsonl
    works.jsonl
    bundles.jsonl
    ticks.jsonl

  # [daemon-life worktree state]
  worktrees/
    <work-id>-<attempt>/            # sibling git worktrees
  worktree-registry.jsonl           # flat at .loopr/ root (not nested in worktrees/; describes all)

  # [per-target derived records - LOCAL view for "what happened to MY repo"]
  records/
    plans/<plan-id>/
      summary.md                    # short digest, rewritten on FSM transitions
      decomposition.md              # Decomposer transcript
    works/<work-id>/
      summary.md                    # aggregates across attempts
      attempts/<n>/
        transcript.md               # Implementer Ralph loop iterations for attempt <n>
    bundles/<bundle-id>/
      summary.md
      review.md                     # Reviewer transcript
```

#### User-Global Tree (XDG)

```
~/.local/share/loopr/
  sessions/<session-id>/
    manifest.yml                    # session metadata + cross-reference index
    summary.md                      # session-level digest (may cross targets)
    targets/<target-slug>/
      runs/<process-id>/
        events.log                  # structured JSON stream (source of truth)
        loopr.log                   # pretty human-readable stream
        work/<work-id>.log          # fanout from events.log by work_id
        summary.md                  # process-level digest at exit
      summary.md                    # per-target-per-session digest
```

#### ID Taxonomy

| ID | Format | Lifetime | Scope |
|---|---|---|---|
| `session-id` | `YYYYMMDD-HHMMSS[-N]` (local time; `-N` on collision) | user-initiated; resumable; explicit end | user (crosses targets in principle) |
| `process-id` | `pc-<6char>` (random lowercase alnum slug; new type) | per loopr OS process | process (one daemon boot; one CLI invocation) |
| `target-slug` | path slugification: `/home/x/repos/y` -> `-home-x-repos-y` | per-target path | user (one slug per target path) |
| `plan-id`, `work-id`, `bundle-id`, `tick-id` | `pl-<slug>`, `wk-<slug>`, etc. | record lifetime | target |
| `attempt` | `u32` starting at 1 | per worktree branch for a Work | record |

Rationale: session timestamps are meaningful at a glance - `ls ~/.local/share/loopr/sessions/` shows when each session started. Process ids are ephemeral and numerous; a short opaque slug is the right shape. The existing `RunId` format (timestamp) is semantically closer to a session (one per daemon boot = one long-lived thing) than to a process (one per CLI invocation = many short-lived things); promoting it to `SessionId` is the honest rename.

`target-slug` is claude-style path-slugification: leading slash becomes leading dash, subsequent slashes become dashes. Collisions are possible in principle (`/home/a` and `/home-a` both slug to `-home-a`) but rare with realistic paths; document as a known edge case, not a forcing constraint.

### Data Model

#### Session manifest (`sessions/<session-id>/manifest.yml`)

```yaml
session_id: 20260424-150000
started_at: 2026-04-24T15:00:00-07:00
ended_at: null                      # set when explicitly ended
origin: cli                         # cli | tui | daemon-boot
targets:
  - path: /home/saidler/repos/scottidler/rust-version
    slug: -home-saidler-repos-scottidler-rust-version
    first_attached: 2026-04-24T15:00:00-07:00
processes:
  - process_id: pc-k3m9f2
    target_slug: -home-saidler-repos-scottidler-rust-version
    kind: daemon                    # daemon | client
    subcommand: null                # for client: plan | works | show | ...
    started_at: 2026-04-24T15:00:00-07:00
    ended_at: 2026-04-24T15:05:23-07:00
records:                            # reverse index into target-local records/
  - kind: plan
    id: pl-p9g75
    target_slug: -home-saidler-repos-scottidler-rust-version
  - kind: work
    id: wk-guiap
    target_slug: -home-saidler-repos-scottidler-rust-version
  # etc.
```

Manifest is written on session creation, updated on every process attach and every significant FSM transition in a record the session touched. Atomic writes via write-to-temp + rename. Idempotent (rewriteable from scratch by walking XDG + target).

#### Active-session pointer (`<target>/.loopr/active-session`)

```
20260424-150000
```

Single-line file. Atomic update via write-to-temp + rename. Absence means no active session; next process allocates one.

### API Design

#### Session lifecycle (new CLI verbs)

```
loopr sessions list [--all]           # list sessions for this target (or all if --all)
loopr sessions new                     # explicitly create a new session; updates active-session
loopr sessions resume <id>             # attach active-session to <id>; error if <id> ended
loopr sessions end                     # end the active session; clear pointer
loopr sessions status                  # show active session + process count + recent records
loopr sessions prune [--older-than=30d] # remove ended sessions older than cutoff (earned; not first-gate)
```

Global flag on every other command:

```
loopr --session <id> <subcommand>      # force attach to <id>; bypass active-session pointer
```

#### Session resolution at process start

Every loopr process, at startup, resolves session-id before initializing the telemetry subscriber:

```
resolve_session_id(target, --session flag) -> SessionId:
    if --session <id> provided:
        require <id> exists and not ended; attach
        return <id>
    if <target>/.loopr/active-session exists and points at a valid session:
        return that id
    allocate new session (sn-<6char>); write pointer; return id
```

The function is deterministic given target + flag + disk state; races on pointer updates are avoided by O_CREAT+O_EXCL on the session's manifest file (similar to RunId's create_dir race).

#### IPC handshake extension

The `system.handshake` message gains one field:

```
{
  "method": "system.handshake",
  "params": {
    "protocol_version": 1,
    "client_process_id": "pc-q7x2nh",
    "session_id": "20260424-150000"           # NEW
  }
}
```

Server-side: daemon records `session_id` on the connection's root span. Every request span under that connection inherits it. Daemon-side events emitted in response to that request carry `session_id` and route telemetry to the caller's session dir (via a `SessionFanoutLayer`, parallel to the existing `WorkFanoutLayer`).

Non-breaking additive: older clients that omit the field default to the daemon's own session-id.

#### Subscriber path construction

Subscriber init moves from:

```
let run_dir = target.join(".loopr").join("runs").join(run_id.as_str());
```

to:

```
let xdg = dirs::data_local_dir().expect("XDG data dir").join("loopr");
let run_dir = xdg
    .join("sessions").join(session_id.as_str())
    .join("targets").join(&target_slug)
    .join("runs").join(process_id.as_str());
```

`dirs::data_local_dir()` resolves to `$XDG_DATA_HOME` or `$HOME/.local/share` per the XDG Base Directory Spec.

### Implementation Plan

One phase per committable unit, ordered to avoid intermediate broken states. Every phase ends with `otto ci` passing at workspace root.

#### Phase 1: Rename `RunId` to `SessionId`
**Model:** sonnet
The existing `RunId` type in `crates/telemetry/src/runid.rs` already uses the `YYYYMMDD-HHMMSS[-N]` format that this doc assigns to sessions, and its allocation semantics (atomic via `create_dir` EEXIST-race, one per daemon boot in the daemon's case) are already session-shaped. Rename it: `runid.rs` -> `session.rs`; `RunId` -> `SessionId`; `run-id` in strings/paths/CLI help -> `session-id`. Update every call site in `crates/telemetry/`, `crates/loopr/`, `crates/ipc/` (if any).

Allocator target directory changes in Phase 5 (XDG). In Phase 1, keep the target-local `.loopr/runs/` allocation directory - the rename is semantic, not a path change.

Acceptance: `cargo test -p telemetry -p loopr` passes; `loopr --help` references `session-id`; no string `run-id` survives outside historical doc references.

#### Phase 2: Introduce `ProcessId` type
**Model:** sonnet
New type at `crates/telemetry/src/process.rs`. Format: `pc-<6-char-lowercase-alnum>`. Allocation mints a random slug and verifies uniqueness via `create_dir` (same EEXIST-race pattern as `SessionId`, scoped to the session's `runs/` subdir). Parsing accepts stored ids; serde transparent. Test: allocate 10_000 ids, verify no collisions.

Acceptance: `cargo test -p telemetry` passes; `ProcessId::allocate` and `ProcessId::parse` are the stable API.

#### Phase 3: Target-slug utility
**Model:** sonnet
Add `crates/telemetry/src/slug.rs` with `pub fn target_slug(path: &Path) -> String`. Claude-style slugification: canonicalize path, leading `/` becomes leading `-`, subsequent `/` become `-`. Handle trailing slash (strip). Handle `..` (reject - slugs must be for canonicalized paths). Test: round-trip a handful of realistic paths; verify deterministic output.

Acceptance: `cargo test -p telemetry` passes on slug tests; deterministic across platforms.

#### Phase 4: XDG path resolver + active-session pointer
**Model:** opus (concurrency + edge cases)
Add `dirs` to workspace deps via `cargo add -p telemetry`. Add `crates/telemetry/src/xdg.rs` with `pub fn session_run_dir(session: &SessionId, target_slug: &str, process: &ProcessId) -> PathBuf`. Resolves to `$XDG_DATA_HOME/loopr/sessions/<id>/targets/<slug>/runs/<process-id>/`; creates intermediate dirs as needed.

In `crates/loopr/src/session/` (new module) add `resolve_session_id(target, flag) -> Result<SessionId>`. Read `<target>/.loopr/active-session`; fall back to allocation; atomic pointer update (write-to-temp + rename + fsync).

Acceptance: unit tests for resolver covering: explicit flag, valid pointer, absent pointer, pointer pointing at ended session, pointer corruption. `active-session` pointer updates are atomic under concurrent client invocations (stress test: 50 simultaneous `loopr works` calls, all converge on one session-id).

#### Phase 5: Rewire subscriber path
**Model:** sonnet
Update `crates/telemetry/src/subscriber.rs:init()` to take `(target, session_id, target_slug, process_id, directive)` instead of `(target, run_id, directive)`. Compose the XDG path via Phase 4's resolver. Create the run dir at XDG, not at target. `.loopr/runs/` is no longer written to by the subscriber.

Update `crates/loopr/src/lib.rs` (client init) and `crates/loopr/src/daemon.rs` (daemon init) to call the new signature. Both sites resolve session-id via Phase 4's resolver before calling `telemetry::init`.

Acceptance: a fresh `loopr daemon start` followed by `loopr plan "..."` writes to XDG paths, leaves `<target>/.loopr/runs/` untouched (or absent). `loopr logs tail` reads from XDG correctly. Integration test: full pipeline produces expected paths under XDG.

#### Phase 6: IPC handshake carries `session_id`
**Model:** opus (protocol evolution semantics)
Update `crates/ipc/src/handshake.rs` (or wherever `system.handshake` lives): add `session_id: Option<String>` param. Non-breaking additive: daemon accepts absence, defaults to daemon's own session-id. Bump `PROTOCOL_VERSION` minor if the protocol uses semver-style versioning; else document the additive change.

Update `crates/loopr/src/transport/client.rs` to send `session_id` resolved by Phase 4. Update daemon's handshake handler to record it on the connection span. Every request handled under that connection inherits `session_id` as a span field.

Acceptance: round-trip tests exercise both present and absent `session_id`; daemon-side spans in `events.log` show the field inherited correctly; server-side events for a request route telemetry to the caller's session dir.

#### Phase 7: SessionFanoutLayer
**Model:** opus (subscriber layer with concurrent writes)
Add `crates/telemetry/src/session_fanout.rs` modeled on existing `WorkFanoutLayer`. When an event carries `session_id`, append to that session's pretty log at `sessions/<session-id>/targets/<target-slug>/session-fanout.log` (or similar path; decide in implementation). Activates as soon as handshake-derived session-id appears in spans; dormant until then.

Cache of open file writers to avoid file-handle exhaustion; LRU eviction when cache hits a cap.

Acceptance: a daemon handling requests for two different sessions correctly fans out events to two different files; cache behavior under load (e.g., rapid session cycling) doesn't leak handles.

#### Phase 8: `loopr sessions` CLI surface
**Model:** sonnet
Implement `loopr sessions {list, new, resume, end, status}` in `crates/loopr/src/commands/sessions.rs`. List reads the XDG sessions dir; new allocates via Phase 2 + updates pointer; resume validates target existence and updates pointer; end updates manifest.ended_at and clears pointer; status reads active pointer and recent record counts.

`--session <id>` global flag added to clap `Cli` struct; passed to session resolver.

Acceptance: each verb behaves per spec. E2E test: `loopr sessions new; loopr plan "..."; loopr sessions status; loopr sessions end; loopr sessions list` walks the full lifecycle.

#### Phase 9: Migration policy
**Model:** sonnet
Per the v5 Working Rule "no coexistence migrations": on first boot after this ships, daemon detects any existing `<target>/.loopr/runs/` dir, emits a one-line `warn!("legacy runs dir present; no migration performed; rkvr rmrf to clean")` log, and proceeds. Old logs stay; daemon writes nowhere near them. No auto-delete.

`loopr init` is unchanged by this doc; Phase 5 of the Stage 6 scope memo (Stage 5 carry-over) still governs init.

Acceptance: fresh target works; target with legacy `.loopr/runs/` works; warn fires once per daemon boot.

#### Phase 10: Documentation sweep
**Model:** sonnet
- Update `crates/telemetry/CLAUDE.md` to describe the XDG-backed paths (was "inside `.loopr/`", becomes "inside XDG keyed by session").
- Update `crates/loopr/CLAUDE.md` to name session resolution as part of lifecycle.
- Update `docs/vision.md` "Target-repo layout" section to the new schema.
- Update `docs/roadmap.md` Stage 9 exit criterion to require session-id in the E2E assertion (a session exists; records are correctly indexed).
- Add a schema pointer at `docs/vision.md` top-matter pointing at this doc.

Acceptance: grep for "runs/" in `docs/` and `crates/*/CLAUDE.md` surfaces no stale references. The instrumentation-sweep doc's record-tree paths (`.loopr/records/...`) align with this doc.

## Alternatives Considered

### Alternative 1: Everything nested under `<target>/.loopr/sessions/<id>/` (option A from discussion)

- **Description:** Keep all session + telemetry + records under the target. No XDG.
- **Pros:** Single location; `ls .loopr/` shows everything for this target.
- **Cons:** Sessions are target-bound (no cross-target path forever without migration); target dir grows unboundedly; session concept is effectively a per-target facade with no clean upgrade path to user-global semantics.
- **Why not chosen:** User explicitly picked XDG externalization; TUI-era requirements (cross-target sessions) force this architecture eventually, and "do it now while simple" is cheaper than retrofitting.

### Alternative 2: Symlink hybrid (option C from discussion)

- **Description:** `<target>/.loopr/sessions/<id>/runs/` is a symlink into XDG; everything else stays target-local.
- **Pros:** `ls .loopr/sessions/<id>/` still works for local debugging; raw telemetry prunes globally.
- **Cons:** Symlinks on Windows (WSL edge cases) are fragile; backup/restore tools handle them inconsistently; the abstraction leaks in surprising ways when operators poke around with standard shell tools.
- **Why not chosen:** Clever, but adds failure modes without delivering capability beyond the non-symlinked split.

### Alternative 3: Also move records/ to XDG (option B2 from discussion)

- **Description:** `<target>/.loopr/` has only config + prompts + taskstore + worktrees + daemon handles. Records (summaries + transcripts) live entirely in XDG under the session.
- **Pros:** Strict separation; target dir is minimal; everything derived is session-scoped.
- **Cons:** Asking "what happened to MY repo's Work wk-guiap?" forces the reader to find which session(s) touched the target first, then walk XDG. Local debugging loses the `cat .loopr/records/works/<id>/summary.md` affordance.
- **Why not chosen:** The local debugging affordance for the single-target case (which is 99% of first-gate use) is too valuable to trade for strict session ownership.

### Alternative 4: UUIDs for session-id (claude-exact)

- **Description:** `session-id` is a UUIDv4 like `f7d06a91-86b6-49e5-a479-4fbaf3f0af93`.
- **Pros:** Matches Claude's format exactly; zero collision risk; grep-safe.
- **Cons:** Long and opaque; loses the "when did this session start" information that a timestamp carries for free. Operators typing `loopr --session f7d06a91-...` is painful. Claude accepts UUIDs because it has no equivalent of the RunId timestamp format already in the codebase; loopr does, and it carries useful signal.
- **Why not chosen:** Timestamp is already meaningful + unambiguous within loopr's id space. Keep it.

### Alternative 5: Keep `run-id` name; rename only conceptually in docs

- **Description:** Leave code unchanged; document that `run-id` is process-scoped.
- **Pros:** Zero code churn.
- **Cons:** Future readers still trip over the name. Every new feature that lands reintroduces the confusion. The rename IS the forcing function to clear up the mental model.
- **Why not chosen:** Docs-only renames are compromise solutions. If the concept is process-scoped, the identifier should say so.

## Technical Considerations

### Dependencies

- Add `dirs` to workspace deps (standard XDG lookup). Use `cargo add -p telemetry dirs` when Phase 4 starts.
- No new subscriber-layer deps; `SessionFanoutLayer` mirrors the existing `WorkFanoutLayer` structure.
- `ipc` gains one optional field in `system.handshake`; no protocol version bump required (additive).

### Performance

- Subscriber init gains one XDG lookup (`dirs::data_local_dir()`) and one active-session-pointer read. Both are sub-ms file-system ops; daemon startup latency is unchanged within measurement noise.
- `SessionFanoutLayer` adds one dashmap lookup + one file-append per event carrying `session_id`. Event rate at DEBUG is ~400/run (from the instrumentation-sweep doc); overhead is imperceptible.
- XDG dir may accumulate many sessions over time; retention policy (earned feature) caps this.

### Security

- XDG dir must be 0700 (user-only read/write). Set permissions on session dir creation.
- `active-session` pointer is a plain file in `.loopr/`; already git-ignored via existing `.git/info/exclude` pattern for `.loopr/**`.
- Session manifest contains target paths; these are not secret but are user-identifying. XDG permissions prevent cross-user reads.
- No secrets flow through session state. API keys stay in env vars per `crates/loopr/src/config.rs:resolve_api_key`.

### Testing Strategy

- Per-phase unit tests cover each component (ProcessId rename, SessionId allocation, target-slug, XDG path resolution, pointer atomicity, fanout layer).
- Integration test at Phase 5: `cargo test -p loopr --test session_e2e` spawns a daemon, submits a plan, asserts the expected XDG paths exist and are populated.
- Stress test at Phase 4: 50 concurrent `loopr works` invocations on a target with no active session; all must converge on one `sn-<id>`.
- Migration test at Phase 9: target with a populated `<target>/.loopr/runs/` directory; new daemon ignores it, writes only to XDG.

### Rollout Plan

Per phase:
1. Implement + unit tests.
2. `otto ci` at crate and workspace root.
3. Commit with message `feat(<crate>): <phase name>`.
4. Move on.

Final phase bumps workspace version; ship as v0.6.0 (minor bump reflects the directory-schema change; not semver-breaking in a library sense because loopr is a binary, but recorded at minor to signal "things moved").

Users on older versions will see empty `<target>/.loopr/runs/` (untouched legacy) and a new `.loopr/active-session` pointer. Their next `loopr daemon start` forks under the new scheme.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| XDG lookup fails (container without `$HOME`, permission error) | Low | High (subscriber can't init; daemon can't start) | Fallback `persistence.telemetry: local` config flag reverts to `<target>/.loopr/sessions/` target-nested mode. Default XDG; flag overrides. |
| Active-session pointer gets corrupted | Low | Medium (process can't resolve session) | Treat unreadable/malformed pointer as absent; allocate new session. Add a `warn!` log when this fires so operators notice. |
| Target path rename orphans XDG history | Medium | Low (old-slug sessions become unreachable by CLI; data persists on disk) | `loopr sessions migrate --from <old-slug> --to <new-slug>` as an earned feature. Document the slug-path coupling in user-facing help. |
| Concurrent client calls race on active-session allocation | Medium | Low (duplicate sessions allocated; one wins) | Pointer write uses write-to-temp + rename + fsync; allocator uses O_CREAT+O_EXCL on manifest.yml; losers re-read pointer and retry. Stress test at Phase 4 covers. |
| XDG dir grows unboundedly without retention | High (over time) | Medium (disk pressure) | `loopr sessions prune` CLI + config `sessions.retention.keep_last: 50` as earned features. First-gate: accept unbounded growth; monitor. |
| Cross-target session manifest becomes inconsistent if only some targets are reachable | Low | Medium (manifest claims targets that are gone) | Manifest is derived; rebuildable from per-target-slug subtrees. `loopr sessions reconcile <id>` rebuilds manifest by walking targets. Earned feature. |
| `dirs` crate resolves `data_local_dir()` to a path without `loopr/` and creation fails | Low | High (subscriber init fails) | Check + create-dir-all at resolver time; surface a clear error if creation fails; Phase 4 unit test covers. |
| Daemon writes events to an ended session's dir | Medium | Low (cosmetic; events still captured) | End-of-session is a marker on the manifest, not a filesystem lock. Events after `ended_at` are tagged to the ended session and appear as "post-end activity" in displays. Document as known behavior; TUI renders accordingly. |
| IPC handshake version drift breaks older clients talking to newer daemon (or vice versa) | Low | Medium (handshake fails) | Additive field; older clients omit `session_id`, daemon defaults to its own. Document compatibility window. Reject only on major protocol-version mismatch. |

## Open Questions

- [ ] **Should the daemon's own startup/reconcile/shutdown events belong to a "daemon-boot" session created at fork, or the active session at fork time?** Leaning: daemon-boot session created at fork; daemon's own bookkeeping events go there. User-initiated work (plan.create et al.) tags events with the caller's session-id via handshake. This cleanly separates "the daemon booted and swept" from "the user submitted a plan."
- [ ] **On `loopr sessions end`, should currently-in-flight work (Works whose Ralph loops are mid-iteration) continue under the ended session, or be blocked pending user action?** Leaning: continue under the ended session; `ended_at` marks user intent but doesn't halt the daemon. User can `loopr sessions resume` to re-own the session, or `loopr sessions status` to see what's still happening.
- [ ] **Target-slug collisions.** `/home/a` and `/home-a` both slug to `-home-a`. Should we use a more collision-proof scheme (e.g., path hash or path + hash suffix)? Leaning: keep claude-style path-slug for readability; collision is a pathological edge case; if it occurs, error clearly at slug allocation with guidance.
- [ ] **Should session manifests be git-committable in the target's taskstore, providing record-of-record for post-hoc audit?** Leaning: no; XDG stays user-scope; committing session state would leak activity patterns to git history. Keep XDG ephemeral-ish (retention-managed) and taskstore FSM-truth-only.
- [ ] **Can the TUI span multiple targets in one session?** Architecture says yes; first-gate CLI doesn't exercise it. When the TUI lands, it picks up the architecture as-is. No forcing decision in this doc.
- [ ] **`loopr logs runs` CLI rename.** Two shapes are reasonable: (a) `loopr logs sessions` (primary) with `--process <pc-id>` to drill down; (b) split into `loopr logs sessions` and `loopr logs processes` as sibling verbs. Leaning (a): sessions are the user-facing unit, processes are an implementation detail; surface the detail as a filter, not a primary verb.
- [ ] **Should `<target>/.loopr/active-session` be a symlink into XDG instead of a pointer file?** Symlink makes `ls -l` show the session path. File makes the contract explicit (it's data, not a reference). Leaning: file. Symlinks invite accidental `rm` breakage.

## References

- `docs/vision.md` - architectural shape, Observability section, target-repo layout commitments
- `docs/roadmap.md` - Stage 9 (blocked on stable layout)
- `docs/design/2026-04-19-telemetry-stage-2.md` - original telemetry subscriber design; this doc supersedes the path construction
- `docs/design/2026-04-23-cli-plumbing-shape.md` - CLI plumbing shape; this doc adds `loopr sessions` verbs and the `--session` global flag
- `docs/design/2026-04-24-instrumentation-sweep.md` - depends on this doc for target-local records/ path; Phase 8.5 summaries + Phase 8.6 transcripts write under `<target>/.loopr/records/` per this schema
- `crates/telemetry/src/runid.rs` - existing RunId (to be renamed SessionId in Phase 1); format already matches target session-id format
- `crates/telemetry/src/subscriber.rs` - existing subscriber path construction (to be rewired in Phase 5)
- `crates/loopr/src/lib.rs:116` and `crates/loopr/src/daemon.rs:277` - existing `RunId::allocate` call sites
- `~/.claude/projects/<path-slug>/` - claude-code's session storage pattern; this doc's XDG layout is the loopr analog
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html)
