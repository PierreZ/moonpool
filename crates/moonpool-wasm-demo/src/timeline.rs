use std::{
    cell::Cell,
    collections::HashMap,
    sync::{Arc, Mutex},
};

use moonpool_sim::{Invariant, SIM_FAULT_EVENT_NAME, TraceQuery};

use crate::{Outcome, RunResult, Shot};

const NODE_A: u8 = 0;
const NODE_B: u8 = 1;
const MIN_DROP_SPAN_MS: u64 = 50;

const EV_ISSUED: &str = "client_issued";
const EV_ACKED: &str = "client_acknowledged";
const EV_FAILED: &str = "client_failed";
const EV_AMBIGUOUS: &str = "client_ambiguous";

#[derive(Default)]
struct RecorderData {
    issued: Vec<(u64, u64)>,
    acked: Vec<(u64, u64)>,
    failed: Vec<(u64, u64)>,
    fault_count: u32,
    last_fault_ms: u64,
}

pub(crate) struct RecorderHandle(Arc<Mutex<RecorderData>>);

fn collect_sequences(
    query: &dyn TraceQuery,
    name: &str,
    cursor: &Cell<usize>,
) -> impl Iterator<Item = (u64, u64)> {
    query
        .since(name, cursor)
        .into_iter()
        .filter_map(|event| event.u64("seq_id").map(|seq| (seq, event.time_ms)))
}

#[derive(Default)]
struct RecorderCursors {
    issued: Cell<usize>,
    acked: Cell<usize>,
    failed: Cell<usize>,
    ambiguous: Cell<usize>,
    faults: Cell<usize>,
}

pub(crate) struct TimelineRecorder {
    data: Arc<Mutex<RecorderData>>,
    cursors: RecorderCursors,
}

impl Invariant for TimelineRecorder {
    fn name(&self) -> &'static str {
        "timeline_recorder"
    }

    fn observe(&self, query: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut data = self
            .data
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        data.issued
            .extend(collect_sequences(query, EV_ISSUED, &self.cursors.issued));
        data.acked
            .extend(collect_sequences(query, EV_ACKED, &self.cursors.acked));
        data.failed
            .extend(collect_sequences(query, EV_FAILED, &self.cursors.failed));
        data.failed.extend(collect_sequences(
            query,
            EV_AMBIGUOUS,
            &self.cursors.ambiguous,
        ));
        for event in query.since(SIM_FAULT_EVENT_NAME, &self.cursors.faults) {
            data.fault_count = data.fault_count.saturating_add(1);
            data.last_fault_ms = data.last_fault_ms.max(event.time_ms);
        }
    }

    fn reset(&mut self) {
        self.cursors = RecorderCursors::default();
        *self
            .data
            .lock()
            .expect("Mutex poisoned: prior task panicked") = RecorderData::default();
    }
}

pub(crate) fn recorder() -> (RecorderHandle, TimelineRecorder) {
    let data = Arc::new(Mutex::new(RecorderData::default()));
    let invariant = TimelineRecorder {
        data: data.clone(),
        cursors: RecorderCursors::default(),
    };
    (RecorderHandle(data), invariant)
}

pub(crate) fn finish(seed: u64, handle: &RecorderHandle) -> RunResult {
    let data = handle
        .0
        .lock()
        .expect("Mutex poisoned: prior task panicked");
    if data.issued.is_empty() {
        return RunResult::empty(seed);
    }

    let acknowledgements: HashMap<_, _> = data.acked.iter().copied().collect();
    let failures: HashMap<_, _> = data.failed.iter().copied().collect();
    let mut issued = data.issued.clone();
    issued.sort_by_key(|&(_, time)| time);

    let mut shots = Vec::with_capacity(issued.len().saturating_mul(2));
    let mut delivered = 0_u32;
    let mut dropped = 0_u32;
    let mut longest_rtt_ms = 0_u64;

    for (sequence, issue_ms) in issued.iter().copied() {
        if let Some(&acknowledged_ms) = acknowledgements.get(&sequence) {
            delivered += 1;
            let rtt = acknowledged_ms.saturating_sub(issue_ms);
            longest_rtt_ms = longest_rtt_ms.max(rtt);
            let midpoint_ms = issue_ms.saturating_add(rtt / 2);
            shots.extend([
                Shot {
                    seq: sequence,
                    from: NODE_A,
                    to: NODE_B,
                    depart_ms: issue_ms,
                    arrive_ms: midpoint_ms,
                    latency_ms: midpoint_ms.saturating_sub(issue_ms),
                    outcome: Outcome::Delivered,
                },
                Shot {
                    seq: sequence,
                    from: NODE_B,
                    to: NODE_A,
                    depart_ms: midpoint_ms,
                    arrive_ms: acknowledged_ms,
                    latency_ms: acknowledged_ms.saturating_sub(midpoint_ms),
                    outcome: Outcome::Delivered,
                },
            ]);
        } else {
            dropped += 1;
            let failed_ms = failures.get(&sequence).copied().unwrap_or(issue_ms);
            let span = failed_ms.saturating_sub(issue_ms).max(MIN_DROP_SPAN_MS);
            shots.push(Shot {
                seq: sequence,
                from: NODE_A,
                to: NODE_B,
                depart_ms: issue_ms,
                arrive_ms: issue_ms.saturating_add(span),
                latency_ms: span,
                outcome: Outcome::Dropped,
            });
        }
    }

    let sim_duration_ms = shots
        .iter()
        .map(|shot| shot.arrive_ms)
        .max()
        .unwrap_or_default()
        .max(data.last_fault_ms);

    RunResult {
        seed,
        requests: u32::try_from(issued.len()).unwrap_or(u32::MAX),
        shots,
        delivered,
        dropped,
        faults: data.fault_count,
        longest_rtt_ms,
        sim_duration_ms,
    }
}
