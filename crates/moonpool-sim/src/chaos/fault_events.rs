//! Simulator-emitted fault events for the timeline.
//!
//! When faults are injected (network partitions, process reboots, storage corruption, etc.),
//! the simulation engine records [`SimFaultEvent`]s and the runner drains them into the
//! captured timeline under the [`SIM_FAULT_EVENT_NAME`] event name, with a `kind` field
//! identifying the fault variant and the payload flattened into fields.
//! Invariants can read these to correlate application behavior with infrastructure faults.
//!
//! # Usage
//!
//! ```ignore
//! use std::cell::Cell;
//! use moonpool_sim::{Invariant, SIM_FAULT_EVENT_NAME, TraceQuery};
//!
//! struct FaultCounter { cursor: Cell<usize> }
//!
//! impl Invariant for FaultCounter {
//!     fn name(&self) -> &str { "fault_counter" }
//!     fn observe(&self, q: &dyn TraceQuery, _t: u64) {
//!         for e in q.since(SIM_FAULT_EVENT_NAME, &self.cursor) {
//!             if e.str("kind") == Some("process_force_kill") {
//!                 let ip = e.str("ip");
//!                 // ...
//!             }
//!         }
//!     }
//! }
//! ```

use std::collections::BTreeMap;

use serde::Serialize;

use crate::observability::FieldValue;

/// Well-known event name for simulator-emitted fault events.
pub const SIM_FAULT_EVENT_NAME: &str = "sim_fault";

/// Fault events automatically recorded by the simulator.
///
/// Invariants read these from the timeline via [`crate::TraceQuery`]:
/// `q.since(SIM_FAULT_EVENT_NAME, &cursor)`, matching on the `kind` field
/// (see [`SimFaultEvent::kind`]).
#[derive(Debug, Clone, Serialize)]
pub enum SimFaultEvent {
    // -- Process lifecycle --
    /// Process graceful shutdown initiated.
    ProcessGracefulShutdown {
        /// IP address of the process.
        ip: String,
        /// Grace period before force-kill, in milliseconds.
        grace_period_ms: u64,
    },
    /// Process force-killed.
    ProcessForceKill {
        /// IP address of the process.
        ip: String,
    },
    /// Process restarted after recovery delay.
    ProcessRestart {
        /// IP address of the process.
        ip: String,
    },

    // -- Network --
    /// Bidirectional network partition created between two IPs.
    PartitionCreated {
        /// Source IP.
        from: String,
        /// Destination IP.
        to: String,
    },
    /// Network partition healed between two IPs.
    PartitionHealed {
        /// Source IP.
        from: String,
        /// Destination IP.
        to: String,
    },
    /// Connection temporarily cut.
    ConnectionCut {
        /// The connection that was cut.
        connection_id: u64,
        /// Duration of the cut in milliseconds.
        duration_ms: u64,
    },
    /// Temporarily cut connection restored.
    CutRestored {
        /// The connection that was restored.
        connection_id: u64,
    },
    /// Half-open connection error triggered.
    HalfOpenError {
        /// The connection now returning errors.
        connection_id: u64,
    },
    /// Send partition created (blocks outgoing from an IP).
    SendPartitionCreated {
        /// The partitioned IP.
        ip: String,
    },
    /// Receive partition created (blocks incoming to an IP).
    RecvPartitionCreated {
        /// The partitioned IP.
        ip: String,
    },
    /// Connection randomly closed by chaos.
    RandomClose {
        /// The connection that was closed.
        connection_id: u64,
    },
    /// Peer crash simulated (half-open connection created).
    PeerCrash {
        /// The connection now in half-open state.
        connection_id: u64,
    },
    /// Bit flip corruption injected during data delivery.
    BitFlip {
        /// The connection carrying corrupted data.
        connection_id: u64,
        /// Number of bits flipped.
        flip_count: usize,
    },

    // -- Storage --
    /// Read fault injected (sector marked as faulted).
    StorageReadFault {
        /// IP of the process owning the file.
        ip: String,
        /// File identifier.
        file_id: u64,
    },
    /// Write fault injected (phantom, misdirected, or corruption).
    StorageWriteFault {
        /// IP of the process owning the file.
        ip: String,
        /// File identifier.
        file_id: u64,
        /// Kind of write fault: "phantom", "misdirected", or "corruption".
        /// (Named `write_kind` to avoid colliding with the timeline's
        /// variant-discriminator `kind` field.)
        write_kind: String,
    },
    /// Sync failure injected.
    StorageSyncFault {
        /// IP of the process owning the file.
        ip: String,
        /// File identifier.
        file_id: u64,
    },
    /// Storage crash simulated for a process.
    StorageCrash {
        /// IP of the crashed process.
        ip: String,
    },
    /// All storage wiped for a process (`CrashAndWipe` reboot).
    StorageWipe {
        /// IP of the wiped process.
        ip: String,
    },
}

impl SimFaultEvent {
    /// Stable `snake_case` identifier for this fault variant, stored in the
    /// timeline event's `kind` field.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ProcessGracefulShutdown { .. } => "process_graceful_shutdown",
            Self::ProcessForceKill { .. } => "process_force_kill",
            Self::ProcessRestart { .. } => "process_restart",
            Self::PartitionCreated { .. } => "partition_created",
            Self::PartitionHealed { .. } => "partition_healed",
            Self::ConnectionCut { .. } => "connection_cut",
            Self::CutRestored { .. } => "cut_restored",
            Self::HalfOpenError { .. } => "half_open_error",
            Self::SendPartitionCreated { .. } => "send_partition_created",
            Self::RecvPartitionCreated { .. } => "recv_partition_created",
            Self::RandomClose { .. } => "random_close",
            Self::PeerCrash { .. } => "peer_crash",
            Self::BitFlip { .. } => "bit_flip",
            Self::StorageReadFault { .. } => "storage_read_fault",
            Self::StorageWriteFault { .. } => "storage_write_fault",
            Self::StorageSyncFault { .. } => "storage_sync_fault",
            Self::StorageCrash { .. } => "storage_crash",
            Self::StorageWipe { .. } => "storage_wipe",
        }
    }

    /// Flatten this fault's payload into timeline fields.
    ///
    /// Serializes via serde's external tagging (`{"Variant": {fields}}`) and
    /// maps the inner object's scalars to [`FieldValue`]s. Returns an empty
    /// map if serialization fails (it cannot for these variants).
    pub(crate) fn to_fields(&self) -> BTreeMap<String, FieldValue> {
        let mut fields = BTreeMap::new();
        let Ok(serde_json::Value::Object(tagged)) = serde_json::to_value(self) else {
            return fields;
        };
        for payload in tagged.into_values() {
            let serde_json::Value::Object(entries) = payload else {
                continue;
            };
            for (key, value) in entries {
                let field = match value {
                    serde_json::Value::Bool(b) => FieldValue::Bool(b),
                    serde_json::Value::Number(n) => {
                        if let Some(u) = n.as_u64() {
                            FieldValue::U64(u)
                        } else if let Some(i) = n.as_i64() {
                            FieldValue::I64(i)
                        } else if let Some(f) = n.as_f64() {
                            FieldValue::F64(f)
                        } else {
                            continue;
                        }
                    }
                    serde_json::Value::String(s) => FieldValue::Str(s),
                    _ => continue,
                };
                fields.insert(key, field);
            }
        }
        fields
    }
}
