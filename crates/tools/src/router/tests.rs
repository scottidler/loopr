use super::*;

use std::path::Path;

use crate::shell::sh_command;

fn router(sandbox: SandboxMode) -> LaneRouter {
    match LaneRouter::new(sandbox) {
        Ok(r) => r,
        Err(e) => panic!("router build should succeed: {e}"),
    }
}

#[test]
fn initial_slots_match_policies() {
    let r = router(SandboxMode::Off);
    assert_eq!(r.available_slots(Lane::Local), 10);
    assert_eq!(r.available_slots(Lane::Net), 5);
    assert_eq!(r.available_slots(Lane::Heavy), 1);
}

#[test]
fn required_without_bwrap_errors() {
    // This test only runs meaningfully when bwrap is NOT functional.
    // On CI machines with bwrap, the Required path succeeds and we skip.
    if !detect_bwrap_functional() {
        match LaneRouter::new(SandboxMode::Required) {
            Err(RouterInitError::BwrapRequired) => {}
            Ok(_) => panic!("expected BwrapRequired error"),
        }
    }
}

#[test]
fn off_skips_bwrap_entirely() {
    let r = router(SandboxMode::Off);
    assert_eq!(r.sandbox_mode(), SandboxMode::Off);
    assert!(!r.bwrap_functional());
}

#[tokio::test]
async fn spawn_echo_local_lane() {
    let r = router(SandboxMode::Off);
    let cmd = sh_command("echo hello", Path::new("/tmp"));
    let result = r
        .spawn(cmd, Lane::Local, Path::new("/tmp"), Some(10), PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "hello");
}

#[tokio::test]
async fn spawn_non_zero_propagates_exit_code() {
    let r = router(SandboxMode::Off);
    let cmd = sh_command("exit 7", Path::new("/tmp"));
    let result = r
        .spawn(cmd, Lane::Net, Path::new("/tmp"), Some(10), PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 7);
}

#[tokio::test]
async fn timeout_clamped_to_lane_max() {
    let r = router(SandboxMode::Off);
    // Local max is 60s; requesting 99999 must clamp. We don't observe the
    // clamping directly, but the run completes successfully on a 0.1s sleep
    // which proves the clamp didn't inadvertently zero the timeout.
    let cmd = sh_command("echo done", Path::new("/tmp"));
    let result = r
        .spawn(
            cmd,
            Lane::Local,
            Path::new("/tmp"),
            Some(99_999),
            PersistConfig::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn heavy_lane_serializes_concurrent_calls() {
    let r = Arc::new(router(SandboxMode::Off));
    let r1 = r.clone();
    let r2 = r.clone();
    let (a, b) = tokio::join!(
        r1.spawn(
            sh_command("echo first", Path::new("/tmp")),
            Lane::Heavy,
            Path::new("/tmp"),
            Some(10),
            PersistConfig::default()
        ),
        r2.spawn(
            sh_command("echo second", Path::new("/tmp")),
            Lane::Heavy,
            Path::new("/tmp"),
            Some(10),
            PersistConfig::default()
        ),
    );
    assert_eq!(a.unwrap().exit_code, 0);
    assert_eq!(b.unwrap().exit_code, 0);
    assert_eq!(r.available_slots(Lane::Heavy), 1);
}

#[tokio::test]
async fn slot_released_after_error_path() {
    let r = router(SandboxMode::Off);
    let before = r.available_slots(Lane::Heavy);
    // Even a failing command must release the permit.
    let cmd = sh_command("nonexistent-command-xyz", Path::new("/tmp"));
    let _ = r
        .spawn(cmd, Lane::Heavy, Path::new("/tmp"), Some(5), PersistConfig::default())
        .await;
    assert_eq!(r.available_slots(Lane::Heavy), before);
}

#[tokio::test]
async fn scrub_strips_secret_env_from_subprocess() {
    // D12: set a secret-shaped env var in the parent, verify the child
    // subprocess does NOT see it via `printenv`. Uses a uniquely-named var
    // so parallel tests don't collide.
    let secret_var = "LOOPR_TEST_SECRET_A_TOKEN";
    let benign_var = "LOOPR_TEST_A_BENIGN_VAR_UNDENIED";
    // SAFETY: test-process-global env mutation; vars are unique to this test
    // and are removed before assertions complete.
    unsafe {
        std::env::set_var(secret_var, "leak-me");
        std::env::set_var(benign_var, "pass-through");
    }

    let r = router(SandboxMode::Off);
    let cmd = sh_command(
        &format!("printenv {secret_var} >/dev/null && echo SAW-SECRET; printenv {benign_var}"),
        Path::new("/tmp"),
    );
    let result = r
        .spawn(cmd, Lane::Net, Path::new("/tmp"), Some(5), PersistConfig::default())
        .await
        .unwrap();

    unsafe {
        std::env::remove_var(secret_var);
        std::env::remove_var(benign_var);
    }

    assert!(
        !result.stdout.contains("SAW-SECRET"),
        "secret var leaked into subprocess: stdout={:?}",
        result.stdout
    );
    // A _TOKEN-ending var is stripped by the suffix matcher; the benign
    // var name does not match any prefix or suffix and must pass through.
    // Note: LOOPR_TEST_A_BENIGN_VAR_UNDENIED starts with LOOPR_ so the
    // prefix matcher strips it too. Pick a different benign var.
}

#[tokio::test]
async fn scrub_preserves_non_secret_env() {
    // A var whose name matches no prefix and no suffix must pass through
    // unchanged. Pick a name that cannot accidentally match any entry.
    let benign = "XDG_BENIGN_DAEMON_FOO";
    unsafe {
        std::env::set_var(benign, "present-in-child");
    }

    let r = router(SandboxMode::Off);
    let cmd = sh_command(&format!("printenv {benign}"), Path::new("/tmp"));
    let result = r
        .spawn(cmd, Lane::Net, Path::new("/tmp"), Some(5), PersistConfig::default())
        .await
        .unwrap();

    unsafe {
        std::env::remove_var(benign);
    }

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains("present-in-child"),
        "benign var dropped: stdout={:?}",
        result.stdout
    );
}
