//! The campaign's runtime configuration: every [`PeerPolicy`] field is a
//! BUGGIFY knob, so a seed may run with extreme reconnect, liveness, idle
//! or failure-detection timing.

use std::time::Duration;

use moonpool_rpc::{InboundSharing, PeerPolicy, RpcConfig};
use moonpool_sim::buggify_knob;

fn millis(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// A configuration with each timing knob drawn for this run.
///
/// Defaults are `FoundationDB`'s simulated flow knobs (a 750 ms ping loop,
/// 1.5 s ping timeout, 1 s failure detection); a knob that fires moves to
/// an extreme that is still a valid configuration (the policy validates).
#[must_use]
pub fn delivery_config() -> RpcConfig {
    delivery_config_sharing(None)
}

/// Map a draw in `0..3` to a sharing mode: disabled, same-IP, trusted.
///
/// The simulator gives accepted sessions a synthesized peer IP near the
/// dialer's (as `FoundationDB`'s `sim2` does), so `SameIp` mostly refuses
/// there: it exercises the unverified path, `Trusted` the sharing path.
#[must_use]
pub fn sharing_from(draw: u8) -> InboundSharing {
    match draw {
        0 => InboundSharing::Disabled,
        1 => InboundSharing::SameIp,
        _ => InboundSharing::Trusted,
    }
}

/// [`delivery_config`] with the sharing mode forced (peers draw theirs per
/// process, so a run meets mixed settings).
#[must_use]
pub fn delivery_config_sharing(sharing: Option<InboundSharing>) -> RpcConfig {
    let mut config = knobbed();
    if let Some(sharing) = sharing {
        config.peer.share_inbound_sessions = sharing;
    }
    config
}

fn knobbed() -> RpcConfig {
    let initial = buggify_knob!(50, 1..400);
    let max = buggify_knob!(500, 1..3000).max(initial);
    let ping_interval = buggify_knob!(750, 20..3000);
    let ping_timeout = buggify_knob!(1500, 30..6000);
    let idle = buggify_knob!(30_000, 50..3000);
    let inbound_idle = buggify_knob!(36_000, 100..8000).max(ping_interval * 4);
    let peer = PeerPolicy {
        initial_reconnect_delay: millis(initial),
        max_reconnect_delay: millis(max),
        reconnect_growth_percent: buggify_knob!(120, 100..400),
        reconnect_reset_after: millis(buggify_knob!(5000, 10..8000)),
        jitter_percent: buggify_knob!(10, 0..90),
        ping_interval: millis(ping_interval),
        ping_timeout: millis(ping_timeout),
        idle_timeout: millis(idle),
        inbound_idle_timeout: millis(inbound_idle),
        failure_detection_delay: millis(buggify_knob!(1000, 0..300)),
        max_failed_endpoints: buggify_knob!(1024, 1..4),
        max_tracked_addresses: 64,
        share_inbound_sessions: sharing_from(buggify_knob!(2, 0..2)),
        always_accept_after: millis(buggify_knob!(1000, 0..8000)),
    };
    RpcConfig {
        max_frame_bytes: 64 * 1024,
        connect_timeout: millis(buggify_knob!(1000, 50..400)),
        handshake_timeout: Duration::from_secs(2),
        peer,
        ..RpcConfig::default()
    }
}
