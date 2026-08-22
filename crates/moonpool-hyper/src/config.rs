//! Settings shared by the client and server sides.

use std::time::Duration;

/// HTTP/2 keepalive settings.
///
/// These are the three h2 PING knobs hyper exposes on both its client and its
/// server builders, grouped so a channel or a serve helper can take them as one
/// optional value: `None` means no keepalive, hyper's default.
///
/// Under the simulation the pings run on provider time (see
/// [`HyperTimer`](crate::HyperTimer)), so keepalive-driven connection drops are
/// deterministic and reproducible from a seed.
#[derive(Clone, Debug)]
pub struct KeepAlive {
    /// How often to send a keepalive PING.
    pub interval: Duration,
    /// How long to wait for the PONG before considering the connection dead.
    pub timeout: Duration,
    /// Whether to keep pinging a connection with no open streams. When
    /// `false`, an idle connection is left alone until it is used again.
    pub while_idle: bool,
}
