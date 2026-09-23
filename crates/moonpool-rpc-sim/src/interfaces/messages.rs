//! The interfaces campaign's messages and methods. Every identifier is an
//! explicit constant; the application identities (participant, boot,
//! configuration) are plain fields, never RPC identity.

use moonpool_rpc::{InterfaceRef, MethodId, RpcMethod, SchemaVersion, WellKnownId};

/// A participant's directory: its current base interface.
pub const DIRECTORY_ID: WellKnownId = WellKnownId::new(1);
/// A participant's recruiter: a fresh role instance per configuration.
pub const RECRUITER_ID: WellKnownId = WellKnownId::new(2);
/// A participant's dismissal endpoint: drop a recruited instance.
pub const DISMISS_ID: WellKnownId = WellKnownId::new(3);
/// The third participant's adopter: call recruited members.
pub const ADOPTER_ID: WellKnownId = WellKnownId::new(4);

/// The role every participant serves, generated from a trait on top of
/// the manual group API.
#[moonpool_rpc::service(id = 0x0215_0001, version = 1)]
pub trait Role {
    /// Report who is serving; the request says who the caller expects.
    #[method(id = 1, schema = 1)]
    async fn status(&self, request: Probe) -> Answer;
    /// Record a one-way note; never answered.
    #[method(id = 2, schema = 1)]
    async fn note(&self, request: Probe);
}

/// A call to a role instance, carrying who the caller expects to reach: the
/// identities of the publication its reference came from.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Probe {
    /// Workload-generated id, unique per run.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The participant the reference was published by (its IP).
    #[prost(string, tag = "2")]
    pub participant: String,
    /// The boot that published it.
    #[prost(uint64, tag = "3")]
    pub boot: u64,
    /// The configuration it serves (0: the base instance).
    #[prost(uint64, tag = "4")]
    pub configuration: u64,
}

/// Who served a probe.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Answer {
    /// The probe id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The serving participant (its IP).
    #[prost(string, tag = "2")]
    pub participant: String,
    /// The serving boot.
    #[prost(uint64, tag = "3")]
    pub boot: u64,
    /// The serving instance's configuration.
    #[prost(uint64, tag = "4")]
    pub configuration: u64,
}

/// An interface a participant boot published, with the application's own
/// identities beside the RPC routing data.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Publication {
    /// The participant (its IP).
    #[prost(string, tag = "1")]
    pub participant: String,
    /// The boot that published it.
    #[prost(uint64, tag = "2")]
    pub boot: u64,
    /// The configuration it serves (0: the base instance).
    #[prost(uint64, tag = "3")]
    pub configuration: u64,
    /// The fresh interface.
    #[prost(message, optional, tag = "4")]
    pub role: Option<InterfaceRef<RoleInterface>>,
}

/// An empty request.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Lookup {}

/// Recruit (or dismiss) an instance for a configuration.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Recruit {
    /// The configuration.
    #[prost(uint64, tag = "1")]
    pub configuration: u64,
}

/// Whether a dismissal removed an instance.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Dismissed {
    /// An instance for the configuration existed and was dropped.
    #[prost(bool, tag = "1")]
    pub removed: bool,
}

/// Hand the third participant recruited members to call.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Adopt {
    /// The configuration they were recruited into.
    #[prost(uint64, tag = "1")]
    pub configuration: u64,
    /// The members, as their participants published them.
    #[prost(message, repeated, tag = "2")]
    pub members: Vec<Publication>,
    /// One workload-generated probe id per member.
    #[prost(uint64, repeated, tag = "3")]
    pub ids: Vec<u64>,
}

/// What the third participant saw, one entry per member.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Adopted {
    /// Answers from the members that replied.
    #[prost(message, repeated, tag = "1")]
    pub answers: Vec<Answer>,
    /// `id:reason/execution` for the members that did not.
    #[prost(string, repeated, tag = "2")]
    pub failures: Vec<String>,
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
    /// A participant's well-known directory.
    Directory, Lookup, Publication, 0x0215_0101, "interfaces.directory"
);
method!(
    /// A participant's well-known recruiter.
    Recruiter, Recruit, Publication, 0x0215_0102, "interfaces.recruit"
);
method!(
    /// A participant's well-known dismissal endpoint.
    Dismiss, Recruit, Dismissed, 0x0215_0103, "interfaces.dismiss"
);
method!(
    /// The third participant's well-known adopter.
    Adopter, Adopt, Adopted, 0x0215_0104, "interfaces.adopt"
);
