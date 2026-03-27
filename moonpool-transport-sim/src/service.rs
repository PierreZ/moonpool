//! Echo service types and hand-rolled endpoint constants.
//!
//! Three methods exercising different server behaviors:
//! - `echo`: immediate echo
//! - `echo_delayed`: echo after random delay
//! - `echo_or_fail`: echo with buggify-controlled BrokenPromise

use moonpool_sim::SimulationResult;
use moonpool_transport::{NetworkAddress, UID};
use serde::{Deserialize, Serialize};

// =============================================================================
// Interface constants
// =============================================================================

/// Interface ID for the echo service.
const ECHO_INTERFACE: u64 = 0xECE0_0001;

/// Method index for `echo`.
pub const METHOD_ECHO: u64 = 1;

/// Method index for `echo_delayed`.
pub const METHOD_ECHO_DELAYED: u64 = 2;

/// Method index for `echo_or_fail`.
pub const METHOD_ECHO_OR_FAIL: u64 = 3;

/// Get the UID for a method on the echo interface.
pub fn echo_method_uid(method_index: u64) -> UID {
    UID::new(ECHO_INTERFACE, method_index)
}

/// Parse a sim IP address (which may lack a port) into a NetworkAddress.
/// Appends `:4500` if no port is present.
pub fn parse_sim_addr(ip: &str) -> SimulationResult<NetworkAddress> {
    let addr_str = if ip.contains(':') {
        ip.to_string()
    } else {
        format!("{ip}:4500")
    };
    NetworkAddress::parse(&addr_str)
        .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("bad addr: {e}")))
}

// =============================================================================
// Message types
// =============================================================================

/// Which delivery mode the client used for this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeliveryMode {
    /// `send()` - fire and forget, unreliable.
    FireAndForget,
    /// `try_get_reply()` - at-most-once, unreliable.
    AtMostOnce,
    /// `get_reply()` - at-least-once, reliable.
    AtLeastOnce,
    /// `get_reply_unless_failed_for()` - at-least-once with timeout.
    Timeout,
}

impl std::fmt::Display for DeliveryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeliveryMode::FireAndForget => write!(f, "fire_and_forget"),
            DeliveryMode::AtMostOnce => write!(f, "at_most_once"),
            DeliveryMode::AtLeastOnce => write!(f, "at_least_once"),
            DeliveryMode::Timeout => write!(f, "timeout"),
        }
    }
}

/// Request sent by the client workload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EchoRequest {
    /// Unique sequence ID per client.
    pub seq_id: u64,
    /// Identifies which client sent this.
    pub client_id: usize,
    /// Which delivery mode was used (for invariant tracking).
    pub mode: DeliveryMode,
    /// Which method this targets (1, 2, or 3).
    pub method: u64,
}

/// Response from the echo server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EchoResponse {
    /// Echoed sequence ID.
    pub seq_id: u64,
    /// Echoed client ID.
    pub client_id: usize,
    /// Server IP that handled the request.
    pub server_ip: String,
}
