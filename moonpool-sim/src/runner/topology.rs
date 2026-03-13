//! Workload and process topology configuration.
//!
//! This module provides types for configuring topology and
//! creating topology information for workloads and processes.

use super::tags::{ProcessTags, TagRegistry};

/// Topology information provided to workloads and processes.
#[derive(Debug, Clone)]
pub struct WorkloadTopology {
    /// The IP address assigned to this workload or process.
    pub my_ip: String,
    /// This workload's client ID (assigned by the builder's [`ClientId`] strategy).
    /// For processes, this is the process index.
    pub client_id: usize,
    /// Total number of workload instances sharing this entry (factory count or 1).
    /// For processes, this is the total process count.
    pub client_count: usize,
    /// The IP addresses of all other peers in the simulation (workloads + processes).
    pub peer_ips: Vec<String>,
    /// The names of all other peers in the simulation (parallel to peer_ips).
    pub peer_names: Vec<String>,
    /// All server process IP addresses.
    pub process_ips: Vec<String>,
    /// Tags assigned to this workload/process (empty for workloads without tags).
    pub my_tags: ProcessTags,
    /// Tag registry for querying process tags.
    pub tag_registry: TagRegistry,
    /// Shutdown signal that gets triggered when the first workload exits with Ok.
    pub shutdown_signal: tokio_util::sync::CancellationToken,
}

impl WorkloadTopology {
    /// Find the IP address of a peer by its workload name.
    pub fn peer_by_name(&self, name: &str) -> Option<String> {
        self.peer_names
            .iter()
            .position(|peer_name| peer_name == name)
            .map(|index| self.peer_ips[index].clone())
    }

    /// Get all peers with a name prefix (useful for finding multiple clients, servers, etc.)
    pub fn peers_with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        self.peer_names
            .iter()
            .zip(self.peer_ips.iter())
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, ip)| (name.clone(), ip.clone()))
            .collect()
    }

    /// Get all server process IPs in the simulation.
    pub fn all_process_ips(&self) -> &[String] {
        &self.process_ips
    }

    /// Get IPs of processes matching a tag key=value pair.
    pub fn ips_tagged(&self, key: &str, value: &str) -> Vec<String> {
        self.tag_registry
            .ips_tagged(key, value)
            .into_iter()
            .map(|ip| ip.to_string())
            .collect()
    }

    /// Get tags for a specific IP.
    pub fn tags_for(&self, ip: &str) -> Option<&ProcessTags> {
        let ip_addr: std::net::IpAddr = ip.parse().ok()?;
        self.tag_registry.tags_for(ip_addr)
    }

    /// Get this process/workload's own tags.
    pub fn my_tags(&self) -> &ProcessTags {
        &self.my_tags
    }
}

/// Factory for creating workload topology configurations.
pub(crate) struct TopologyFactory;

impl TopologyFactory {
    /// Create topology for a workload or process, including process information.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_topology_with_processes(
        ip: &str,
        client_id: usize,
        client_count: usize,
        all_entities: &[(String, String)],
        process_ips: &[String],
        my_tags: ProcessTags,
        tag_registry: TagRegistry,
        shutdown_signal: tokio_util::sync::CancellationToken,
    ) -> WorkloadTopology {
        let peer_ips = all_entities
            .iter()
            .filter(|(_, peer_ip)| peer_ip != ip)
            .map(|(_, peer_ip)| peer_ip.clone())
            .collect();

        let peer_names = all_entities
            .iter()
            .filter(|(_, peer_ip)| peer_ip != ip)
            .map(|(name, _)| name.clone())
            .collect();

        WorkloadTopology {
            my_ip: ip.to_string(),
            client_id,
            client_count,
            peer_ips,
            peer_names,
            process_ips: process_ips.iter().filter(|p| *p != ip).cloned().collect(),
            my_tags,
            tag_registry,
            shutdown_signal,
        }
    }
}
