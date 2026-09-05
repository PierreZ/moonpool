//! Reusable waker registries for simulation coordination.

use std::{collections::BTreeMap, task::Waker};

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

    /// Removes and returns the waker registered for `key`.
    pub fn take(&mut self, key: &K) -> Option<Waker> {
        self.entries.remove(key)
    }

    /// Removes every registered waker.
    pub fn drain(&mut self) -> impl Iterator<Item = (K, Waker)> + '_ {
        std::mem::take(&mut self.entries).into_iter()
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
    /// Wakers waiting on time-based events per task id.
    pub(crate) tasks: WakerRegistry<u64>,
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

    /// Merges another batch without waking it yet.
    pub(crate) fn append(&mut self, mut other: Self) {
        self.0.append(&mut other.0);
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

        registry.take(&7).expect("waker should exist").wake();
        assert!(registry.take(&7).is_none(), "one key holds one waker");
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
