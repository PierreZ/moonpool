//! Shared reconnect state kept behind the channel mutex.

use std::task::Waker;

use super::H2Channel;
use crate::ChannelError;

/// Mutable state shared by channel clones and the connection task.
pub(super) struct Inner<B> {
    pub(super) conn: Conn<B>,
    pub(super) failures: u32,
    pub(super) generation: u64,
    wakers: Vec<Waker>,
}

impl<B> Inner<B> {
    pub(super) fn new() -> Self {
        Self {
            conn: Conn::Disconnected,
            failures: 0,
            generation: 0,
            wakers: Vec::new(),
        }
    }

    /// Park a readiness caller once, preserving first-poll order.
    pub(super) fn park(&mut self, waker: &Waker) {
        if !self.wakers.iter().any(|parked| parked.will_wake(waker)) {
            self.wakers.push(waker.clone());
        }
    }

    /// Consume a failed attempt while leaving every other state untouched.
    pub(super) fn take_failure(&mut self) -> Option<ChannelError> {
        let state = std::mem::replace(&mut self.conn, Conn::Disconnected);
        match state {
            Conn::Failed(error) => Some(error),
            state => {
                self.conn = state;
                None
            }
        }
    }

    /// Drain parked callers so they can be woken after releasing the mutex.
    pub(super) fn take_wakers(&mut self) -> Vec<Waker> {
        std::mem::take(&mut self.wakers)
    }

    #[cfg(test)]
    pub(super) fn parked_count(&self) -> usize {
        self.wakers.len()
    }
}

/// Connection lifecycle and its one-shot failure outcome.
pub(super) enum Conn<B> {
    /// Explicitly shut down; this is a terminal state.
    Closed,
    /// No connection and no attempt running.
    Disconnected,
    /// The previous attempt failed; the next readiness poll reserves this for
    /// its following call.
    Failed(ChannelError),
    /// Backing off, connecting, or handshaking.
    Connecting,
    /// Live connection used to reserve request senders.
    Connected(H2Channel<B>),
}
