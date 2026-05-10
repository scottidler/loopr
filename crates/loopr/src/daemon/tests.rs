use std::time::Duration;

use super::bound_startup;
use crate::error::LooprError;

#[tokio::test]
async fn bound_startup_passes_through_ok() {
    let result = bound_startup(Duration::from_secs(60), async { Ok::<_, LooprError>(42) }).await;
    assert_eq!(result.expect("ok"), 42);
}

#[tokio::test]
async fn bound_startup_propagates_inner_error() {
    let result = bound_startup(Duration::from_secs(60), async {
        Err::<(), _>(LooprError::DaemonStartup("inner".into()))
    })
    .await;
    let err = result.expect_err("err propagates");
    match err {
        LooprError::DaemonStartup(msg) => assert_eq!(msg, "inner"),
        other => panic!("expected DaemonStartup(\"inner\"), got {other:?}"),
    }
}

#[tokio::test]
async fn bound_startup_returns_daemon_startup_on_elapsed() {
    // Real 50ms budget against a never-resolving future. Plenty of margin
    // under any CI scheduler; the elapsed branch still fires within ~50ms.
    let start = std::time::Instant::now();
    let result = bound_startup::<_, ()>(Duration::from_millis(50), std::future::pending()).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "timeout should fire promptly, took {elapsed:?}"
    );
    let err = result.expect_err("must elapse");
    match err {
        LooprError::DaemonStartup(msg) => {
            // budget.as_secs() truncates 50ms to 0; that's a known edge of
            // the second-granular config field. The point of the test is
            // that the elapsed branch fired and the message format is
            // stable.
            assert!(
                msg.contains("exceeded") && msg.contains("startup budget"),
                "wanted budget breach in error string, got: {msg}"
            );
        }
        other => panic!("expected DaemonStartup, got {other:?}"),
    }
}
