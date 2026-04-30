# Rename Role::Coordinator to Role::Reactor

The `Coordinator` Role variant is a v3 fossil. v3 had `Coordinator` as the top LLM agent; v4 renamed that agent to `Director` but kept the `Coordinator` enum variant as the authorizer of mechanical FSM edges performed by the daemon. v5 inherited the v4 shape, and `roles-and-states.md` codified the rule that "Coordinator never holds an LLM opinion." But the name still reads as an agent, and the deferred roadmap accidentally re-introduced a "Coordinator agent" entry that collides with the daemon-only meaning. We are renaming `Role::Coordinator` to `Role::Reactor` so the daemon's deterministic FSM authority has a name that cannot be mistaken for an LLM agent — and the future LLM-driven orchestration agent (v3's Coordinator-equivalent, partially described in the deferred-roadmap as "1.2 Coordinator agent") collapses cleanly into Director with two delivery phases. "Reactor" also encodes vision.md's "Loopr is reactive" thesis directly into the Role taxonomy, contrasting cleanly with `Director` (deterministic plane vs. judgment plane).

## Status

Decided. Doc rename: commit `305bcdb`. Code rename: commit `d58d0a9`.

## Considered alternatives

- **Rename to `Daemon`.** Rejected: collides with the OS-process meaning used everywhere (`loopr daemon start`, `DaemonContext`, `daemon.pid`).
- **Rename to `Router`.** Rejected: collides with `crates/tools::LaneRouter`, which routes tools to concurrency lanes — different layer, but two `Router`s in the workspace creates noise.
- **Keep `Coordinator` and let it have two implementations (deterministic daemon and LLM agent).** Rejected: kills the load-bearing "Coordinator never holds an LLM opinion" invariant from `roles-and-states.md`; future readers cannot trust a `by (Coordinator)` annotation on an FSM edge to mean mechanical.
