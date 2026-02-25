//! Workload topology configuration.
//!
//! This module provides types for configuring workload topology and
//! creating topology information for individual workloads.

/// Topology information provided to workloads to understand the simulation network.
#[derive(Debug, Clone)]
pub struct WorkloadTopology {
    /// The IP address assigned to this workload
    pub my_ip: String,
    /// This workload's client ID (assigned by the builder's [`ClientId`] strategy)
    pub client_id: usize,
    /// Total number of workload instances sharing this entry (factory count or 1)
    pub client_count: usize,
    /// The IP addresses of all other peers in the simulation
    pub peer_ips: Vec<String>,
    /// The names of all other peers in the simulation (parallel to peer_ips)
    pub peer_names: Vec<String>,
    /// Shutdown signal that gets triggered when the first workload exits with Ok
    pub shutdown_signal: tokio_util::sync::CancellationToken,
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

/// Factory for creating workload topology configurations.
pub(crate) struct TopologyFactory;

impl TopologyFactory {
    /// Create topology for a specific workload within a set of all workloads.
    ///
    /// `all_workloads` is a list of `(name, ip)` pairs for all workloads.
    pub(crate) fn create_topology(
        workload_ip: &str,
        client_id: usize,
        client_count: usize,
        all_workloads: &[(String, String)],
        shutdown_signal: tokio_util::sync::CancellationToken,
    ) -> WorkloadTopology {
        let peer_ips = all_workloads
            .iter()
            .filter(|(_, ip)| ip != workload_ip)
            .map(|(_, ip)| ip.clone())
            .collect();

        let peer_names = all_workloads
            .iter()
            .filter(|(_, ip)| ip != workload_ip)
            .map(|(name, _)| name.clone())
            .collect();

        WorkloadTopology {
            my_ip: workload_ip.to_string(),
            client_id,
            client_count,
            peer_ips,
            peer_names,
            shutdown_signal,
        }
    }
}
