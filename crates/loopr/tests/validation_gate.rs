//! Phase 12 of `docs/design/2026-07-11-verified-swarm.md`: end-to-end
//! proof that `integrator.require-validation` (default `true`) is a real
//! daemon-startup gate, not just a unit-tested config-struct method.
//! Exercises the compiled `loopr` binary against a live fork attempt so
//! the client-visible failure (or success) is asserted, not just the
//! in-process `IntegratorSection::validate()` return value.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

mod common;
use common::init_git_repo;

/// XDG-isolated `loopr` subprocess. Deliberately does NOT call any
/// validation opt-out helper — every test here writes (or omits) its
/// own `.loopr/config.yml` to exercise a specific gate state.
fn loopr(target: &Path) -> Command {
    let mut cmd = Command::cargo_bin("loopr").unwrap();
    cmd.env("XDG_DATA_HOME", target.join(".xdg"));
    cmd.env("XDG_CONFIG_HOME", target.join(".xdg"));
    cmd
}

fn write_config(target: &Path, body: &str) {
    let loopr_dir = target.join(".loopr");
    fs::create_dir_all(&loopr_dir).unwrap();
    fs::write(loopr_dir.join("config.yml"), body).unwrap();
}

/// Success criterion: "daemon start on a target with no validation
/// config fails with the named error." No `.loopr/config.yml` at all
/// means `require-validation: true` (default) + `validation-commands: []`
/// (default) -- the daemon must refuse to start and the CLI must surface
/// the failure naming both knobs.
#[test]
fn no_validation_config_refuses_daemon_start() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_git_repo(target);
    // No config.yml written: pure defaults.

    // Phase 16 of `docs/design/2026-07-11-verified-swarm.md`: read verbs
    // (`plans` among them) no longer auto-fork a daemon, so this gate is
    // now exercised via an explicit `daemon start` -- the same
    // `ensure_daemon` fork path, just invoked directly instead of via a
    // read verb's now-removed auto-fork side effect.
    let assertion = loopr(target)
        .args(["-C", target.to_str().unwrap(), "daemon", "start"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("integrator.require-validation"),
        "error names the require-validation knob: {stderr}"
    );
    assert!(
        stderr.contains("integrator.validation-commands"),
        "error names the validation-commands knob: {stderr}"
    );
    // The daemon never bound a socket; no pid file should exist.
    assert!(
        !target.join(".loopr").join("daemon.pid").exists(),
        "refused daemon must not leave a pid file behind"
    );
}

/// Success criterion: with `["true"]` (a trivial always-green command),
/// the daemon starts normally.
#[test]
fn configured_validation_commands_permit_daemon_start() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_git_repo(target);
    write_config(target, "integrator:\n  validation-commands:\n    - \"true\"\n");

    // Phase 16: `plans` no longer auto-forks; start the daemon explicitly.
    loopr(target)
        .args(["-C", target.to_str().unwrap(), "daemon", "start"])
        .assert()
        .success();
    assert!(
        target.join(".loopr").join("daemon.pid").exists(),
        "configured validation-commands must permit daemon startup"
    );

    common::stop_daemon_for(target);
}

/// Success criterion: `require-validation: false` is the explicit
/// operator escape hatch — legacy behavior (daemon starts with no
/// validation-commands configured).
#[test]
fn require_validation_false_permits_daemon_start_with_no_commands() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_git_repo(target);
    write_config(target, "integrator:\n  require-validation: false\n");

    // Phase 16: `plans` no longer auto-forks; start the daemon explicitly.
    loopr(target)
        .args(["-C", target.to_str().unwrap(), "daemon", "start"])
        .assert()
        .success();
    assert!(
        target.join(".loopr").join("daemon.pid").exists(),
        "require-validation: false must permit daemon startup with empty validation-commands"
    );

    common::stop_daemon_for(target);
}
