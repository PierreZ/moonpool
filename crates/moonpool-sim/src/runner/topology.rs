//! What one process or workload sees of the simulated cluster.
//!
//! [`WorkloadTopology`] comes from
//! [`SimContext::topology`](crate::SimContext::topology). It answers "where
//! is everyone" by process group
//! ([`ips_in_group`](WorkloadTopology::ips_in_group)), by tag
//! ([`ips_tagged`](WorkloadTopology::ips_tagged)) and by failure domain
//! ([`ips_in_domain`](WorkloadTopology::ips_in_domain),
//! [`peers_on_my_machine`](WorkloadTopology::peers_on_my_machine)), and
//! gives the actor its own IP, tags and locality.

use crate::locality::{DomainLevel, LocalityInfo};

use super::membership::Membership;
use super::tags::ProcessTags;

/// Topology information provided to workloads and processes.
///
/// Process queries (groups, tags, failure domains) read the run's live
/// [`Membership`]: a process spawned mid-run shows up once it boots, and a
/// retired one is gone at once.
#[derive(Debug, Clone)]
pub struct WorkloadTopology {
    /// The IP address assigned to this workload or process.
    pub(crate) my_ip: String,
    /// A workload's `(client_id, client_count)`; `None` for a process, whose
    /// rank in its group is read live.
    pub(crate) workload_rank: Option<(usize, usize)>,
    /// The IP addresses of the other peers present at boot (workloads +
    /// processes).
    pub(crate) peer_ips: Vec<String>,
    /// The names of those peers (parallel to `peer_ips`).
    pub(crate) peer_names: Vec<String>,
    /// Tags assigned to this workload/process (empty for workloads).
    pub(crate) my_tags: ProcessTags,
    /// Failure-domain locality of this process (`None` for workloads and for
    /// processes registered without [`.cluster()`](super::builder::SimulationBuilder::cluster)).
    pub(crate) my_locality: Option<LocalityInfo>,
    /// The run's live membership.
    pub(crate) membership: Membership,
    /// Shutdown signal that gets triggered when the first workload exits with Ok.
    pub(crate) shutdown_signal: tokio_util::sync::CancellationToken,
}

/// Parse an IP, `None` when it does not parse.
fn parse(ip: &str) -> Option<std::net::IpAddr> {
    ip.parse().ok()
}

/// Format a list of IPs as the topology names them.
fn strings(ips: Vec<std::net::IpAddr>) -> Vec<String> {
    ips.into_iter().map(|ip| ip.to_string()).collect()
}

impl WorkloadTopology {
    /// This actor's IP.
    #[must_use]
    pub fn my_ip(&self) -> &str {
        &self.my_ip
    }

    /// This actor's client id: a workload's index among the instances of its
    /// builder entry, or a process's rank in its group now (ascending IP
    /// order among the live members).
    #[must_use]
    pub fn client_id(&self) -> usize {
        self.workload_rank.map_or_else(
            || self.process_rank().map_or(0, |(rank, _)| rank),
            |(id, _)| id,
        )
    }

    /// The size of this actor's entry: a workload's instance count, or a
    /// process's group size now.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.workload_rank.map_or_else(
            || self.process_rank().map_or(1, |(_, size)| size),
            |(_, count)| count,
        )
    }

    fn process_rank(&self) -> Option<(usize, usize)> {
        self.membership.position_in_group(parse(&self.my_ip)?)
    }

    /// The membership version: +1 on every spawned process that boots and
    /// every retire, so a harness can notice the deployment changed.
    #[must_use]
    pub fn membership_version(&self) -> u64 {
        self.membership.version()
    }

    /// The run's live membership.
    #[must_use]
    pub fn membership(&self) -> &Membership {
        &self.membership
    }

    /// All process groups in registration order (groups started mid-run
    /// come last).
    ///
    /// Every `.processes()` / `.cluster()` call on the builder is one group,
    /// named after its process type ([`Process::name`](super::process::Process::name)).
    #[must_use]
    pub fn groups(&self) -> Vec<String> {
        self.membership.groups()
    }

    /// Get the IPs of every live process in `group`, ascending.
    ///
    /// Unlike [`all_process_ips`](Self::all_process_ips), this includes the
    /// caller itself when it is a member of the group. Unknown groups are empty.
    #[must_use]
    pub fn ips_in_group(&self, group: &str) -> Vec<String> {
        strings(self.membership.ips_in_group(group))
    }

    /// Get the group of a live process IP.
    #[must_use]
    pub fn group_for(&self, ip: &str) -> Option<String> {
        self.membership.group_for(parse(ip)?)
    }

    /// Get this process's own group (`None` for workloads).
    #[must_use]
    pub fn my_group(&self) -> Option<String> {
        self.group_for(&self.my_ip)
    }

    /// Find the IP address of a peer by its name (workloads, and the
    /// processes present at boot).
    #[must_use]
    pub fn peer_by_name(&self, name: &str) -> Option<String> {
        self.peer_names
            .iter()
            .position(|peer_name| peer_name == name)
            .map(|index| self.peer_ips[index].clone())
    }

    /// Get the live server process IPs, excluding the caller's own.
    #[must_use]
    pub fn all_process_ips(&self) -> Vec<String> {
        self.membership
            .process_ips()
            .into_iter()
            .filter(|ip| ip != &self.my_ip)
            .collect()
    }

    /// Get IPs of live processes matching a tag key=value pair.
    #[must_use]
    pub fn ips_tagged(&self, key: &str, value: &str) -> Vec<String> {
        strings(self.membership.ips_tagged(key, value))
    }

    /// Get tags for a specific live process IP.
    #[must_use]
    pub fn tags_for(&self, ip: &str) -> Option<ProcessTags> {
        self.membership.tags_for(parse(ip)?)
    }

    /// Get this process/workload's own tags.
    #[must_use]
    pub fn my_tags(&self) -> &ProcessTags {
        &self.my_tags
    }

    /// Get this process's failure-domain locality, if it was assigned via
    /// [`.cluster()`](super::builder::SimulationBuilder::cluster).
    #[must_use]
    pub fn my_locality(&self) -> Option<&LocalityInfo> {
        self.my_locality.as_ref()
    }

    /// Get the locality of a specific live process IP.
    #[must_use]
    pub fn locality_for(&self, ip: &str) -> Option<LocalityInfo> {
        self.membership.locality_for(parse(ip)?)
    }

    /// Get the IPs of the other live processes collocated on this process's
    /// machine.
    ///
    /// Returns an empty vector for workloads or processes without locality.
    #[must_use]
    pub fn peers_on_my_machine(&self) -> Vec<String> {
        let Some(machine) = self.my_locality.as_ref().map(LocalityInfo::machine) else {
            return Vec::new();
        };
        strings(self.membership.machine_registry().ips_on_machine(machine))
            .into_iter()
            .filter(|ip| ip != &self.my_ip)
            .collect()
    }

    /// Get the IPs of all live processes in a failure domain (`level` + `id`).
    #[must_use]
    pub fn ips_in_domain(&self, level: DomainLevel, id: &str) -> Vec<String> {
        strings(self.membership.machine_registry().ips_in_domain(level, id))
    }
}

/// Inputs needed to construct a [`WorkloadTopology`].
pub(crate) struct TopologyInputs<'a> {
    /// The IP of the workload or process being configured.
    pub(crate) ip: &'a str,
    /// A workload's `(client_id, client_count)`; `None` for a process.
    pub(crate) workload_rank: Option<(usize, usize)>,
    /// All `(name, ip)` pairs present at boot (workloads + processes).
    pub(crate) all_entities: &'a [(String, String)],
    /// Tags assigned to this workload/process.
    pub(crate) my_tags: ProcessTags,
    /// Failure-domain locality of this process (`None` for workloads).
    pub(crate) my_locality: Option<LocalityInfo>,
    /// The run's live membership.
    pub(crate) membership: Membership,
    /// Shutdown signal observed by this workload/process.
    pub(crate) shutdown_signal: tokio_util::sync::CancellationToken,
}

/// Factory for creating workload topology configurations.
pub(crate) struct TopologyFactory;

impl TopologyFactory {
    /// Create topology for a workload or process.
    pub(crate) fn create_topology_with_processes(inputs: TopologyInputs<'_>) -> WorkloadTopology {
        let TopologyInputs {
            ip,
            workload_rank,
            all_entities,
            my_tags,
            my_locality,
            membership,
            shutdown_signal,
        } = inputs;

        let (peer_ips, peer_names): (Vec<_>, Vec<_>) = all_entities
            .iter()
            .filter(|(_, peer_ip)| peer_ip != ip)
            .map(|(name, peer_ip)| (peer_ip.clone(), name.clone()))
            .unzip();

        WorkloadTopology {
            my_ip: ip.to_string(),
            workload_rank,
            peer_ips,
            peer_names,
            my_tags,
            my_locality,
            membership,
            shutdown_signal,
        }
    }
}
