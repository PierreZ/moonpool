//! The campaign's runtime configuration: the delivery campaign's
//! buggified peer policy, plus stream and admission budgets that a seed
//! may squeeze until streams and requests are refused before admission.

use moonpool_rpc::{ResourceLimits, RpcConfig, StreamPolicy};
use moonpool_sim::buggify_knob;

use crate::delivery::delivery_config;

/// Admitted requests (calls and streams) one connection may owe replies
/// for: small enough that the workload's bursts cross it.
pub const INFLIGHT_PER_CONNECTION: usize = 16;

/// Streams one connection may produce at once: above what saturation
/// opens, below what a burst opens.
pub const STREAMS_PER_CONNECTION: usize = 8;

/// A configuration with each knob drawn for this run.
#[must_use]
pub fn streams_config() -> RpcConfig {
    let mut config = delivery_config();
    config.limits = ResourceLimits {
        max_inflight_per_connection: buggify_knob!(INFLIGHT_PER_CONNECTION, 2..12),
        max_frames_per_batch: buggify_knob!(64usize, 1..4),
        endpoint_queue_bytes: buggify_knob!(16u64 << 20, 256..4096),
        ..ResourceLimits::default()
    };
    config.endpoint_queue_capacity = buggify_knob!(256usize, 1..8);
    config.streams = StreamPolicy {
        window_bytes: buggify_knob!(64u64 << 10, 4096..16384),
        max_streams_per_connection: buggify_knob!(STREAMS_PER_CONNECTION, 1..6),
        ..StreamPolicy::default()
    };
    config
}
