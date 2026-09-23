//! The qualification campaign's own messages and methods. Every identifier
//! is an explicit constant; the application identities (server, boot,
//! configuration) are plain fields, never RPC identity. Streams reuse the
//! streams campaign's `ScanItems`, credentials the security campaign's
//! `PrivateEcho`.

use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion, WellKnownId};

/// A server's (or the third participant's) directory.
pub const DIRECTORY_ID: WellKnownId = WellKnownId::new(1);
/// A server's recruiter: a fresh `Work` instance per configuration.
pub const RECRUITER_ID: WellKnownId = WellKnownId::new(2);
/// A server's forwarder: call a callback reference found in the request.
pub const FORWARDER_ID: WellKnownId = WellKnownId::new(3);
/// The third participant's adopter: call recruited members.
pub const ADOPTER_ID: WellKnownId = WellKnownId::new(4);

/// A unit of work, carrying who the caller expects to run it: the
/// identities of the publication its reference came from (empty server:
/// any, for a balanced call over a set).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Job {
    /// Workload-generated id, unique per run.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The instance's process (its IP), or empty for "any member of the
    /// set".
    #[prost(string, tag = "2")]
    pub server: String,
    /// The boot that published the reference.
    #[prost(uint64, tag = "3")]
    pub boot: u64,
    /// The configuration it serves (0: the base instance).
    #[prost(uint64, tag = "4")]
    pub configuration: u64,
    /// How long the handler holds the reply, in milliseconds.
    #[prost(uint32, tag = "5")]
    pub hold_ms: u32,
}

/// Who ran a job.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Done {
    /// The job id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The serving process (its IP).
    #[prost(string, tag = "2")]
    pub server: String,
    /// Its boot.
    #[prost(uint64, tag = "3")]
    pub boot: u64,
    /// The serving instance's configuration.
    #[prost(uint64, tag = "4")]
    pub configuration: u64,
}

/// One interface a boot published, with the application's identities
/// beside the routing data (reference bytes: stored and forwarded as
/// opaque data).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Publication {
    /// The publishing process (its IP).
    #[prost(string, tag = "1")]
    pub server: String,
    /// Its boot.
    #[prost(uint64, tag = "2")]
    pub boot: u64,
    /// The configuration (0: the base instance).
    #[prost(uint64, tag = "3")]
    pub configuration: u64,
    /// The `Work` (or `Callback`) reference.
    #[prost(bytes = "vec", tag = "4")]
    pub work: Vec<u8>,
    /// The `ScanItems` reference (base instances of servers only).
    #[prost(bytes = "vec", tag = "5")]
    pub scan: Vec<u8>,
    /// The credential-protected `PrivateEcho` reference (servers only).
    #[prost(bytes = "vec", tag = "6")]
    pub private: Vec<u8>,
}

/// An empty request.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Lookup {}

/// Recruit an instance for a configuration.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Recruit {
    /// The configuration.
    #[prost(uint64, tag = "1")]
    pub configuration: u64,
}

/// Hand the third participant recruited members to call.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Adopt {
    /// The members, as their servers published them.
    #[prost(message, repeated, tag = "1")]
    pub members: Vec<Publication>,
    /// One workload-generated job id per member.
    #[prost(uint64, repeated, tag = "2")]
    pub ids: Vec<u64>,
}

/// What a relay (the third participant, or a forwarding server) saw.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Relayed {
    /// Answers of the calls that replied.
    #[prost(message, repeated, tag = "1")]
    pub answers: Vec<Done>,
    /// `id:reason/execution` of the calls that did not.
    #[prost(string, repeated, tag = "2")]
    pub failures: Vec<String>,
}

/// Ask a server to call a callback: a `Callback` reference that arrived as
/// data, owned by a third process.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Forward {
    /// The callback's publication.
    #[prost(message, optional, tag = "1")]
    pub callback: Option<Publication>,
    /// The job id to call it with.
    #[prost(uint64, tag = "2")]
    pub id: u64,
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
    /// A work instance: unary, one-way, reliable and balanced calls.
    Work, Job, Done, 0x0219_0001, "qualification.work"
);
method!(
    /// A callback endpoint the third participant registers per boot.
    Callback, Job, Done, 0x0219_0002, "qualification.callback"
);
method!(
    /// A directory: the boot's current publication.
    Directory, Lookup, Publication, 0x0219_0003, "qualification.directory"
);
method!(
    /// A server's recruiter.
    Recruiter, Recruit, Publication, 0x0219_0004, "qualification.recruit"
);
method!(
    /// A server's forwarder.
    Forwarder, Forward, Relayed, 0x0219_0005, "qualification.forward"
);
method!(
    /// The third participant's adopter.
    Adopter, Adopt, Relayed, 0x0219_0006, "qualification.adopt"
);
