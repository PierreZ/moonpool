//! Per-peer connection selection and reconnect timing.
//!
//! A [`Peer`] is this runtime's relationship with one canonical remote
//! address: at most one *selected* connection carries this runtime's calls
//! to it (dialed here, or accepted and adopted), and a backoff paces how
//! often it is re-dialed. The rules follow `FoundationDB`'s
//! `connectionKeeper`: wait until `last_connect + reconnect_delay` (jittered)
//! before dialing; when a connection ends, reset the delay if it stayed up
//! for [`PeerPolicy::reconnect_reset_after`], otherwise grow it towards
//! [`PeerPolicy::max_reconnect_delay`].

use std::sync::{Arc, Weak};
use std::time::Duration;

use super::connection::Connection;
use crate::config::PeerPolicy;

/// Scale `duration` by a factor in `1 ± jitter_percent / 100`, using a
/// uniform `draw` in `0.0..1.0`.
pub(crate) fn jittered(duration: Duration, jitter_percent: u32, draw: f64) -> Duration {
    let spread = f64::from(jitter_percent.min(99)) / 100.0;
    let factor = 1.0 - spread + 2.0 * spread * draw.clamp(0.0, 1.0);
    duration.mul_f64(factor)
}

/// See the module docs.
#[derive(Default)]
pub(crate) struct Peer {
    /// The selected connection, in any state from waiting to dial to
    /// established.
    pub(crate) current: Option<Arc<Connection>>,
    reconnect_delay: Duration,
    /// When the current or last selected connection was dialed or adopted.
    last_connect: Option<Duration>,
    /// When this runtime started dialing without a connection establishing
    /// since.
    unestablished_since: Option<Duration>,
    /// The latest verified session this peer dialed that the tie-break
    /// left served only: adopted later if this runtime's own dials stall.
    pub(crate) candidate: Option<Weak<Connection>>,
}

impl Peer {
    pub(crate) fn new(policy: &PeerPolicy) -> Self {
        Self {
            current: None,
            reconnect_delay: policy.initial_reconnect_delay,
            last_connect: None,
            unestablished_since: None,
            candidate: None,
        }
    }

    /// The live selected connection, if any.
    pub(crate) fn live(&self) -> Option<&Arc<Connection>> {
        self.current
            .as_ref()
            .filter(|connection| !connection.is_closed())
    }

    /// The candidate session, if it is still open and established.
    pub(crate) fn usable_candidate(&self) -> Option<Arc<Connection>> {
        self.candidate
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|connection| !connection.is_closed() && connection.is_established())
    }

    /// How long to wait at `now` before dialing, and record the dial as
    /// happening then. `draw` is a uniform jitter draw.
    pub(crate) fn next_dial(&mut self, now: Duration, policy: &PeerPolicy, draw: f64) -> Duration {
        let wait = self.last_connect.map_or(Duration::ZERO, |last| {
            (last + self.reconnect_delay).saturating_sub(now)
        });
        let wait = jittered(wait, policy.jitter_percent, draw);
        self.last_connect = Some(now + wait);
        self.unestablished_since.get_or_insert(now);
        wait
    }

    /// A selected connection established: dialing is no longer stalled.
    pub(crate) fn established(&mut self) {
        self.unestablished_since = None;
    }

    /// Whether this runtime has been dialing for at least `delay` at `now`
    /// without any selected connection establishing.
    pub(crate) fn dialing_stalled(&self, now: Duration, delay: Duration) -> bool {
        self.unestablished_since
            .is_some_and(|since| now.saturating_sub(since) >= delay)
    }

    /// An accepted session was adopted as the selected connection at `now`.
    pub(crate) fn adopted(&mut self, now: Duration) {
        self.last_connect = Some(now);
        self.unestablished_since = None;
    }

    /// The selected connection ended at `now`: reset the backoff after a
    /// stable connection, grow it otherwise.
    pub(crate) fn session_ended(&mut self, now: Duration, policy: &PeerPolicy) {
        let stable = self
            .last_connect
            .is_some_and(|last| now.saturating_sub(last) > policy.reconnect_reset_after);
        self.reconnect_delay = if stable {
            policy.initial_reconnect_delay
        } else {
            let grown = self
                .reconnect_delay
                .mul_f64(f64::from(policy.reconnect_growth_percent) / 100.0);
            grown.min(policy.max_reconnect_delay)
        };
    }

    /// Whether forgetting this peer at `now` loses nothing: no connection,
    /// and its backoff would no longer delay a dial.
    pub(crate) fn is_forgettable(&self, now: Duration) -> bool {
        self.live().is_none()
            && self
                .last_connect
                .is_none_or(|last| now.saturating_sub(last) >= self.reconnect_delay)
    }

    #[cfg(test)]
    fn delay(&self) -> Duration {
        self.reconnect_delay
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Peer, jittered};
    use crate::config::PeerPolicy;

    fn policy() -> PeerPolicy {
        PeerPolicy {
            initial_reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_millis(400),
            reconnect_growth_percent: 200,
            reconnect_reset_after: Duration::from_secs(5),
            jitter_percent: 0,
            ..PeerPolicy::default()
        }
    }

    #[test]
    fn backoff_grows_while_unstable_and_resets_only_after_stable_operation() {
        let policy = policy();
        let mut peer = Peer::new(&policy);
        let ms = Duration::from_millis;
        // First dial: no wait.
        assert_eq!(peer.next_dial(ms(0), &policy, 0.5), ms(0));
        // It fails at once: the delay grows, the next dial waits for it.
        peer.session_ended(ms(10), &policy);
        assert_eq!(peer.delay(), ms(200));
        assert_eq!(peer.next_dial(ms(10), &policy, 0.5), ms(190));
        peer.session_ended(ms(300), &policy);
        assert_eq!(peer.delay(), ms(400));
        // Capped at the maximum.
        let _ = peer.next_dial(ms(700), &policy, 0.5);
        peer.session_ended(ms(800), &policy);
        assert_eq!(peer.delay(), ms(400));
        // A connection that stayed up past the reset window resets it.
        let dialed = peer.next_dial(ms(1200), &policy, 0.5);
        peer.session_ended(ms(1200) + dialed + Duration::from_secs(6), &policy);
        assert_eq!(peer.delay(), ms(100));
        // A short one after that grows it again.
        let _ = peer.next_dial(ms(20_000), &policy, 0.5);
        peer.session_ended(ms(20_050), &policy);
        assert_eq!(peer.delay(), ms(200));
    }

    #[test]
    fn dialing_stalls_until_a_connection_establishes() {
        let policy = policy();
        let mut peer = Peer::new(&policy);
        let second = Duration::from_secs(1);
        assert!(!peer.dialing_stalled(Duration::ZERO, Duration::ZERO));
        // Repeated failing dials keep the original start.
        let _ = peer.next_dial(Duration::ZERO, &policy, 0.5);
        peer.session_ended(Duration::from_millis(10), &policy);
        let _ = peer.next_dial(Duration::from_millis(900), &policy, 0.5);
        assert!(!peer.dialing_stalled(Duration::from_millis(900), second));
        assert!(peer.dialing_stalled(second, second));
        peer.established();
        assert!(!peer.dialing_stalled(Duration::from_secs(5), second));
    }

    #[test]
    fn jitter_stays_within_its_band() {
        let base = Duration::from_secs(1);
        assert_eq!(jittered(base, 10, 0.0), Duration::from_millis(900));
        assert_eq!(jittered(base, 10, 1.0), Duration::from_millis(1100));
        assert_eq!(jittered(base, 0, 0.3), base);
        assert_eq!(jittered(Duration::ZERO, 50, 0.9), Duration::ZERO);
    }

    #[test]
    fn a_peer_is_forgettable_once_its_backoff_expired() {
        let policy = policy();
        let mut peer = Peer::new(&policy);
        assert!(peer.is_forgettable(Duration::ZERO));
        let _ = peer.next_dial(Duration::from_secs(1), &policy, 0.5);
        assert!(!peer.is_forgettable(Duration::from_secs(1)));
        assert!(peer.is_forgettable(Duration::from_secs(2)));
    }
}
