# agents test hang - 2026-05-24

## Summary

A `cargo test --workspace` run in the loopr-v5 repo launched the `agents` test binary at 15:14 on 2026-05-24. The binary hung in a deadlocked state for over 7 hours, consuming ~99% of one CPU core and contributing to system swap exhaustion. It was killed at approximately 22:31.

## Origin

A previous Claude session (`--resume 83363a5c`) ran the following shell loop:

```
for repo in ~/repos/scottidler/aka ~/repos/scottidler/loopr-v5 ~/repos/scottidler/second-brain; do
  otto -C "$repo" ci
done
```

The `otto ci` task for loopr-v5 runs `cargo test --workspace`. That spawned the `agents` test binary. The full process chain:

```
claude --resume 83363a5c (PID 856068)
  zsh -c [...loop...] (PID 1885154)
    otto -C ~/repos/scottidler/loopr-v5 ci (PID 1902765)
      bash .../tasks/test/script.sh (PID 1902807)
        cargo test --workspace (PID 1902839)
          agents-758f0deb91919c02 (PID 1949212)  <-- hung here
```

## Process state at time of inspection

- **State:** sleeping (S)
- **Blocking syscall:** `futex_do_wait` - blocked on a futex, not spinning
- **Threads:** 2
- **Open file descriptors:** stdin=/dev/null, stdout/stderr piped to cargo, 2 Unix domain sockets (connected to the test harness), epoll and eventfd handles
- **Nonvoluntary context switches:** 0 - never preempted; pure voluntary block
- **Accumulated CPU:** 432 minutes over ~7 hours (approximately 1 full core)
- **RSS:** ~196 MB

The 99% CPU figure in `ps aux` reflects accumulated CPU time averaged over the process lifetime, not a current spin. The process had been deadlocked for most of its lifetime.

## Likely cause

The test binary covers `crates/agents/tests/agents_visibility.rs`, which exercises `Lifeguard`, `parse_actions`, and the telemetry subscriber via `telemetry::init_for_test`. These tests are synchronous and structurally simple. The hang is unlikely to be in the test assertions themselves.

The two Unix domain sockets suggest the process was connected to a local peer (test harness or an agent under test) and blocked waiting for a response that never arrived. The futex block indicates a lock or async channel that was never signaled - consistent with a deadlock in the agent coordination logic or a test that spawns an async task and waits for it to complete.

## What was not captured

- stdout/stderr were piped to cargo and are gone; no test output survived
- No core dump was taken before killing
- The specific test that hung was not identified

## Resolution

Process killed with `kill -9 1949212` at approximately 22:31. The cargo and otto parent processes exited as a result.

## Follow-up

- Identify which test in the `agents` crate hangs. Run `cargo test -p agents -- --test-threads=1 --nocapture` and watch which test does not return.
- Add a timeout to any test that spawns async tasks or connects sockets.
- The claude session that kicked this off (83363a5c) was still alive at the time of discovery. Consider whether unattended `otto ci` loops across repos should have a wall-clock timeout.
