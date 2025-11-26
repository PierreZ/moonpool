//! FDB-compatible messaging types and wire format.
//!
//! This module provides the foundation for endpoint-based messaging:
//! - [`UID`]: 128-bit unique identifier for endpoints
//! - [`Endpoint`]: Network address + token for direct addressing
//! - [`WellKnownToken`]: System service tokens (Ping, EndpointNotFound)
//! - Wire format with CRC32C checksum

mod types;
mod well_known;
mod wire;

pub use types::{Endpoint, NetworkAddress, NetworkAddressParseError, UID, flags};
pub use well_known::{WELL_KNOWN_RESERVED_COUNT, WellKnownToken};
pub use wire::{
    HEADER_SIZE, MAX_PAYLOAD_SIZE, PacketHeader, WireError, deserialize_packet, serialize_packet,
    try_deserialize_packet,
};
