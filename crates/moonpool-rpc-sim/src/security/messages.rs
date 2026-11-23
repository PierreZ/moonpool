//! The campaign's messages and methods; every identifier is an explicit
//! constant.

use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion};

/// A request: the ledger id the caller issued, and how long to hold it.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Probe {
    /// Ledger id, unique per run.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// How long the handler holds it before answering, in milliseconds.
    #[prost(uint32, tag = "2")]
    pub hold_ms: u32,
    /// The request's carrier mark ([`crate::carriers::mark`] of `id`):
    /// lets the oracle count, from the network, the connections that
    /// carried it.
    #[prost(bytes = "vec", tag = "3")]
    pub mark: Vec<u8>,
}

impl Probe {
    /// A marked request.
    #[must_use]
    pub fn new(id: u64, hold_ms: u32) -> Self {
        Self {
            id,
            hold_ms,
            mark: crate::carriers::mark(id),
        }
    }
}

/// A reply, or a stream item.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Answer {
    /// The request's id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The subject the server's verification established (empty when
    /// anonymous).
    #[prost(string, tag = "2")]
    pub subject: String,
}

/// A private unary endpoint: callable with a verified credential only.
pub struct PrivateEcho;
impl RpcMethod for PrivateEcho {
    type Request = Probe;
    type Reply = Answer;
    const METHOD: MethodId = MethodId::new(0x5ec0_0001);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "security.private_echo";
}

/// A public unary endpoint: anyone, but a presented credential must
/// verify.
pub struct PublicEcho;
impl RpcMethod for PublicEcho {
    type Request = Probe;
    type Reply = Answer;
    const METHOD: MethodId = MethodId::new(0x5ec0_0002);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "security.public_echo";
}

/// A private streaming endpoint: two items per admitted stream.
pub struct PrivateScan;
impl RpcMethod for PrivateScan {
    type Request = Probe;
    type Reply = Answer;
    const METHOD: MethodId = MethodId::new(0x5ec0_0003);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "security.private_scan";
    const STREAMING: bool = true;
}

/// A private endpoint called one-way.
pub struct PrivateNote;
impl RpcMethod for PrivateNote {
    type Request = Probe;
    type Reply = Answer;
    const METHOD: MethodId = MethodId::new(0x5ec0_0004);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "security.private_note";
}

/// Items a [`PrivateScan`] stream carries.
pub const SCAN_ITEMS: u64 = 2;
