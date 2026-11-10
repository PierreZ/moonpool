//! The pending-call table, with the running totals that would otherwise
//! need a scan of every call under the state lock.

use std::collections::{BTreeMap, BTreeSet};

use super::{Origin, PendingCall};

/// Every pending call by id, plus the calls that ride each connection and
/// the bytes that reliable calls retain.
///
/// The totals stay correct only when a call's `origin` changes through
/// [`Self::set_origin`]. Its `retained` body never changes after insert.
#[derive(Default)]
pub(super) struct PendingCalls {
    calls: BTreeMap<u64, PendingCall>,
    /// Call ids by the connection id of their origin, in id order (the
    /// order a scan of `calls` gives, so the iteration stays the same).
    by_connection: BTreeMap<u64, BTreeSet<u64>>,
    /// The sum of the retained bodies.
    retained_bytes: u64,
    /// The calls that retain a body.
    retained_calls: usize,
}

impl PendingCalls {
    /// The number of pending calls.
    pub(super) fn len(&self) -> usize {
        self.calls.len()
    }

    /// Whether no call is pending.
    pub(super) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// The bytes that reliable calls retain for retransmission.
    pub(super) fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// The calls that retain a body.
    pub(super) fn retained_calls(&self) -> usize {
        self.retained_calls
    }

    /// The call `call_id`.
    pub(super) fn get(&self, call_id: u64) -> Option<&PendingCall> {
        self.calls.get(&call_id)
    }

    /// The call `call_id`, to change its attempt flags. Change its origin
    /// with [`Self::set_origin`], never through this reference.
    pub(super) fn get_mut(&mut self, call_id: u64) -> Option<&mut PendingCall> {
        self.calls.get_mut(&call_id)
    }

    /// Every pending call, in id order.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&u64, &PendingCall)> {
        self.calls.iter()
    }

    /// Every pending call, in id order.
    pub(super) fn values(&self) -> impl Iterator<Item = &PendingCall> {
        self.calls.values()
    }

    /// Add call `call_id`.
    pub(super) fn insert(&mut self, call_id: u64, call: PendingCall) {
        drop(self.remove(call_id));
        if let Some(body) = &call.retained {
            self.retained_bytes = self.retained_bytes.saturating_add(body.len() as u64);
            self.retained_calls += 1;
        }
        if let Origin::Connection(id) = call.origin {
            self.by_connection.entry(id).or_default().insert(call_id);
        }
        self.calls.insert(call_id, call);
    }

    /// Remove call `call_id`.
    pub(super) fn remove(&mut self, call_id: u64) -> Option<PendingCall> {
        let call = self.calls.remove(&call_id)?;
        if let Some(body) = &call.retained {
            self.retained_bytes = self.retained_bytes.saturating_sub(body.len() as u64);
            self.retained_calls = self.retained_calls.saturating_sub(1);
        }
        if let Origin::Connection(id) = call.origin {
            self.unlink(id, call_id);
        }
        Some(call)
    }

    /// Move call `call_id` to `origin`. Returns whether the call exists.
    pub(super) fn set_origin(&mut self, call_id: u64, origin: Origin) -> bool {
        let Some(call) = self.calls.get_mut(&call_id) else {
            return false;
        };
        let previous = std::mem::replace(&mut call.origin, origin);
        if let Origin::Connection(id) = previous {
            self.unlink(id, call_id);
        }
        if let Origin::Connection(id) = origin {
            self.by_connection.entry(id).or_default().insert(call_id);
        }
        true
    }

    /// The ids of the calls whose replies come from connection `id`, in id
    /// order.
    pub(super) fn on_connection(&self, id: u64) -> Vec<u64> {
        self.by_connection
            .get(&id)
            .map(|calls| calls.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Whether any call waits for a reply from connection `id`.
    pub(super) fn rides(&self, id: u64) -> bool {
        self.by_connection.contains_key(&id)
    }

    /// Remove every call, in id order.
    pub(super) fn take_all(&mut self) -> Vec<(u64, PendingCall)> {
        self.by_connection.clear();
        self.retained_bytes = 0;
        self.retained_calls = 0;
        std::mem::take(&mut self.calls).into_iter().collect()
    }

    fn unlink(&mut self, id: u64, call_id: u64) {
        if let Some(calls) = self.by_connection.get_mut(&id) {
            calls.remove(&call_id);
            if calls.is_empty() {
                self.by_connection.remove(&id);
            }
        }
    }
}
