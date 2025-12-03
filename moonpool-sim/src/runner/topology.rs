//! Workload topology configuration.
//!
//! This module provides types for configuring workload topology and
//! creating topology information for individual workloads.

use crate::chaos::StateRegistry;

/// Topology information provided to workloads to understand the simulation network.
#[derive(Debug, Clone)]
pub struct WorkloadTopology {
    /// The IP address assigned to this workload
    pub my_ip: String,
    /// The IP addresses of all other peers in the simulation
    pub peer_ips: Vec<String>,
    /// The names of all other peers in the simulation (parallel to peer_ips)
    pub peer_names: Vec<String>,
    /// Shutdown signal that gets triggered when the first workload exits with Ok
    pub shutdown_signal: tokio_util::sync::CancellationToken,
    /// State registry for cross-workload invariant checking
    pub state_registry: StateRegistry,
}

impl WorkloadTopology {
    /// Find the IP address of a peer by its workload name
    pub fn get_peer_by_name(&self, name: &str) -> Option<String> {
        self.peer_names
            .iter()
            .position(|peer_name| peer_name == name)
            .map(|index| self.peer_ips[index].clone())
    }

    /// Get all peers with a name prefix (useful for finding multiple clients, servers, etc.)
    pub fn get_peers_with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        self.peer_names
            .iter()
            .zip(self.peer_ips.iter())
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, ip)| (name.clone(), ip.clone()))
            .collect()
    }
}

/// A registered workload that can be executed during simulation.
pub struct Workload {
    pub(crate) name: String,
    pub(crate) ip_address: String,
    pub(crate) workload: super::builder::WorkloadFn,
}

impl std::fmt::Debug for Workload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workload")
            .field("name", &self.name)
            .field("ip_address", &self.ip_address)
            .field("workload", &"<closure>")
            .finish()
    }
}

/// Factory for creating workload topology configurations.
pub(crate) struct TopologyFactory;

impl TopologyFactory {
    /// Create topology for a specific workload within a set of all workloads.
    pub(crate) fn create_topology(
        workload_ip: &str,
        all_workloads: &[Workload],
        shutdown_signal: tokio_util::sync::CancellationToken,
        state_registry: StateRegistry,
    ) -> WorkloadTopology {
        let peer_ips = all_workloads
            .iter()
            .filter(|w| w.ip_address != workload_ip)
            .map(|w| w.ip_address.clone())
            .collect();

        let peer_names = all_workloads
            .iter()
            .filter(|w| w.ip_address != workload_ip)
            .map(|w| w.name.clone())
            .collect();

        WorkloadTopology {
            my_ip: workload_ip.to_string(),
            peer_ips,
            peer_names,
            shutdown_signal,
            state_registry,
        }
    }
}
