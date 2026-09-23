//! State keys the delivery campaign's roles share.

/// How many times the server booted.
pub const SERVER_BOOTS_KEY: &str = "rpc.delivery.server.boots";
/// Lost-reply jobs the server received (each asks for a cut).
pub const CUT_REQUESTS_KEY: &str = "rpc.delivery.cuts";
/// Cuts the fault script has put in place (the server replies into the
/// cut only once its request is counted here).
pub const CUTS_INSTALLED_KEY: &str = "rpc.delivery.cuts.installed";
/// Crash jobs the server received (each asks for a crash).
pub const CRASH_REQUESTS_KEY: &str = "rpc.delivery.crashes";
/// The workload's IP, published when it starts.
pub const WORKLOAD_IP_KEY: &str = "rpc.delivery.workload.ip";
/// The workload's runtime handle, published when it starts, so the fault
/// script can see when the workload noticed a cut.
pub const WORKLOAD_RPC_KEY: &str = "rpc.delivery.workload.rpc";
/// Set once the fault script stopped serving crash and cut requests.
pub const SCRIPT_DONE_KEY: &str = "rpc.delivery.script.done";
/// The scripted resolver the workload bootstraps through.
pub const RESOLVER_KEY: &str = "rpc.delivery.resolver";
/// The bootstrap name of the server.
pub const SERVER_NAME: &str = "server.rpc";
/// Board label of the workload's runtime.
pub const WORKLOAD_LABEL: &str = "workload";
