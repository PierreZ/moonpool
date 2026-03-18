//! Simulation workloads for chaos testing transport layer.

pub mod e2e;
pub mod messaging;

use std::collections::HashMap;

use async_trait::async_trait;
use moonpool_sim::{
    SIM_FAULT_TIMELINE, SimContext, SimFaultEvent, SimulationResult, Workload, assert_always,
    assert_sometimes,
};

/// Per-message event emitted to the `"msg:sent"` and `"msg:recv"` timelines.
///
/// Enables temporal correlation between message delivery and fault events.
#[derive(Debug, Clone)]
pub struct MsgEvent {
    /// Message sequence ID.
    pub seq_id: u64,
    /// Whether this message was sent reliably.
    pub reliable: bool,
    /// Sender identifier (disambiguates seq_ids across multiple clients).
    pub sender_id: String,
}

/// Passive workload that validates transport timeline consistency at quiescence.
///
/// Reads `"sim:faults"`, `"msg:sent"`, and `"msg:recv"` timelines after all
/// workloads complete and checks transport-specific temporal properties:
/// - **Causal delivery**: no message received before it was sent
/// - **No phantom receives**: every received message was actually sent
/// - **Fault coverage**: chaos injected faults AND transport recovered
pub struct TransportTimelineCheck;

#[async_trait(?Send)]
impl Workload for TransportTimelineCheck {
    fn name(&self) -> &str {
        "transport_timeline_check"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.shutdown().cancelled().await;
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state();

        // --- Fault timeline ---
        let fault_count = state
            .timeline::<SimFaultEvent>(SIM_FAULT_TIMELINE)
            .map_or(0, |tl| tl.len());

        assert_sometimes!(fault_count > 0, "Faults should sometimes be injected");

        // --- Build sent index: (sender_id, seq_id, reliable) → time_ms ---
        // Key by all three fields to disambiguate across clients and queue types.
        let mut sent_time: HashMap<(String, u64, bool), u64> = HashMap::new();
        let mut reliable_sent_count = 0usize;
        let mut unreliable_sent_count = 0usize;

        if let Some(sent_tl) = state.timeline::<MsgEvent>("msg:sent") {
            for entry in sent_tl.all().iter() {
                sent_time.insert(
                    (
                        entry.event.sender_id.clone(),
                        entry.event.seq_id,
                        entry.event.reliable,
                    ),
                    entry.time_ms,
                );
                if entry.event.reliable {
                    reliable_sent_count += 1;
                } else {
                    unreliable_sent_count += 1;
                }
            }
        }

        // --- Check received messages against sent ---
        let mut recv_count = 0usize;
        let mut reliable_recv_count = 0usize;
        let mut unreliable_recv_count = 0usize;
        let mut latency_sum = 0u64;
        let mut latency_min = u64::MAX;
        let mut latency_max = 0u64;
        let mut latency_samples = 0u64;

        if let Some(recv_tl) = state.timeline::<MsgEvent>("msg:recv") {
            for entry in recv_tl.all().iter() {
                let key = (
                    entry.event.sender_id.clone(),
                    entry.event.seq_id,
                    entry.event.reliable,
                );
                recv_count += 1;

                // No phantom receives: every received message must have been sent
                assert_always!(
                    sent_time.contains_key(&key),
                    format!(
                        "Phantom receive: {} msg seq={} from {} received at {}ms but never sent",
                        if entry.event.reliable {
                            "reliable"
                        } else {
                            "unreliable"
                        },
                        entry.event.seq_id,
                        entry.event.sender_id,
                        entry.time_ms
                    )
                );

                // Causal delivery: receive time >= send time
                if let Some(&send_time) = sent_time.get(&key) {
                    assert_always!(
                        entry.time_ms >= send_time,
                        format!(
                            "Causal violation: msg seq={} sent at {}ms but received at {}ms",
                            entry.event.seq_id, send_time, entry.time_ms
                        )
                    );

                    let latency = entry.time_ms - send_time;
                    latency_sum += latency;
                    latency_min = latency_min.min(latency);
                    latency_max = latency_max.max(latency);
                    latency_samples += 1;
                }

                if entry.event.reliable {
                    reliable_recv_count += 1;
                } else {
                    unreliable_recv_count += 1;
                }
            }
        }

        // Fault-aware coverage
        assert_sometimes!(
            fault_count > 0 && recv_count > 0,
            "Transport should recover: messages delivered despite faults"
        );
        assert_sometimes!(
            unreliable_recv_count < unreliable_sent_count,
            "Chaos should sometimes drop unreliable messages"
        );

        // Summary
        let avg_latency = if latency_samples > 0 {
            latency_sum / latency_samples
        } else {
            0
        };
        tracing::info!(
            "Timeline: {} faults, {} sent ({} reliable, {} unreliable), {} received ({} reliable, {} unreliable), latency min/avg/max = {}/{}/{}ms",
            fault_count,
            sent_time.len(),
            reliable_sent_count,
            unreliable_sent_count,
            recv_count,
            reliable_recv_count,
            unreliable_recv_count,
            if latency_min == u64::MAX {
                0
            } else {
                latency_min
            },
            avg_latency,
            latency_max,
        );

        Ok(())
    }
}
