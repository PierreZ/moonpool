//! The messages in flight.

use crate::Rng;

/// The messages in flight: a bounded bag, taken in random order.
///
/// A random [`take`](Self::take) is the reordering of an asynchronous
/// network. A mailbox past its bound **evicts a random message**: a lossy
/// network, and the limit that stops a backlog from starving the work a
/// schedule is meant to reach. Dropping and duplicating a taken message are
/// the caller's coins ([`Walk`](crate::Walk) flips them only while chaos is
/// on).
///
/// The walk owns one mailbox for each run. The ordered methods
/// ([`pop_front`](Self::pop_front), [`retain`](Self::retain),
/// [`take_where`](Self::take_where)) are for a scripted schedule, which
/// reaches them through a [`Harness`](crate::Harness).
#[derive(Debug, Clone)]
pub struct Mailbox<M> {
    inflight: Vec<M>,
    bound: usize,
}

impl<M> Mailbox<M> {
    /// An empty mailbox that holds at most `bound` messages.
    #[must_use]
    pub fn new(bound: usize) -> Self {
        Self {
            inflight: Vec::new(),
            bound,
        }
    }

    /// Put `message` in flight. Past the bound, a random message (maybe this
    /// one) is evicted and returned.
    pub fn push(&mut self, message: M, rng: &mut Rng) -> Option<M> {
        self.inflight.push(message);
        if self.inflight.len() <= self.bound {
            return None;
        }
        let index = rng.index(self.inflight.len())?;
        Some(self.inflight.swap_remove(index))
    }

    /// Take a random message out of flight.
    pub fn take(&mut self, rng: &mut Rng) -> Option<M> {
        let index = rng.index(self.inflight.len())?;
        Some(self.inflight.swap_remove(index))
    }

    /// Take the oldest message that `wanted` admits.
    pub fn take_where(&mut self, wanted: impl FnMut(&M) -> bool) -> Option<M> {
        let index = self.inflight.iter().position(wanted)?;
        Some(self.inflight.remove(index))
    }

    /// Take the oldest message in flight.
    pub fn pop_front(&mut self) -> Option<M> {
        if self.inflight.is_empty() {
            return None;
        }
        Some(self.inflight.remove(0))
    }

    /// Take the newest message in flight.
    pub fn pop_back(&mut self) -> Option<M> {
        self.inflight.pop()
    }

    /// Take everything in flight, oldest first.
    pub fn take_all(&mut self) -> Vec<M> {
        std::mem::take(&mut self.inflight)
    }

    /// Keep only the messages `keep` admits: a scripted partition.
    pub fn retain(&mut self, keep: impl FnMut(&M) -> bool) {
        self.inflight.retain(keep);
    }

    /// Drop everything in flight.
    pub fn clear(&mut self) {
        self.inflight.clear();
    }

    /// The messages in flight. Arrival order holds only until the first
    /// random take or eviction.
    #[must_use]
    pub fn messages(&self) -> &[M] {
        &self.inflight
    }

    /// How many messages are in flight.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inflight.len()
    }

    /// Whether nothing is in flight.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inflight.is_empty()
    }

    /// The most messages this mailbox holds.
    #[must_use]
    pub fn bound(&self) -> usize {
        self.bound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_mailbox_evicts_one_message() {
        let mut rng = Rng::new(1);
        let mut mailbox = Mailbox::new(3);
        let evicted: Vec<u32> = (0..10)
            .filter_map(|message| mailbox.push(message, &mut rng))
            .collect();
        assert_eq!(mailbox.len(), 3);
        assert_eq!(mailbox.bound(), 3);
        assert_eq!(evicted.len(), 7);
    }

    #[test]
    fn a_random_take_reorders() {
        let mut rng = Rng::new(2);
        let mut mailbox = Mailbox::new(64);
        for message in 0..32_u32 {
            assert_eq!(mailbox.push(message, &mut rng), None);
        }
        let mut taken = Vec::new();
        while let Some(message) = mailbox.take(&mut rng) {
            taken.push(message);
        }
        assert!(mailbox.is_empty());
        assert_ne!(taken, (0..32).collect::<Vec<_>>(), "not in arrival order");
        taken.sort_unstable();
        assert_eq!(taken, (0..32).collect::<Vec<_>>(), "nothing lost");
    }

    #[test]
    fn the_ordered_methods_script_a_schedule() {
        let mut rng = Rng::new(3);
        let mut mailbox = Mailbox::new(8);
        for message in 1..=6_u32 {
            mailbox.push(message, &mut rng);
        }
        assert_eq!(mailbox.pop_front(), Some(1));
        assert_eq!(mailbox.pop_back(), Some(6));
        assert_eq!(mailbox.take_where(|m| m % 2 == 1), Some(3));
        mailbox.retain(|m| *m != 4);
        assert_eq!(mailbox.messages(), &[2, 5]);
        assert_eq!(mailbox.take_all(), vec![2, 5]);
        assert_eq!(mailbox.pop_front(), None);
        mailbox.push(9, &mut rng);
        mailbox.clear();
        assert!(mailbox.is_empty());
    }
}
