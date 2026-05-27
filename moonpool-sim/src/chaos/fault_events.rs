//! Simulator-emitted fault events for the fault trail.
//!
//! When faults are injected (network partitions, process reboots, storage corruption, etc.),
//! the simulator automatically emits [`SimFaultEvent`]s to the [`SIM_FAULT_TRAIL`] trail.
//! Invariants can read these to correlate application behavior with infrastructure faults.
//!
//! # Usage
//!
//! ```ignore
//! use std::cell::Cell;
//! use moonpool_sim::{Invariant, SimFaultEvent, SIM_FAULT_TRAIL, TrailQuery, TrailQueryExt};
//!
//! struct FaultCounter { cursor: Cell<usize> }
//!
//! impl Invariant for FaultCounter {
//!     fn name(&self) -> &str { "fault_counter" }
//!     fn observe(&self, q: &dyn TrailQuery, _t: u64) {
//!         for entry in q.since::<SimFaultEvent>(SIM_FAULT_TRAIL, &self.cursor) {
//!             if let SimFaultEvent::ProcessForceKill { ip } = &entry.event {
//!                 // ...
//!             }
//!         }
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use valuable::Valuable;

/// Well-known trail name for simulator-emitted fault events.
pub const SIM_FAULT_TRAIL: &str = "sim:faults";

/// Fault events automatically emitted by the simulator.
///
/// Invariants read these via the [`crate::TrailQuery`] / [`crate::TrailQueryExt`]
/// API: `q.since::<SimFaultEvent>(SIM_FAULT_TRAIL, &cursor)`.
#[derive(Debug, Clone, Valuable, Serialize, Deserialize)]
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
        kind: String,
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
