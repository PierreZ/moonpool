//! Paxos client for submitting commands to the cluster.
//!
//! The client discovers the current primary from the configuration master
//! and submits commands via RPC. If the primary fails or a `NotLeader` error
//! is returned, the client re-discovers the primary and retries.
//!
//! ## Raft Comparison
//!
//! | Aspect | Raft | VP II |
//! |---|---|---|
//! | **Discovery** | Any cluster node can redirect | Master provides current config |
//! | **Redirect** | Leader hint in RPC response | Re-query master's current_config |
//! | **Retry** | Client retries with new leader | Client retries after re-discovery |
//!
//! In Raft, a client typically connects to any node, which either handles the
//! request (if it's the leader) or redirects to the leader. In VP II, the
//! client goes through the configuration master for discovery, which is a
//! centralized but simpler approach.
//!
//! ## Client Flow
//!
//! ```text
//! Client                  Master                  Primary
//!   │                       │                       │
//!   │── current_config() ──>│                       │
//!   │<── config (leader) ───│                       │
//!   │                                               │
//!   │── submit(command) ───────────────────────────>│
//!   │<── CommandResponse ──────────────────────────│
//!   │                                               │
//!   │  (if error or NotLeader:)                    │
//!   │── current_config() ──>│                       │
//!   │<── new config ────────│                       │
//!   │── submit(command) ──────────> (new primary)   │
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use moonpool_core::{Endpoint, NetworkAddress, TimeProvider, UID};
use moonpool_transport::{JsonCodec, NetTransport, Providers, send_request};
use tracing::{debug, info, warn};

use crate::leader::{LEADER_INTERFACE_ID, SUBMIT_METHOD};
use crate::master::ConfigurationMaster;
use crate::types::{CommandRequest, CommandResponse, Configuration, PaxosError};

/// Maximum number of retries when the leader changes during a request.
const MAX_RETRIES: usize = 3;

/// Paxos client for submitting commands.
///
/// The client maintains a cached reference to the current primary and
/// re-discovers it from the master when needed (on error or startup).
///
/// ## Usage
///
/// ```ignore
/// let client = PaxosClient::new(master);
/// let response = client.submit(&transport, &time, b"set x=42".to_vec()).await?;
/// println!("Command committed at slot {}", response.slot);
/// ```
pub struct PaxosClient {
    /// Reference to the configuration master for leader discovery.
    master: Rc<RefCell<dyn ConfigurationMaster>>,

    /// Cached current configuration (leader address + acceptors).
    ///
    /// Set to `None` on startup or after a discovery failure, forcing
    /// re-discovery on the next request.
    cached_config: Option<Configuration>,
}

impl PaxosClient {
    /// Create a new Paxos client.
    ///
    /// The client will discover the current primary on its first request
    /// by querying the master.
    pub fn new(master: Rc<RefCell<dyn ConfigurationMaster>>) -> Self {
        Self {
            master,
            cached_config: None,
        }
    }

    /// Submit a command to the Paxos cluster.
    ///
    /// The client:
    /// 1. Discovers the current primary (from cache or master)
    /// 2. Sends the command to the primary
    /// 3. If the primary fails or returns NotLeader, re-discovers and retries
    ///
    /// # Arguments
    ///
    /// * `transport` - Network transport for RPC
    /// * `time` - Time provider for timeouts
    /// * `command` - The serialized command bytes to replicate
    ///
    /// # Returns
    ///
    /// `CommandResponse` with the slot where the command was committed.
    ///
    /// # Errors
    ///
    /// Returns `PaxosError` if the command could not be committed after
    /// `MAX_RETRIES` attempts.
    pub async fn submit<P: Providers>(
        &mut self,
        transport: &Rc<NetTransport<P>>,
        time: &P::Time,
        command: Vec<u8>,
    ) -> Result<CommandResponse, PaxosError> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            // Step 1: Discover current primary
            let config = self.discover_primary()?;
            let leader_addr = config.leader.clone();

            debug!(
                attempt = attempt,
                leader = %leader_addr,
                "submitting command to primary"
            );

            // Step 2: Send command to primary
            let request = CommandRequest {
                command: command.clone(),
            };

            match self
                .send_command(transport, time, &leader_addr, request)
                .await
            {
                Ok(response) if response.success => {
                    info!(slot = %response.slot, "command committed");
                    return Ok(response);
                }
                Ok(response) => {
                    // Command was not successful — leader may have lost its ballot
                    warn!(
                        slot = %response.slot,
                        "command rejected by leader, re-discovering primary"
                    );
                    self.invalidate_cache();
                    last_error = Some(PaxosError::NotLeader);
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        attempt = attempt,
                        "command failed, re-discovering primary"
                    );
                    self.invalidate_cache();
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or(PaxosError::NotLeader))
    }

    /// Discover the current primary from the master.
    ///
    /// Uses the cached configuration if available, otherwise queries
    /// the master.
    fn discover_primary(&mut self) -> Result<Configuration, PaxosError> {
        if let Some(config) = &self.cached_config {
            return Ok(config.clone());
        }

        let config = {
            let m = self.master.borrow();
            m.current_config()?
        };

        match config {
            Some(config) => {
                debug!(leader = %config.leader, ballot = %config.ballot, "discovered primary");
                self.cached_config = Some(config.clone());
                Ok(config)
            }
            None => Err(PaxosError::NotLeader),
        }
    }

    /// Invalidate the cached configuration, forcing re-discovery.
    fn invalidate_cache(&mut self) {
        self.cached_config = None;
    }

    /// Send a command RPC to the given leader address.
    async fn send_command<P: Providers>(
        &self,
        transport: &Rc<NetTransport<P>>,
        time: &P::Time,
        leader_addr: &NetworkAddress,
        request: CommandRequest,
    ) -> Result<CommandResponse, PaxosError> {
        let endpoint = make_leader_submit_endpoint(leader_addr);

        let future = send_request(transport, &endpoint, request, JsonCodec)
            .map_err(|e| PaxosError::Network(e.to_string()))?;

        let timeout_duration = std::time::Duration::from_secs(10);
        match time.timeout(timeout_duration, future).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(reply_err)) => Err(PaxosError::Network(reply_err.to_string())),
            Err(_) => Err(PaxosError::Timeout),
        }
    }
}

/// Create an endpoint for the leader's submit RPC.
fn make_leader_submit_endpoint(addr: &NetworkAddress) -> Endpoint {
    let base = Endpoint::new(addr.clone(), UID::new(LEADER_INTERFACE_ID, 0));
    moonpool_transport::method_endpoint(&base, SUBMIT_METHOD as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master::InMemoryConfigMaster;
    use crate::types::BallotNumber;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port)
    }

    fn make_config(ballot: u64, leader_port: u16) -> Configuration {
        Configuration {
            ballot: BallotNumber::new(ballot),
            leader: make_addr(leader_port),
            acceptors: vec![make_addr(5001), make_addr(5002), make_addr(5003)],
        }
    }

    fn make_master_with_config(
        ballot: u64,
        leader_port: u16,
    ) -> Rc<RefCell<dyn ConfigurationMaster>> {
        let mut master = InMemoryConfigMaster::new();
        master
            .bootstrap(make_config(ballot, leader_port))
            .expect("bootstrap");
        Rc::new(RefCell::new(master))
    }

    #[test]
    fn test_client_discover_primary() {
        let master = make_master_with_config(1, 5001);
        let mut client = PaxosClient::new(master);

        let config = client.discover_primary().expect("discover");
        assert_eq!(config.leader, make_addr(5001));
        assert_eq!(config.ballot, BallotNumber::new(1));
    }

    #[test]
    fn test_client_caches_config() {
        let master = make_master_with_config(1, 5001);
        let mut client = PaxosClient::new(master);

        // First call discovers
        let config1 = client.discover_primary().expect("discover");

        // Second call uses cache
        let config2 = client.discover_primary().expect("discover");
        assert_eq!(config1, config2);
    }

    #[test]
    fn test_client_invalidate_cache() {
        let master = make_master_with_config(1, 5001);
        let mut client = PaxosClient::new(master.clone());

        // Discover and cache
        client.discover_primary().expect("discover");
        assert!(client.cached_config.is_some());

        // Invalidate
        client.invalidate_cache();
        assert!(client.cached_config.is_none());
    }

    #[test]
    fn test_client_no_leader() {
        let master: Rc<RefCell<dyn ConfigurationMaster>> =
            Rc::new(RefCell::new(InMemoryConfigMaster::new()));
        let mut client = PaxosClient::new(master);

        let result = client.discover_primary();
        assert!(result.is_err());
    }

    #[test]
    fn test_make_leader_endpoint() {
        let addr = make_addr(5001);
        let endpoint = make_leader_submit_endpoint(&addr);

        assert_eq!(endpoint.address, addr);
        assert!(endpoint.token.is_valid());
    }

    #[test]
    fn test_client_rediscovers_after_invalidation() {
        let master = make_master_with_config(1, 5001);
        let mut client = PaxosClient::new(master.clone());

        // Initial discovery
        let config1 = client.discover_primary().expect("discover");
        assert_eq!(config1.ballot, BallotNumber::new(1));

        // Simulate reconfiguration on the master
        {
            let mut m = master.borrow_mut();
            m.new_ballot(make_config(2, 5002)).expect("new_ballot");
            m.report_complete(BallotNumber::new(2), BallotNumber::new(1))
                .expect("complete");
        }

        // Without invalidation, cache still returns old config
        let config_cached = client.discover_primary().expect("discover");
        assert_eq!(config_cached.ballot, BallotNumber::new(1));

        // After invalidation, re-discovers new config
        client.invalidate_cache();
        let config2 = client.discover_primary().expect("discover");
        assert_eq!(config2.ballot, BallotNumber::new(2));
        assert_eq!(config2.leader, make_addr(5002));
    }
}
