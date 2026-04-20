//! Daemon process lifecycle: double-fork detachment, pid/version/run-id
//! sentinels, signal handling, run loop. Owned by the `loopr` driver crate;
//! the `transport` module hangs off the daemon's accept loop but does not
//! know about the fork or the pid file.
//!
//! Submodules: `fork` (libc double-fork primitive), `sentinel` (pid /
//! version / run-id / socket filesystem helpers), `context` (`DaemonContext`
//! shared state).

pub(crate) mod context;
pub(crate) mod fork;
pub(crate) mod sentinel;
