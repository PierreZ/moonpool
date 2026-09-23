//! A versioned change latch with any number of waiters.
//!
//! Waiters read the [`version`](Watch::version) *before* they check the
//! state they care about, then wait for the version to move past it. A
//! change is always published by mutating the state first and then calling
//! [`notify`](Watch::notify), so a change made between the read and the wait
//! still wakes the waiter: no lost wakeups, at the price of spurious ones
//! (waiters re-check their state).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct WatchState {
    version: u64,
    closed: bool,
    waiters: BTreeMap<u64, Waker>,
    next_waiter: u64,
}

/// The latch. Closed when its runtime goes away; a closed latch releases
/// every waiter.
#[derive(Default)]
pub(crate) struct Watch {
    state: Mutex<WatchState>,
}

impl Watch {
    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// The current version.
    pub(crate) fn version(&self) -> u64 {
        self.lock().version
    }

    /// Publish a change: bump the version and wake every waiter.
    pub(crate) fn notify(&self) {
        let mut state = self.lock();
        state.version = state.version.wrapping_add(1);
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for waker in waiters.into_values() {
            waker.wake();
        }
    }

    /// Close the latch for good and wake every waiter.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for waker in waiters.into_values() {
            waker.wake();
        }
    }

    /// Resolves once the version differs from `since` (`true`) or the latch
    /// closed (`false`).
    pub(crate) fn changed(self: &Arc<Self>, since: u64) -> Changed {
        Changed {
            watch: Arc::clone(self),
            since,
            waiter: None,
        }
    }

    #[cfg(test)]
    fn waiters(&self) -> usize {
        self.lock().waiters.len()
    }
}

/// See [`Watch::changed`].
pub(crate) struct Changed {
    watch: Arc<Watch>,
    since: u64,
    waiter: Option<u64>,
}

impl Future for Changed {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let watch = Arc::clone(&self.watch);
        let mut state = watch.lock();
        if state.closed {
            return Poll::Ready(false);
        }
        if state.version != self.since {
            return Poll::Ready(true);
        }
        let id = if let Some(id) = self.waiter {
            id
        } else {
            let id = state.next_waiter;
            state.next_waiter += 1;
            id
        };
        state.waiters.insert(id, cx.waker().clone());
        drop(state);
        self.waiter = Some(id);
        Poll::Pending
    }
}

impl Drop for Changed {
    fn drop(&mut self) {
        if let Some(id) = self.waiter {
            self.watch.lock().waiters.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::FutureExt;

    use super::Watch;

    #[test]
    fn a_change_between_read_and_wait_is_not_lost() {
        let watch = Arc::new(Watch::default());
        let seen = watch.version();
        // The state changes after the waiter read the version but before it
        // waits: the wait must complete at once.
        watch.notify();
        assert_eq!(watch.changed(seen).now_or_never(), Some(true));
    }

    #[test]
    fn waiters_wake_on_notify_and_close_and_clean_up_on_drop() {
        let watch = Arc::new(Watch::default());
        let mut pending = Box::pin(watch.changed(watch.version()));
        assert_eq!((&mut pending).now_or_never(), None);
        assert_eq!(watch.waiters(), 1);
        drop(pending);
        assert_eq!(watch.waiters(), 0);

        let mut woken = Box::pin(watch.changed(watch.version()));
        assert_eq!((&mut woken).now_or_never(), None);
        watch.notify();
        assert_eq!(woken.now_or_never(), Some(true));

        let mut closing = Box::pin(watch.changed(watch.version()));
        assert_eq!((&mut closing).now_or_never(), None);
        watch.close();
        assert_eq!(closing.now_or_never(), Some(false));
    }
}
