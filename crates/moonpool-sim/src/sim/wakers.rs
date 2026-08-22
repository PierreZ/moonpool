//! Reusable waker registries for simulation coordination.

use std::{collections::BTreeMap, task::Waker};

use crate::{
    network::sim::{ConnectionId, ListenerId},
    sim::state::FileId,
};

/// A keyed, single-waiter waker registry.
///
/// Registering the same task repeatedly replaces its previous waker only when
/// the executor supplies a different wake target.
#[derive(Debug)]
pub struct WakerRegistry<K> {
    entries: BTreeMap<K, Waker>,
}

impl<K: Ord> WakerRegistry<K> {
    /// Registers `waker` for `key`, deduplicating equivalent wake targets.
    pub fn register(&mut self, key: K, waker: &Waker) {
        if self
            .entries
            .get(&key)
            .is_some_and(|registered| registered.will_wake(waker))
        {
            return;
        }
        self.entries.insert(key, waker.clone());
    }

    pub(crate) fn insert(&mut self, key: K, waker: Waker) {
        if self
            .entries
            .get(&key)
            .is_some_and(|registered| registered.will_wake(&waker))
        {
            return;
        }
        self.entries.insert(key, waker);
    }

    /// Removes and returns the waker registered for `key`.
    pub fn take(&mut self, key: &K) -> Option<Waker> {
        self.entries.remove(key)
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<Waker> {
        self.take(key)
    }

    /// Returns whether `key` has a registered waker.
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.contains(key)
    }

    /// Returns the number of registered keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the registry contains no wakers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Removes every registered waker.
    pub fn drain(&mut self) -> impl Iterator<Item = (K, Waker)> + '_ {
        std::mem::take(&mut self.entries).into_iter()
    }
}

impl<K> IntoIterator for WakerRegistry<K> {
    type Item = (K, Waker);
    type IntoIter = std::collections::btree_map::IntoIter<K, Waker>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<K> Default for WakerRegistry<K> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

/// Waker registries owned by the simulation state.
#[derive(Debug, Default)]
pub(crate) struct Wakers {
    /// Wakers waiting on `accept()` per listener.
    pub(crate) listeners: WakerRegistry<ListenerId>,
    /// Wakers waiting on `read` per connection.
    pub(crate) reads: WakerRegistry<ConnectionId>,
    /// Wakers waiting on time-based events per task id.
    pub(crate) tasks: WakerRegistry<u64>,
    /// Wakers waiting for write clog to clear.
    pub(crate) write_clogs: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for read clog to clear.
    pub(crate) read_clogs: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for cut connections to be restored.
    pub(crate) cuts: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for send buffer space to become available.
    pub(crate) send_buffers: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for storage operations to complete.
    pub(crate) storage_ops: WakerRegistry<(FileId, u64)>,
}

/// Wakers collected while the simulation state is locked.
#[derive(Debug, Default)]
pub(crate) struct WakeBatch(Vec<Waker>);

impl WakeBatch {
    /// Adds an optional waker to this batch.
    pub(crate) fn push(&mut self, waker: Option<Waker>) {
        self.0.extend(waker);
    }

    /// Adds every waker in an iterator to this batch.
    pub(crate) fn extend(&mut self, wakers: impl IntoIterator<Item = Waker>) {
        self.0.extend(wakers);
    }

    /// Invokes all collected wakers.
    pub(crate) fn wake(self) {
        for waker in self.0 {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WakerRegistry;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Wake, Waker},
    };

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn equivalent_wakers_are_deduplicated() {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut registry = WakerRegistry::default();

        registry.register(7, &waker);
        registry.register(7, &waker);
        assert_eq!(registry.len(), 1);

        registry.take(&7).expect("waker should exist").wake();
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_different_waker_replaces_the_previous_one() {
        let first = Arc::new(Counter(AtomicUsize::new(0)));
        let second = Arc::new(Counter(AtomicUsize::new(0)));
        let first_waker = Waker::from(Arc::clone(&first));
        let second_waker = Waker::from(Arc::clone(&second));
        let mut registry = WakerRegistry::default();

        registry.register(7, &first_waker);
        registry.register(7, &second_waker);
        registry.take(&7).expect("waker should exist").wake();

        assert_eq!(first.0.load(Ordering::Relaxed), 0);
        assert_eq!(second.0.load(Ordering::Relaxed), 1);
    }
}
