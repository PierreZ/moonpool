//! `CompioTimeProvider` on a real compio runtime.

use std::time::Duration;

use moonpool_compio::CompioTimeProvider;
use moonpool_core::{TimeError, TimeProvider};

#[compio::test]
async fn sleeping_advances_now() {
    let time = CompioTimeProvider::new();
    let before = time.now();
    time.sleep(Duration::from_millis(20)).await.expect("sleep");
    let after = time.now();
    assert!(after.saturating_sub(before) >= Duration::from_millis(20));
    assert!(time.timer() >= after, "timer equals now in production");
}

#[compio::test]
async fn a_timeout_returns_the_value_or_elapses() {
    let time = CompioTimeProvider::new();
    assert_eq!(
        time.timeout(Duration::from_secs(5), async { 7 }).await,
        Ok(7)
    );
    let slow = time.sleep(Duration::from_secs(10));
    assert_eq!(
        time.timeout(Duration::from_millis(10), slow).await,
        Err(TimeError::Elapsed)
    );
}

/// compio computes `now + duration` and panics on overflow; the provider
/// treats an unrepresentable deadline as "never".
#[compio::test]
async fn an_unrepresentable_deadline_never_fires() {
    let time = CompioTimeProvider::new();
    assert_eq!(time.timeout(Duration::MAX, async { 3 }).await, Ok(3));
    let forever = time.sleep(Duration::MAX);
    assert_eq!(
        time.timeout(Duration::from_millis(10), forever).await,
        Err(TimeError::Elapsed)
    );
}
