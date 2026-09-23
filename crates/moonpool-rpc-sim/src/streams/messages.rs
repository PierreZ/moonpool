//! The streams campaign's messages and methods. Every identifier is an
//! explicit constant.

use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion, WellKnownId};

/// The well-known id of the producer's directory.
pub const DIRECTORY_ID: WellKnownId = WellKnownId::new(1);

/// The application code a producer fails its streams with when its
/// process shuts down gracefully.
pub const SHUTDOWN_CODE: u64 = 0x5d;

/// How a producer ends a stream, as `Scan::end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum End {
    /// Send `count` items, then end normally.
    Finish = 0,
    /// Send `count` items, then fail with `Scan::code`.
    Fail = 1,
    /// Send `count` items, then drop the producer (a broken promise).
    Drop = 2,
    /// Keep sending until the stream is closed under it.
    Endless = 3,
}

impl End {
    /// Decode the wire value.
    #[must_use]
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Fail,
            2 => Self::Drop,
            3 => Self::Endless,
            _ => Self::Finish,
        }
    }
}

/// A stream request, identified by a workload-generated id.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Scan {
    /// Workload-generated stream id, unique per run.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Items to send (ignored by [`End::Endless`]).
    #[prost(uint64, tag = "2")]
    pub count: u64,
    /// Payload bytes of each item.
    #[prost(uint32, tag = "3")]
    pub item_bytes: u32,
    /// Pause before the first item, in milliseconds.
    #[prost(uint32, tag = "4")]
    pub first_delay_ms: u32,
    /// Pause before every later item, in milliseconds.
    #[prost(uint32, tag = "5")]
    pub delay_ms: u32,
    /// An [`End`] value.
    #[prost(uint32, tag = "6")]
    pub end: u32,
    /// The failure code for [`End::Fail`].
    #[prost(uint64, tag = "7")]
    pub code: u64,
}

/// One stream item.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Chunk {
    /// The stream id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The item's position, from zero.
    #[prost(uint64, tag = "2")]
    pub seq: u64,
    /// The producer boot that sent it.
    #[prost(uint64, tag = "3")]
    pub boot: u64,
    /// Filler.
    #[prost(bytes = "vec", tag = "4")]
    pub data: Vec<u8>,
}

/// A unary probe, identified by a workload-generated id.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Probe {
    /// Workload-generated id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// How long the handler holds the reply, in milliseconds.
    #[prost(uint32, tag = "2")]
    pub hold_ms: u32,
}

/// An empty request.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Lookup {}

/// The directory's answer: the current boot's dynamic references.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Listing {
    /// The producer boot that registered them.
    #[prost(uint64, tag = "1")]
    pub boot: u64,
    /// The `ScanItems` reference bytes.
    #[prost(bytes = "vec", tag = "2")]
    pub scan: Vec<u8>,
    /// The `Ping` reference bytes.
    #[prost(bytes = "vec", tag = "3")]
    pub probe: Vec<u8>,
}

/// The producer's streaming endpoint.
pub struct ScanItems;

impl RpcMethod for ScanItems {
    type Request = Scan;
    type Reply = Chunk;
    const METHOD: MethodId = MethodId::new(0x0400);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "streams.scan";
    const STREAMING: bool = true;
}

/// The producer's unary probe.
pub struct Ping;

impl RpcMethod for Ping {
    type Request = Probe;
    type Reply = Probe;
    const METHOD: MethodId = MethodId::new(0x0401);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "streams.ping";
}

/// The producer's well-known directory.
pub struct Directory;

impl RpcMethod for Directory {
    type Request = Lookup;
    type Reply = Listing;
    const METHOD: MethodId = MethodId::new(0x0402);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "streams.directory";
}
