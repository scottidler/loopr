//! Filesystem sentinel helpers. Owns `.loopr/daemon.pid`,
//! `.loopr/daemon.version`, `.loopr/daemon.run-id`, and `.loopr/socket`
//! lifecycle. Pure sync I/O; no tokio.
