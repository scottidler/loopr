//! Double-fork primitive.
//!
//! Hand-rolled libc calls that detach the daemon from the parent's
//! controlling terminal. Lifted from v4's `src/daemon.rs`, which has
//! production miles.
//!
//! The dance: `fork` -> `setsid` (child becomes session leader) -> `fork`
//! again (grandchild is no longer a session leader, so it can never acquire
//! a controlling terminal). The grandchild redirects stdio to `/dev/null`
//! so any stray writes don't send SIGTTOU / SIGPIPE when the parent shell
//! exits.
//!
//! IMPORTANT: `fork()` in a multithreaded process is POSIX-undefined. The
//! parent must not have a `tokio::runtime::Runtime` live when it calls
//! `double_fork`. The grandchild creates its own runtime *after* the second
//! fork.

use std::os::unix::io::RawFd;

use crate::error::LooprError;

/// Outcome of a `double_fork`. The parent returns to its client-side caller;
/// the grandchild is the future daemon.
#[derive(Debug)]
pub enum ForkOutcome {
    /// Original process. Return control to the client.
    Parent,
    /// Detached grandchild. The caller must create a tokio runtime and
    /// `block_on(daemon_main(...))`, then `process::exit`.
    Daemon,
}

/// Double-fork and detach. Returns `Parent` to the original caller and
/// `Daemon` only inside the grandchild process.
///
/// On fork failure, returns `LooprError::DaemonStartup` to the parent; the
/// error never surfaces inside the child or grandchild (they either become
/// the daemon or `process::exit`).
pub fn double_fork() -> Result<ForkOutcome, LooprError> {
    // First fork: the parent waits for the intermediate child, then returns.
    // SAFETY: fork() is async-signal-safe and called from a single-threaded
    // process. The caller contract for `double_fork` requires no tokio
    // runtime to be live.
    let first = unsafe { libc::fork() };
    if first < 0 {
        return Err(LooprError::DaemonStartup(format!(
            "fork (first) failed: errno {}",
            last_errno()
        )));
    }
    if first > 0 {
        // Parent: wait for the intermediate child to exit so we don't leave
        // a zombie. The intermediate child exits immediately after the
        // second fork, so this returns within a millisecond.
        // SAFETY: waitpid(pid, &status, 0) is async-signal-safe.
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(first, &mut status, 0) };
        return Ok(ForkOutcome::Parent);
    }

    // Intermediate child. Detach from the controlling terminal, then fork
    // again so the grandchild is not a session leader.
    //
    // SAFETY: setsid() is async-signal-safe. Called from a single-threaded
    // child (we never spawn threads before here).
    let sid = unsafe { libc::setsid() };
    if sid < 0 {
        // SAFETY: _exit() is async-signal-safe; we cannot use process::exit
        // in an intermediate child because the Rust runtime is in an
        // indeterminate state post-fork.
        unsafe { libc::_exit(1) };
    }

    // Second fork. The grandchild is what becomes the daemon.
    // SAFETY: same as first fork; still single-threaded.
    let second = unsafe { libc::fork() };
    if second < 0 {
        unsafe { libc::_exit(1) };
    }
    if second > 0 {
        // Intermediate child's only job was the second fork; exit now so
        // the original parent's waitpid returns.
        unsafe { libc::_exit(0) };
    }

    // Grandchild: redirect stdio to /dev/null and return.
    redirect_stdio_to_devnull();
    Ok(ForkOutcome::Daemon)
}

/// Redirect stdin, stdout, stderr to `/dev/null`. Called inside the
/// grandchild so stray writes don't go to the parent's terminal.
///
/// Errors are swallowed: if `/dev/null` can't be opened we're already in a
/// very weird state and the caller will notice when bind() or write() fails.
fn redirect_stdio_to_devnull() {
    // SAFETY: `open` is async-signal-safe; the path is a valid NUL-terminated C string.
    let fd: RawFd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return;
    }
    // SAFETY: dup2 is async-signal-safe; stdio FDs are 0/1/2.
    unsafe {
        libc::dup2(fd, libc::STDIN_FILENO);
        libc::dup2(fd, libc::STDOUT_FILENO);
        libc::dup2(fd, libc::STDERR_FILENO);
        if fd > libc::STDERR_FILENO {
            libc::close(fd);
        }
    }
}

/// Read `errno` from the current thread. Used only for error-message
/// decoration on fork failure (the parent still gets a sensible
/// `LooprError::DaemonStartup`).
fn last_errno() -> i32 {
    // SAFETY: __errno_location / errno access is async-signal-safe on Linux
    // and macOS; we use libc::__errno_location on Linux and the equivalent
    // on other platforms via std::io::Error::last_os_error, which is a
    // Rust-stdlib wrapper that is safe to call post-fork in the parent
    // (we're calling this on the PARENT side only).
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
