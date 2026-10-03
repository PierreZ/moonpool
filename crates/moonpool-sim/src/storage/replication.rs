//! Replicated fault patterns: random storage damage spread over failure
//! domains so that no replicated record is damaged everywhere.
//!
//! A replicated system survives disk damage only while some replica keeps
//! a clean copy of every record. Random faults, drawn independently per
//! disk, eventually hit every copy of one record and turn a valid run into
//! an unwinnable one. So the simulator, which knows the topology, decides
//! *where* damage may land, from what each process publishes about its own
//! layout ([`LayoutRegion`]s with a `stripe`, the record's key shared by
//! every replica's copy):
//!
//! - **minority**: only the processes of a few failure domains take damage,
//!   anywhere on their disks — `TigerBeetle`'s minority corruption;
//! - **helical**: every domain takes damage, but each stripe only in the
//!   domains its key rotates to, so the damaged records "spin" around the
//!   domains — `TigerBeetle`'s helical corruption, keyed by record instead
//!   of by file offset, so it holds even where replicas lay their records
//!   out differently. Bytes a node alone holds (stripe `None`, or bytes it
//!   never published) are damaged only in the domains drawn for them.
//!
//! In both, a record's copies are damaged in at most `tolerance` domains,
//! and so is node-local data: by construction, with no ledger.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::ops::Range;

use moonpool_core::LayoutRegion;

use super::faults::SECTOR_SIZE;
use crate::{DomainLevel, LocalityInfo};

/// Opt-in replicated fault patterns for a simulation
/// ([`SimulationBuilder::replicated_storage_faults`](crate::SimulationBuilder::replicated_storage_faults)).
///
/// Each seed draws one [`FaultPattern`] over the failure domains at
/// `level` of the run's topology. A record (a stripe) loses copies in at
/// most `tolerance` domains, and so does node-local data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicatedFaults {
    level: DomainLevel,
    tolerance: usize,
}

impl ReplicatedFaults {
    /// Spread damage over the domains at `level`, losing each record in at
    /// most one of them.
    #[must_use]
    pub fn new(level: DomainLevel) -> Self {
        Self {
            level,
            tolerance: 1,
        }
    }

    /// How many domains may lose their copy of one record (at least 1).
    /// It is clamped below the number of domains a seed draws, so one
    /// domain always keeps every record.
    #[must_use]
    pub fn tolerance(mut self, domains: usize) -> Self {
        self.tolerance = domains.max(1);
        self
    }

    /// The failure-domain level damage is spread over.
    #[must_use]
    pub fn level(&self) -> DomainLevel {
        self.level
    }
}

/// The pattern one seed drew (see [`ReplicatedFaults`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultPattern {
    /// Only these domains take damage.
    Minority {
        /// The damaged domains' ids.
        domains: Vec<String>,
    },
    /// Every domain takes damage on the stripes its key rotates to; these
    /// domains also take damage on node-local bytes.
    Helical {
        /// The domains, in rotation order: stripe `s` may be damaged in
        /// the `tolerance` domains starting at `s mod len`.
        domains: Vec<String>,
        /// The domains whose node-local bytes may be damaged.
        local: Vec<String>,
    },
    /// The topology has fewer than two domains: no placement keeps a clean
    /// copy elsewhere, so no process inside it takes damage.
    Spared,
}

/// One seed's plan: the pattern, and each process's domain index.
#[derive(Debug, Clone)]
pub(crate) struct ReplicationPlan {
    pattern: FaultPattern,
    tolerance: usize,
    /// Domain index (into the pattern's rotation) per process.
    domain_of: BTreeMap<IpAddr, usize>,
    /// For a minority: whether each domain index is damaged. For a helix:
    /// whether each domain index may lose node-local bytes.
    marked: Vec<bool>,
}

impl ReplicationPlan {
    /// Draw a plan over `localities` with `pick(n)` returning a uniform
    /// index below `n` (the simulation stream) and `helical` choosing the
    /// pattern. With fewer than two domains the plan is
    /// [`FaultPattern::Spared`].
    pub(crate) fn draw(
        config: ReplicatedFaults,
        localities: &BTreeMap<IpAddr, LocalityInfo>,
        helical: bool,
        mut pick: impl FnMut(usize) -> usize,
    ) -> Self {
        let mut domains: Vec<String> = localities
            .values()
            .map(|locality| locality.id_for(config.level).to_string())
            .collect();
        domains.sort_unstable();
        domains.dedup();
        let domain_of: BTreeMap<IpAddr, usize> = localities
            .iter()
            .filter_map(|(ip, locality)| {
                let id = locality.id_for(config.level);
                domains.iter().position(|d| d == id).map(|at| (*ip, at))
            })
            .collect();
        if domains.len() < 2 {
            return Self {
                pattern: FaultPattern::Spared,
                tolerance: 0,
                domain_of,
                marked: vec![false; domains.len()],
            };
        }
        let tolerance = config.tolerance.min(domains.len() - 1);
        // `tolerance` distinct domains, drawn without replacement.
        let mut pool: Vec<usize> = (0..domains.len()).collect();
        let mut marked = vec![false; domains.len()];
        for _ in 0..tolerance {
            let at = pick(pool.len());
            marked[pool.swap_remove(at)] = true;
        }
        let chosen: Vec<String> = domains
            .iter()
            .zip(&marked)
            .filter(|(_, marked)| **marked)
            .map(|(id, _)| id.clone())
            .collect();
        let pattern = if helical {
            FaultPattern::Helical {
                domains,
                local: chosen,
            }
        } else {
            FaultPattern::Minority { domains: chosen }
        };
        Self {
            pattern,
            tolerance,
            domain_of,
            marked,
        }
    }

    pub(crate) fn pattern(&self) -> &FaultPattern {
        &self.pattern
    }

    fn domain_count(&self) -> usize {
        self.marked.len()
    }

    /// Whether stripe `stripe` may be damaged in domain `domain`.
    fn stripe_allowed(&self, stripe: u64, domain: usize) -> bool {
        let count = self.domain_count() as u64;
        let first = stripe % count;
        let offset = (domain as u64 + count - first) % count;
        offset < self.tolerance as u64
    }

    /// Whether a random fault may damage `sector` of `owner`'s file at
    /// `path`, given what `owner` published. A process outside the
    /// topology is not constrained.
    pub(crate) fn allows(
        &self,
        owner: IpAddr,
        layout: Option<&LayoutIndex>,
        path: &str,
        sector: u64,
    ) -> bool {
        let Some(&domain) = self.domain_of.get(&owner) else {
            return true;
        };
        match &self.pattern {
            FaultPattern::Spared => false,
            FaultPattern::Minority { .. } => self.marked[domain],
            FaultPattern::Helical { .. } => {
                let mut stripes = layout
                    .map(|layout| layout.stripes(path, sector))
                    .unwrap_or_default();
                if stripes.is_empty() || stripes.contains(&None) {
                    // Bytes this node alone holds, or never described.
                    return self.marked[domain];
                }
                stripes.dedup();
                stripes
                    .into_iter()
                    .flatten()
                    .all(|stripe| self.stripe_allowed(stripe, domain))
            }
        }
    }
}

/// One file's published regions: `(byte range, stripe)`, sorted by start.
type Regions = Vec<(Range<u64>, Option<u64>)>;

/// One process's published layout, by path, for sector lookups.
#[derive(Debug, Clone, Default)]
pub(crate) struct LayoutIndex {
    /// Per path: `(byte range, stripe)`, sorted by range start.
    by_path: BTreeMap<String, Regions>,
    /// The longest range, bounding the backwards scan of a lookup.
    longest: u64,
}

impl LayoutIndex {
    pub(crate) fn new(regions: &[LayoutRegion]) -> Self {
        let mut index = Self::default();
        for region in regions {
            if region.bytes.start >= region.bytes.end {
                continue;
            }
            index.longest = index.longest.max(region.bytes.end - region.bytes.start);
            index
                .by_path
                .entry(super::faults::normalize_path(&region.path))
                .or_default()
                .push((region.bytes.clone(), region.stripe));
        }
        for ranges in index.by_path.values_mut() {
            ranges.sort_by_key(|(range, _)| range.start);
        }
        index
    }

    /// The stripes of every region sharing a byte with `sector`, in range
    /// order (`None` for node-local ones); empty if none was published.
    fn stripes(&self, path: &str, sector: u64) -> Vec<Option<u64>> {
        let size = SECTOR_SIZE as u64;
        let bytes = sector * size..(sector + 1) * size;
        let Some(ranges) = self.by_path.get(&super::faults::normalize_path(path)) else {
            return Vec::new();
        };
        let end = ranges.partition_point(|(range, _)| range.start < bytes.end);
        let floor = bytes.start.saturating_sub(self.longest);
        ranges[..end]
            .iter()
            .rev()
            .take_while(|(range, _)| range.start >= floor)
            .filter(|(range, _)| range.end > bytes.start)
            .map(|(_, stripe)| *stripe)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn localities(zones: &[&str]) -> BTreeMap<IpAddr, LocalityInfo> {
        zones
            .iter()
            .enumerate()
            .map(|(at, zone)| {
                let ip: IpAddr = format!("10.0.1.{}", at + 1).parse().expect("ip");
                (ip, LocalityInfo::new("dc", *zone, format!("m{at}")))
            })
            .collect()
    }

    fn ip(n: u8) -> IpAddr {
        format!("10.0.1.{n}").parse().expect("ip")
    }

    fn region(bytes: Range<u64>, stripe: Option<u64>) -> LayoutRegion {
        LayoutRegion {
            path: "wal/seg".to_string(),
            bytes,
            kind: "entry",
            stripe,
        }
    }

    #[test]
    fn a_helix_damages_each_stripe_in_one_domain() {
        let zones = localities(&["a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            true,
            |_| 0,
        );
        // One stripe per sector.
        let regions: Vec<LayoutRegion> = (0..9)
            .map(|s| region(s * 512..(s + 1) * 512, Some(s)))
            .collect();
        let layout = LayoutIndex::new(&regions);
        for stripe in 0..9 {
            let damaged: Vec<u8> = (1..=3)
                .filter(|n| plan.allows(ip(*n), Some(&layout), "wal/seg", stripe))
                .collect();
            assert_eq!(damaged.len(), 1, "stripe {stripe}: {damaged:?}");
        }
    }

    #[test]
    fn a_sector_holding_two_stripes_needs_both_allowed() {
        let zones = localities(&["a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            true,
            |_| 0,
        );
        // Stripes 0 and 1 share sector 0: no domain may damage it.
        let layout = LayoutIndex::new(&[region(0..256, Some(0)), region(256..512, Some(1))]);
        assert!((1..=3).all(|n| !plan.allows(ip(n), Some(&layout), "wal/seg", 0)));
    }

    #[test]
    fn node_local_bytes_are_damaged_only_in_the_drawn_domains() {
        let zones = localities(&["a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            true,
            |_| 1,
        );
        let layout = LayoutIndex::new(&[region(0..512, None)]);
        let damaged: Vec<u8> = (1..=3)
            .filter(|n| plan.allows(ip(*n), Some(&layout), "wal/seg", 0))
            .collect();
        assert_eq!(damaged, vec![2], "domain index 1 is zone b");
        // Bytes never published count as node-local too.
        assert!(plan.allows(ip(2), None, "other", 7));
        assert!(!plan.allows(ip(1), None, "other", 7));
    }

    #[test]
    fn a_minority_spares_every_other_domain() {
        let zones = localities(&["a", "a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone).tolerance(5),
            &zones,
            false,
            |_| 0,
        );
        let FaultPattern::Minority { domains } = plan.pattern() else {
            panic!("a minority");
        };
        assert_eq!(
            domains.len(),
            2,
            "tolerance is clamped below the domain count"
        );
        let spared = (1..=4)
            .filter(|n| !plan.allows(ip(*n), None, "x", 0))
            .count();
        assert!(spared >= 1, "one domain keeps every record");
    }

    #[test]
    fn one_domain_spares_every_process_in_it() {
        let zones = localities(&["a", "a"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            true,
            |_| 0,
        );
        assert_eq!(plan.pattern(), &FaultPattern::Spared);
        assert!(!plan.allows(ip(1), None, "x", 0));
        assert!(plan.allows(ip(9), None, "x", 0), "outside the topology");
    }
}
