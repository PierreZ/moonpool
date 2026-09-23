//! Metrics and diagnostics: the runtime's counters and gauges as a
//! [`MetricsSource`], with a fixed, bounded label set.
//!
//! No metrics runtime of its own: [`RpcMetrics`] flattens
//! [`RpcStats`] into [`MetricSample`]s on demand, which the simulation
//! scrapes into its report and a production adapter can export (the
//! `moonpool-prometheus` crate's registry, OpenTelemetry, ...).
//!
//! # Label cardinality
//!
//! Every series is named `moonpool_rpc_*`. The only labels are drawn from
//! closed enumerations: `reason` on `moonpool_rpc_requests_denied_total`
//! (one value per [`CredentialError`], plus `permission_denied`). No label
//! ever carries a peer address, an endpoint token, a principal or a
//! credential, so the number of series is the same for one peer or a
//! million. Per-peer detail belongs in traces.
//!
//! # Traces
//!
//! `tracing` events at `debug` describe sessions and refusals; `warn`
//! events with target `moonpool_rpc::audit` record security refusals
//! (`rpc_request_denied`, `rpc_connection_refused`) and key replacements
//! (`rpc_verification_keys_replaced`); `info` events mark a graceful
//! shutdown (`rpc_shutdown_started`, `rpc_shutdown_drained`). None of them
//! carries credential bytes.

use std::sync::Arc;

use moonpool_core::Providers;
use moonpool_core::metrics::{MetricSample, MetricValue, MetricsSource, u64_to_f64_exact};

use crate::security::CredentialError;
use crate::stats::{Counters, RpcStats};
use crate::transport::RpcHandle;

type StatsFn = dyn Fn() -> Option<RpcStats> + Send + Sync;

/// A runtime's counters and gauges as a [`MetricsSource`].
///
/// Built from a live runtime's handle; keeps answering after the runtime is
/// gone (counters keep their final values, gauges read zero).
pub struct RpcMetrics {
    counters: Arc<Counters>,
    stats: Box<StatsFn>,
}

impl std::fmt::Debug for RpcMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcMetrics").finish_non_exhaustive()
    }
}

impl RpcMetrics {
    /// The metrics of the runtime behind `rpc`, or `None` if it is already
    /// gone.
    #[must_use]
    pub fn new<P: Providers>(rpc: &RpcHandle<P>) -> Option<Self> {
        let counters = Arc::clone(rpc.upgrade()?.counters());
        let rpc = rpc.clone();
        Some(Self {
            counters,
            stats: Box::new(move || rpc.stats()),
        })
    }

    /// The current snapshot (gauges zero once the runtime is gone).
    #[must_use]
    pub fn snapshot(&self) -> RpcStats {
        (self.stats)().unwrap_or_else(|| {
            let mut stats = self.counters.snapshot(0, 0, 0);
            stats.connections = 0;
            stats.streams_producing = 0;
            stats
        })
    }
}

fn float(value: u64) -> f64 {
    u64_to_f64_exact(value)
}

fn count(value: usize) -> f64 {
    u64_to_f64_exact(u64::try_from(value).unwrap_or(u64::MAX))
}

impl MetricsSource for RpcMetrics {
    fn collect(&self) -> Vec<MetricSample> {
        let stats = self.snapshot();
        let counter = |name: &str, value: u64| {
            MetricSample::new(
                format!("moonpool_rpc_{name}_total"),
                Vec::new(),
                MetricValue::Counter(float(value)),
            )
        };
        let gauge = |name: &str, value: f64| {
            MetricSample::new(
                format!("moonpool_rpc_{name}"),
                Vec::new(),
                MetricValue::Gauge(value),
            )
        };
        let mut samples = vec![
            counter("calls_started", stats.calls_started),
            counter("reliable_calls_started", stats.reliable_calls_started),
            counter("retransmissions", stats.retransmissions),
            counter("calls_abandoned", stats.calls_abandoned),
            counter("calls_failed_fast", stats.calls_failed_fast),
            counter("calls_ended_by_shutdown", stats.calls_ended_by_shutdown),
            counter("requests_admitted", stats.requests_admitted),
            counter("requests_rejected", stats.requests_rejected),
            counter("requests_authenticated", stats.requests_authenticated),
            counter("shutdown_refusals", stats.shutdown_refusals),
            counter("overload_refusals", stats.overload_refusals),
            counter("replies_sent", stats.replies_sent),
            counter("replies_dropped", stats.replies_dropped),
            counter("late_replies", stats.late_replies),
            counter("broken_promises", stats.broken_promises),
            counter("one_way_sent", stats.one_way_sent),
            counter("one_way_received", stats.one_way_received),
            counter("credentials_attached", stats.credentials_attached),
            counter("connections_opened", stats.connections_opened),
            counter("connections_rejected", stats.connections_rejected),
            counter(
                "connections_refused_by_policy",
                stats.connections_refused_by_policy,
            ),
            counter("accept_errors", stats.accept_errors),
            counter("dials", stats.dials),
            counter("protocol_violations", stats.protocol_violations),
            counter("checksum_failures", stats.checksum_failures),
            counter("version_rejections", stats.version_rejections),
            counter("ping_timeouts", stats.ping_timeouts),
            counter("idle_closes", stats.idle_closes),
            counter("streams_opened", stats.streams_opened),
            counter("streams_admitted", stats.streams_admitted),
            counter("stream_items_sent", stats.stream_items_sent),
            counter("streams_ended", stats.streams_ended),
            counter("streams_cancelled", stats.streams_cancelled),
            counter("streams_disconnected", stats.streams_disconnected),
            gauge("endpoints", count(stats.endpoints)),
            gauge("pending_calls", count(stats.pending_calls)),
            gauge("retained_calls", count(stats.retained_calls)),
            gauge("peers", count(stats.peers)),
            gauge("connections", count(stats.connections)),
            gauge("inflight_requests", count(stats.inflight_requests)),
            gauge("streams_consuming", count(stats.streams_consuming)),
            gauge("streams_producing", count(stats.streams_producing)),
            gauge("queued_bytes", float(stats.queued_bytes)),
            gauge("stream_buffered_bytes", float(stats.stream_buffered_bytes)),
        ];
        let denied = |reason: &str, value: u64| {
            MetricSample::new(
                "moonpool_rpc_requests_denied_total",
                vec![("reason".to_string(), reason.to_string())],
                MetricValue::Counter(float(value)),
            )
        };
        samples.extend(
            CredentialError::all()
                .iter()
                .map(|reason| denied(reason.name(), self.counters.denials(*reason))),
        );
        samples.push(denied(
            "permission_denied",
            stats.requests_permission_denied,
        ));
        samples.sort_by_key(MetricSample::sort_key);
        samples
    }
}
