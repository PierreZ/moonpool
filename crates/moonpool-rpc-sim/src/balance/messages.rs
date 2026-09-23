//! The balance campaign's method. Identities are explicit constants; the
//! server, boot and value in a reply are application data the oracle
//! checks against the ledger.

use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion};

/// One unit of work, identified by a workload-generated id.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Job {
    /// Unique per run.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Ask the balancer's hooks for a comparison copy.
    #[prost(bool, tag = "2")]
    pub compare: bool,
}

/// What a server made of a job.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Done {
    /// The job id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The serving server's IP.
    #[prost(string, tag = "2")]
    pub server: String,
    /// Its boot.
    #[prost(uint64, tag = "3")]
    pub boot: u64,
    /// Declined without effect: the server is temporarily behind
    /// (application-defined).
    #[prost(bool, tag = "4")]
    pub behind: bool,
    /// The computed value: [`expected_value`] unless the server diverges.
    #[prost(uint64, tag = "5")]
    pub value: u64,
    /// The server's load penalty (1: normal).
    #[prost(double, tag = "6")]
    pub penalty: f64,
}

/// The value a healthy server computes for a job.
#[must_use]
pub fn expected_value(id: u64) -> u64 {
    id.wrapping_mul(2)
}

/// The balanced method.
pub struct Work;

impl RpcMethod for Work {
    type Request = Job;
    type Reply = Done;
    const METHOD: MethodId = MethodId::new(0x0217_0001);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "balance.work";
}
