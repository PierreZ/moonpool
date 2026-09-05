//! Events handled by the simulated network engine.

use std::time::Duration;

use super::ConnectionId;

/// Identifier for an in-flight network operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetworkOperationId(pub(crate) u64);

/// A targeted network transition or delivery.
///
/// Events carry identities and deadlines only, never payload: the bytes an
/// event lands live in the engine's per-connection in-flight queue, where a
/// fault injected after scheduling can still reach them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkEvent {
    /// Wake-up feed used to evaluate network maintenance at the current time.
    Maintenance,
    /// Complete a delayed network operation.
    OperationReady {
        /// Operation whose delay elapsed.
        operation_id: NetworkOperationId,
    },
    /// Clear a write clog if this remains its active deadline.
    ClogClear {
        /// Connection whose clog may be cleared.
        connection_id: ConnectionId,
        /// Deadline of the clog that created this event.
        expected_deadline: Duration,
    },
    /// Clear a read clog if this remains its active deadline.
    ReadClogClear {
        /// Connection whose clog may be cleared.
        connection_id: ConnectionId,
        /// Deadline of the clog that created this event.
        expected_deadline: Duration,
    },
    /// Clear directed pair partitions whose deadlines have expired.
    PartitionRestore {
        /// Deadline of the partitions that created this event.
        expected_deadline: Duration,
    },
    /// Clear send partitions whose deadlines have expired.
    SendPartitionClear {
        /// Deadline of the send partition that created this event.
        expected_deadline: Duration,
    },
    /// Clear receive partitions whose deadlines have expired.
    RecvPartitionClear {
        /// Deadline of the receive partition that created this event.
        expected_deadline: Duration,
    },
    /// An in-flight item (data chunk or FIN) reached its delivery time.
    ///
    /// The item itself lives in the sender's in-flight queue, so a fault
    /// injected after this event was scheduled still decides its fate: the
    /// event is a wake-up, not the payload. It is a no-op when the item was
    /// already delivered or re-timed by a partition.
    Delivery {
        /// Sending connection whose flight holds the item.
        connection_id: ConnectionId,
        /// Sequence number of the item within that flight.
        seq: u64,
    },
    /// Process the next buffered send.
    ProcessSendBuffer {
        /// Connection whose next buffered write should run.
        connection_id: ConnectionId,
    },
}

impl NetworkEvent {
    /// Returns whether the event only maintains network infrastructure.
    #[must_use]
    pub fn is_infrastructure(&self) -> bool {
        matches!(
            self,
            Self::PartitionRestore { .. }
                | Self::SendPartitionClear { .. }
                | Self::RecvPartitionClear { .. }
        )
    }
}
