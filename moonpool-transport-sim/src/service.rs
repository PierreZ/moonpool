//! RPC types for the hash-chain workload and the shared `parse_sim_addr` helper.

use std::net::IpAddr;

use moonpool_sim::SimulationResult;
use moonpool_transport::{NetworkAddress, UID};
use serde::{Deserialize, Serialize};

const DEFAULT_SIM_PORT: u16 = 4500;

/// Interface ID for the append-block service.
pub(crate) const APPEND_INTERFACE: u64 = 0xC4A1_0001;

/// Method index for `append_block`.
pub const METHOD_APPEND_BLOCK: u64 = 1;

/// UID for a method on the append-block interface.
#[must_use]
pub fn append_method_uid(method_index: u64) -> UID {
    UID::new(APPEND_INTERFACE, method_index)
}

/// Parse a sim IP address (which may lack a port) into a [`NetworkAddress`].
/// Appends `:4500` when no port is present.
///
/// # Errors
///
/// Returns [`moonpool_sim::SimulationError::InvalidState`] if `ip` cannot be parsed.
pub fn parse_sim_addr(ip: &str) -> SimulationResult<NetworkAddress> {
    let parsed = if let Ok(ip) = ip.parse::<IpAddr>() {
        Ok(NetworkAddress::new(ip, DEFAULT_SIM_PORT))
    } else {
        NetworkAddress::parse(ip)
    };
    parsed.map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("bad addr: {e}")))
}

/// Request to append a block to the hash chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendBlockRequest {
    /// Workload-side request id (independent of `N`); used for request/response matching.
    pub seq_id: u64,
    /// Identifies the originating workload instance.
    pub client_id: usize,
    /// Block payload (0..=64 bytes; may be empty under adversarial ops).
    pub block: Vec<u8>,
}

/// Response from a successful append.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendBlockResponse {
    /// Echoed request id.
    pub seq_id: u64,
    /// New block count after this append (`N` post-transition).
    pub n: u64,
    /// New chain digest after this append (`H` post-transition).
    pub h: u64,
    /// IP of the server that handled the request.
    pub server_ip: String,
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn parse_sim_addr_defaults_bare_ip_port() {
        assert_eq!(
            parse_sim_addr("10.0.1.1").expect("valid IPv4 address"),
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1)), DEFAULT_SIM_PORT)
        );
        assert_eq!(
            parse_sim_addr("::1").expect("valid IPv6 address"),
            NetworkAddress::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_SIM_PORT)
        );
    }

    #[test]
    fn parse_sim_addr_preserves_explicit_port() {
        assert_eq!(
            parse_sim_addr("10.0.1.1:1234").expect("valid IPv4 socket address"),
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1)), 1234)
        );
        assert_eq!(
            parse_sim_addr("[::1]:1234").expect("valid IPv6 socket address"),
            NetworkAddress::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 1234)
        );
    }

    #[test]
    fn parse_sim_addr_rejects_invalid_address() {
        assert!(parse_sim_addr("not-an-address").is_err());
    }
}
