//! The delivery campaign's messages and methods. Every identifier is an
//! explicit constant.

use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion, WellKnownId};

/// The well-known id of the server's directory.
pub const DIRECTORY_ID: WellKnownId = WellKnownId::new(1);

/// What the server's handler does with a [`Job`], as `Job::mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    /// Reply at once.
    Reply = 0,
    /// Reply after `delay_ms` of provider time.
    Slow = 1,
    /// Finish with an explicit no-reply.
    NeverReply = 2,
    /// Drop the reply handle: a broken promise.
    Broken = 3,
    /// Ask the fault script to cut the caller off, then reply into the cut.
    LoseReply = 4,
    /// Ask the fault script to crash the server; never reply.
    Crash = 5,
    /// Ask the fault script to crash the server, and reply `delay_ms`
    /// later: the reply and the disconnect race.
    ReplyThenCrash = 6,
}

impl Mode {
    /// Decode the wire value.
    #[must_use]
    pub fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0 => Self::Reply,
            1 => Self::Slow,
            2 => Self::NeverReply,
            3 => Self::Broken,
            4 => Self::LoseReply,
            5 => Self::Crash,
            6 => Self::ReplyThenCrash,
            _ => return None,
        })
    }
}

/// One unit of work, identified by a workload-generated id.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Job {
    /// Workload-generated id, unique per run.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// A [`Mode`] value.
    #[prost(uint32, tag = "2")]
    pub mode: u32,
    /// Delay for [`Mode::Slow`].
    #[prost(uint32, tag = "3")]
    pub delay_ms: u32,
}

/// The handler's answer.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Done {
    /// The job id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// How many times a handler had received this id when it answered.
    #[prost(uint32, tag = "2")]
    pub receipts: u32,
}

/// An empty request.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Lookup {}

/// The directory's answer: the current incarnation's dynamic reference.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Listing {
    /// The server boot that registered it.
    #[prost(uint64, tag = "1")]
    pub boot: u64,
    /// The `Execute` reference bytes.
    #[prost(bytes = "vec", tag = "2")]
    pub execute: Vec<u8>,
}

/// A peer-to-peer greeting.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Greeting {
    /// Sender-local sequence number.
    #[prost(uint64, tag = "1")]
    pub seq: u64,
}

macro_rules! method {
    ($(#[$doc:meta])* $name:ident, $request:ty, $reply:ty, $method:expr, $label:expr) => {
        $(#[$doc])*
        pub struct $name;

        impl RpcMethod for $name {
            type Request = $request;
            type Reply = $reply;
            const METHOD: MethodId = MethodId::new($method);
            const SCHEMA: SchemaVersion = SchemaVersion::new(1);
            const NAME: &'static str = $label;
        }
    };
}

method!(
    /// The server's dynamic work endpoint.
    Execute, Job, Done, 0x0300, "delivery.execute"
);
method!(
    /// The server's well-known directory.
    Directory, Lookup, Listing, 0x0301, "delivery.directory"
);
method!(
    /// Each peer's greeting endpoint.
    Greet, Greeting, Greeting, 0x0302, "delivery.greet"
);
