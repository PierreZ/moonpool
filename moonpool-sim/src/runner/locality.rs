//! Failure-domain locality for correlated fault injection.
//!
//! Locality models a `FoundationDB`-style Cluster → Datacenter → Zone → Machine →
//! Process hierarchy so that collocated processes share fate. It is **orthogonal
//! to tags** ([`super::tags`]): tags round-robin independent dimensions, while
//! locality assigns *contiguous* hierarchical groups whose processes can be
//! rebooted together (see [`AttritionScope`](super::process::AttritionScope)).
//!
//! # Example
//!
//! ```ignore
//! // 3 datacenters × 3 zones × 3 machines × 1 process = 27 processes.
//! .cluster(LocalityConfig::new(3, 3, 3, 1), || Box::new(MyNode::new()))
//! ```

use std::collections::HashMap;
use std::net::IpAddr;

use super::builder::ProcessCount;

/// The level of a failure domain in the locality hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainLevel {
    /// A whole datacenter.
    Datacenter,
    /// A zone within a datacenter.
    Zone,
    /// A single machine — the unit of shared fate.
    Machine,
}

/// Resolved failure-domain locality for a single process instance.
///
/// Identifiers are globally unique and hierarchical (`dc1`, `dc1-z1`,
/// `dc1-z1-m1`) so that domain queries never confuse a machine in one
/// datacenter with a machine in another.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalityInfo {
    datacenter: String,
    zone: String,
    machine: String,
}

impl LocalityInfo {
    /// Create locality from explicit datacenter, zone, and machine ids.
    #[must_use]
    pub fn new(
        datacenter: impl Into<String>,
        zone: impl Into<String>,
        machine: impl Into<String>,
    ) -> Self {
        Self {
            datacenter: datacenter.into(),
            zone: zone.into(),
            machine: machine.into(),
        }
    }

    /// The datacenter id (e.g. `dc1`).
    #[must_use]
    pub fn datacenter(&self) -> &str {
        &self.datacenter
    }

    /// The zone id (e.g. `dc1-z1`).
    #[must_use]
    pub fn zone(&self) -> &str {
        &self.zone
    }

    /// The machine id (e.g. `dc1-z1-m1`).
    #[must_use]
    pub fn machine(&self) -> &str {
        &self.machine
    }

    /// The id at the given domain level.
    #[must_use]
    pub fn id_for(&self, level: DomainLevel) -> &str {
        match level {
            DomainLevel::Datacenter => &self.datacenter,
            DomainLevel::Zone => &self.zone,
            DomainLevel::Machine => &self.machine,
        }
    }
}

/// Configuration for laying processes out across a failure-domain topology.
///
/// Each dimension accepts a fixed count (`3`) or a range (`1..=3`) via
/// [`ProcessCount`]. Ranges are sampled once per seed from the simulation RNG,
/// so every seed exercises a different cluster shape — mirroring `FoundationDB`'s
/// per-seed topology generation. The total process count is the product of the
/// four sampled dimensions.
#[derive(Debug, Clone)]
pub struct LocalityConfig {
    datacenters: ProcessCount,
    zones_per_datacenter: ProcessCount,
    machines_per_zone: ProcessCount,
    processes_per_machine: ProcessCount,
}

impl LocalityConfig {
    /// Create a topology config. Each argument is a fixed count (`3`) or a range
    /// (`1..=3`) sampled per seed.
    #[must_use]
    pub fn new(
        datacenters: impl Into<ProcessCount>,
        zones_per_datacenter: impl Into<ProcessCount>,
        machines_per_zone: impl Into<ProcessCount>,
        processes_per_machine: impl Into<ProcessCount>,
    ) -> Self {
        Self {
            datacenters: datacenters.into(),
            zones_per_datacenter: zones_per_datacenter.into(),
            machines_per_zone: machines_per_zone.into(),
            processes_per_machine: processes_per_machine.into(),
        }
    }

    /// Sample the topology for the current seed and assign one [`LocalityInfo`]
    /// per process index via contiguous hierarchical slicing.
    ///
    /// Consecutive process indices share a machine, so collocation is contiguous
    /// — the property that makes machine-scoped reboots meaningful. Each
    /// dimension is clamped to at least 1.
    pub(crate) fn resolve_topology(&self) -> Vec<LocalityInfo> {
        let datacenters = self.datacenters.resolve().max(1);
        let zones = self.zones_per_datacenter.resolve().max(1);
        let machines = self.machines_per_zone.resolve().max(1);
        let processes = self.processes_per_machine.resolve().max(1);

        let mut out = Vec::with_capacity(datacenters * zones * machines * processes);
        for d in 0..datacenters {
            for z in 0..zones {
                for m in 0..machines {
                    let datacenter = format!("dc{}", d + 1);
                    let zone = format!("dc{}-z{}", d + 1, z + 1);
                    let machine = format!("dc{}-z{}-m{}", d + 1, z + 1, m + 1);
                    for _ in 0..processes {
                        out.push(LocalityInfo::new(
                            datacenter.clone(),
                            zone.clone(),
                            machine.clone(),
                        ));
                    }
                }
            }
        }
        out
    }
}

/// Registry mapping process IPs to their resolved locality.
///
/// Parallel to [`TagRegistry`](super::tags::TagRegistry) but for failure
/// domains: supports shared-fate queries (all IPs on a machine, in a zone, or in
/// a datacenter).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MachineRegistry {
    ip_locality: HashMap<IpAddr, LocalityInfo>,
}

impl MachineRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ip_locality: HashMap::new(),
        }
    }

    /// Register locality for a process IP.
    pub fn register(&mut self, ip: IpAddr, locality: LocalityInfo) {
        self.ip_locality.insert(ip, locality);
    }

    /// Whether any locality has been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ip_locality.is_empty()
    }

    /// Get the locality for a specific IP.
    #[must_use]
    pub fn locality_for(&self, ip: IpAddr) -> Option<&LocalityInfo> {
        self.ip_locality.get(&ip)
    }

    /// Find all IPs whose domain at `level` matches `id`.
    #[must_use]
    pub fn ips_in_domain(&self, level: DomainLevel, id: &str) -> Vec<IpAddr> {
        self.ip_locality
            .iter()
            .filter(|(_, loc)| loc.id_for(level) == id)
            .map(|(ip, _)| *ip)
            .collect()
    }

    /// Find all IPs on a single machine — the unit of shared fate.
    #[must_use]
    pub fn ips_on_machine(&self, machine_id: &str) -> Vec<IpAddr> {
        self.ips_in_domain(DomainLevel::Machine, machine_id)
    }

    /// All distinct machine ids, sorted for determinism.
    #[must_use]
    pub fn all_machines(&self) -> Vec<String> {
        self.distinct_ids(DomainLevel::Machine)
    }

    /// All distinct zone ids, sorted for determinism.
    #[must_use]
    pub fn all_zones(&self) -> Vec<String> {
        self.distinct_ids(DomainLevel::Zone)
    }

    /// All distinct datacenter ids, sorted for determinism.
    #[must_use]
    pub fn all_datacenters(&self) -> Vec<String> {
        self.distinct_ids(DomainLevel::Datacenter)
    }

    fn distinct_ids(&self, level: DomainLevel) -> Vec<String> {
        let mut ids: Vec<String> = self
            .ip_locality
            .values()
            .map(|loc| loc.id_for(level).to_string())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: usize) -> IpAddr {
        format!("10.0.1.{n}").parse().expect("valid IP")
    }

    #[test]
    fn resolve_topology_slices_contiguously() {
        // 2 dc × 2 zone × 2 machine × 2 proc = 16 processes.
        let locs = LocalityConfig::new(2, 2, 2, 2).resolve_topology();
        assert_eq!(locs.len(), 16);

        // Consecutive indices share a machine (processes_per_machine = 2).
        assert_eq!(locs[0].machine(), "dc1-z1-m1");
        assert_eq!(locs[1].machine(), "dc1-z1-m1");
        assert_eq!(locs[2].machine(), "dc1-z1-m2");

        // Zone boundary after machines_per_zone * processes_per_machine = 4.
        assert_eq!(locs[0].zone(), "dc1-z1");
        assert_eq!(locs[4].zone(), "dc1-z2");

        // Datacenter boundary after zones * machines * processes = 8.
        assert_eq!(locs[7].datacenter(), "dc1");
        assert_eq!(locs[8].datacenter(), "dc2");
    }

    #[test]
    fn globally_unique_machine_ids_across_datacenters() {
        let locs = LocalityConfig::new(2, 1, 1, 1).resolve_topology();
        assert_eq!(locs.len(), 2);
        assert_ne!(locs[0].machine(), locs[1].machine());
        assert_eq!(locs[0].machine(), "dc1-z1-m1");
        assert_eq!(locs[1].machine(), "dc2-z1-m1");
    }

    #[test]
    fn registry_domain_queries() {
        // 2 dc × 1 zone × 2 machine × 1 proc = 4 processes.
        let locs = LocalityConfig::new(2, 1, 2, 1).resolve_topology();
        let mut reg = MachineRegistry::new();
        for (i, loc) in locs.iter().enumerate() {
            reg.register(ip(i + 1), loc.clone());
        }

        assert_eq!(reg.ips_in_domain(DomainLevel::Datacenter, "dc1").len(), 2);
        assert_eq!(reg.ips_on_machine("dc1-z1-m1").len(), 1);
        assert_eq!(reg.all_machines().len(), 4);
        assert_eq!(
            reg.all_datacenters(),
            vec!["dc1".to_string(), "dc2".to_string()]
        );
        assert!(!reg.is_empty());
    }

    #[test]
    fn id_for_matches_accessors() {
        let loc = LocalityInfo::new("dc1", "dc1-z2", "dc1-z2-m3");
        assert_eq!(loc.id_for(DomainLevel::Datacenter), loc.datacenter());
        assert_eq!(loc.id_for(DomainLevel::Zone), loc.zone());
        assert_eq!(loc.id_for(DomainLevel::Machine), loc.machine());
    }
}
