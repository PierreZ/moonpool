//! Failure-domain locality vocabulary shared by the engine and the runner.
//!
//! Locality models a `FoundationDB`-style Cluster → Datacenter → Zone → Machine →
//! Process hierarchy so that collocated processes share fate. The two core types
//! live here, at the crate root, because both layers need them:
//!
//! - the [`runner`](crate::runner) builds the topology
//!   ([`LocalityConfig`](crate::runner::LocalityConfig),
//!   [`MachineRegistry`](crate::runner::MachineRegistry)) and drives correlated
//!   reboots from it;
//! - the [`sim`](crate::sim) engine consumes a plain `ip -> LocalityInfo` map
//!   ([`SimWorld::set_localities`](crate::SimWorld::set_localities)) to shape
//!   network partitions and distance-based latency.
//!
//! The engine never imports from the runner, so the vocabulary sits below both.

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

#[cfg(test)]
mod tests {
    use super::{DomainLevel, LocalityInfo};

    #[test]
    fn id_for_matches_accessors() {
        let loc = LocalityInfo::new("dc1", "dc1-z2", "dc1-z2-m3");
        assert_eq!(loc.id_for(DomainLevel::Datacenter), loc.datacenter());
        assert_eq!(loc.id_for(DomainLevel::Zone), loc.zone());
        assert_eq!(loc.id_for(DomainLevel::Machine), loc.machine());
    }
}
