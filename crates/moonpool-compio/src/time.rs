//! Time on the compio runtime: [`CompioTimeProvider`].

use std::future::Future;
use std::time::{Duration, Instant};

use compio_send_wrapper::SendWrapper;
use moonpool_core::{TimeError, TimeProvider};

/// Time provider over `compio::time`.
///
/// [`now`](TimeProvider::now) counts from the creation of the provider and
/// [`timer`](TimeProvider::timer) equals it, as in the Tokio provider.
/// [`sleep`](TimeProvider::sleep) and [`timeout`](TimeProvider::timeout) use
/// compio's timer wheel, so they need a running compio runtime, and the
/// futures they return are bound to the thread that created them (see the
/// crate documentation).
///
/// A duration too large to add to the current instant sleeps forever (and a
/// timeout that large never fires), where compio itself would panic on the
/// overflow.
#[derive(Debug, Clone)]
pub struct CompioTimeProvider {
    start: Instant,
}

impl CompioTimeProvider {
    /// Create a compio time provider whose clock starts at zero now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for CompioTimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `duration` from now is a representable instant: compio computes
/// `Instant::now() + duration` and panics when it is not.
fn has_deadline(duration: Duration) -> bool {
    Instant::now().checked_add(duration).is_some()
}

impl TimeProvider for CompioTimeProvider {
    fn sleep(&self, duration: Duration) -> impl Future<Output = Result<(), TimeError>> + Send {
        tracing::trace!(?duration, "sleep");
        SendWrapper::new(async move {
            if has_deadline(duration) {
                compio::time::sleep(duration).await;
            } else {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn timeout<F, T>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl Future<Output = Result<T, TimeError>> + Send
    where
        F: Future<Output = T> + Send,
        T: Send,
    {
        SendWrapper::new(async move {
            if has_deadline(duration) {
                compio::time::timeout(duration, future)
                    .await
                    .map_err(|_| TimeError::Elapsed)
            } else {
                Ok(future.await)
            }
        })
    }
}
