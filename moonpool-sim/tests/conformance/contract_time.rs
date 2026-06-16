//! Time provider contract: the invariants any drop-in for `tokio::time` must hold.

use moonpool_core::{Providers, TimeProvider};
use std::time::Duration;

/// Assert the [`TimeProvider`] behaves as a faithful replacement for raw
/// `tokio::time` calls. Uses short durations so the real runtime stays fast.
pub(crate) async fn time_contract<P: Providers>(p: &P) {
    let time = p.time();

    // now() is monotonic non-decreasing.
    let first = time.now();
    let second = time.now();
    assert!(second >= first, "now() must be monotonic non-decreasing");

    // timer() is never behind a now() sampled just before it.
    let sampled = time.now();
    assert!(
        time.timer() >= sampled,
        "timer() must be >= a now() sampled immediately before"
    );

    // After sleep(d), now() advanced by at least d.
    let before = time.now();
    time.sleep(Duration::from_millis(5))
        .await
        .expect("sleep should succeed");
    let elapsed = time.now().saturating_sub(before);
    assert!(
        elapsed >= Duration::from_millis(5),
        "sleep(5ms) must advance now() by >= 5ms, got {elapsed:?}"
    );

    // A future that completes within the timeout returns Ok.
    let ok = time
        .timeout(Duration::from_millis(50), async { 42u32 })
        .await;
    assert_eq!(ok.expect("fast future must not time out"), 42);

    // A future that never completes elapses with an error.
    let elapsed = time
        .timeout(Duration::from_millis(5), std::future::pending::<()>())
        .await;
    assert!(
        elapsed.is_err(),
        "timeout over a never-completing future must elapse"
    );
}
