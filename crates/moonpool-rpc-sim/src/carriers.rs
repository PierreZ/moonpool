//! Which connections carried a request, as the simulated network saw it.
//!
//! An oracle that bounds how many times a reliable call may run must not
//! take the bound from the runtime it judges: the runtime's own counters
//! would excuse the very retransmission it should catch. Instead, each
//! request carries a mark ([`mark`]): a fixed magic and the request's id,
//! as raw bytes in its payload. A [`NetworkTap`] looks for marks in every
//! byte the simulated network delivers and records, per id, the
//! connections that carried one. One connection carries a request at most
//! once (moonpool-rpc never sends a request twice on one connection, and
//! the network does not duplicate bytes), so a request can run at most
//! once per connection that carried it: `executions <= carriers(id)`.
//!
//! A bit flip could hide a mark; every campaign that uses this masks bit
//! flips ([`crate::without_corruption`]). A reply that echoes the mark is
//! counted on the same connection as its request, so it does not loosen
//! the bound.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use moonpool_sim::{DeliveredBytes, NetworkTap, SimContext, StateHandle};

const CARRIERS_KEY: &str = "rpc_sim_carriers";

/// The first bytes of every mark: chosen so that no protobuf field of the
/// campaigns' messages is likely to spell them by chance.
const MAGIC: [u8; 8] = [0xF5, b'm', b'a', b'r', b'k', 0x5F, 0xA7, 0x3C];

/// The length of a mark: the magic and the id (little endian).
const MARK_LEN: usize = MAGIC.len() + 8;

/// The mark a request with id `id` carries in its payload.
#[must_use]
pub fn mark(id: u64) -> Vec<u8> {
    let mut mark = Vec::with_capacity(MARK_LEN);
    mark.extend_from_slice(&MAGIC);
    mark.extend_from_slice(&id.to_le_bytes());
    mark
}

#[derive(Default)]
struct Inner {
    /// The last `MARK_LEN - 1` bytes of each direction of each connection,
    /// so a mark split across two deliveries is still found.
    tails: BTreeMap<(u64, Option<IpAddr>, Option<IpAddr>), Vec<u8>>,
    /// The connections that carried each id.
    carriers: BTreeMap<u64, BTreeSet<u64>>,
    /// Whether the tap is installed in this run.
    tapped: bool,
}

impl Inner {
    fn delivered(&mut self, delivery: &DeliveredBytes<'_>) {
        let key = (delivery.connection, delivery.from, delivery.to);
        let mut window = self.tails.remove(&key).unwrap_or_default();
        window.extend_from_slice(delivery.bytes);
        let mut at = 0;
        while let Some(found) = find(&window[at..], &MAGIC) {
            let start = at + found;
            let Some(id_bytes) = window.get(start + MAGIC.len()..start + MARK_LEN) else {
                break;
            };
            let id = u64::from_le_bytes(id_bytes.try_into().expect("eight bytes"));
            self.carriers
                .entry(id)
                .or_default()
                .insert(delivery.connection);
            at = start + MARK_LEN;
        }
        let keep = window.len().saturating_sub(MARK_LEN - 1).max(at);
        window.drain(..keep.min(window.len()));
        self.tails.insert(key, window);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The run's record of which connections carried which request. Clones
/// share it.
#[derive(Clone, Default)]
pub struct Carriers {
    inner: Arc<Mutex<Inner>>,
}

impl Carriers {
    /// The run's record, created on first use.
    #[must_use]
    pub fn of(state: &StateHandle) -> Self {
        if let Some(carriers) = state.get::<Self>(CARRIERS_KEY) {
            return carriers;
        }
        let carriers = Self::default();
        state.publish(CARRIERS_KEY, carriers.clone());
        carriers
    }

    /// The run's record, with its tap installed in the simulated network
    /// (once per run).
    ///
    /// # Panics
    ///
    /// The tap panics if a prior task panicked while holding the record.
    #[must_use]
    pub fn tapped(ctx: &SimContext) -> Self {
        let carriers = Self::of(ctx.state());
        let install = {
            let mut inner = carriers.lock();
            !std::mem::replace(&mut inner.tapped, true)
        };
        if install {
            let inner = Arc::clone(&carriers.inner);
            let tap: NetworkTap = Arc::new(move |delivery: &DeliveredBytes<'_>| {
                inner
                    .lock()
                    .expect("Mutex poisoned: prior task panicked")
                    .delivered(delivery);
            });
            ctx.tap_network(tap);
        }
        carriers
    }

    /// How many distinct connections carried request `id`.
    #[must_use]
    pub fn count(&self, id: u64) -> usize {
        self.lock().carriers.get(&id).map_or(0, BTreeSet::len)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deliver(inner: &mut Inner, connection: u64, bytes: &[u8]) {
        inner.delivered(&DeliveredBytes {
            connection,
            from: None,
            to: None,
            bytes,
        });
    }

    #[test]
    fn a_mark_split_across_deliveries_is_found() {
        let mut inner = Inner::default();
        let mut stream = b"head".to_vec();
        stream.extend(mark(7));
        stream.extend(b"tail");
        for cut in 0..stream.len() {
            let mut inner_cut = Inner::default();
            deliver(&mut inner_cut, 1, &stream[..cut]);
            deliver(&mut inner_cut, 1, &stream[cut..]);
            assert_eq!(
                inner_cut.carriers.get(&7).map(BTreeSet::len),
                Some(1),
                "cut at {cut}"
            );
        }
        deliver(&mut inner, 1, &stream);
        assert_eq!(inner.carriers.get(&7).map(BTreeSet::len), Some(1));
    }

    #[test]
    fn a_second_send_on_one_connection_counts_once_and_a_new_connection_twice() {
        let mut inner = Inner::default();
        deliver(&mut inner, 1, &mark(3));
        deliver(&mut inner, 1, &mark(3));
        assert_eq!(inner.carriers.get(&3).map(BTreeSet::len), Some(1));
        deliver(&mut inner, 5, &[mark(3), mark(4)].concat());
        assert_eq!(inner.carriers.get(&3).map(BTreeSet::len), Some(2));
        assert_eq!(inner.carriers.get(&4).map(BTreeSet::len), Some(1));
    }

    #[test]
    fn the_tail_stays_short() {
        let mut inner = Inner::default();
        deliver(&mut inner, 1, &[0u8; 4096]);
        assert!(inner.tails.values().all(|tail| tail.len() < MARK_LEN));
    }
}
