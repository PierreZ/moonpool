//! Replicated fault patterns: random storage damage spread over failure
//! domains so that no replicated record is damaged everywhere.
//!
//! A replicated system survives disk damage only while some replica keeps
//! a clean copy of every record. Random faults, drawn independently per
//! disk, eventually hit every copy of one record and turn a valid run into
//! an unwinnable one. So the simulator, which knows the topology, decides
//! *where* damage may land, from what each process publishes about its own
//! layout ([`LayoutRegion`]s with a `stripe`, the record's key shared by
//! every replica's copy). One pattern covers one process group (or the
//! whole topology), and each seed draws one of:
//!
//! - **minority**: only the processes of a few failure domains take damage,
//!   anywhere on their disks — `TigerBeetle`'s minority corruption;
//! - **striped**: every domain takes damage, but each stripe only in the
//!   domain its key rotates to — `TigerBeetle`'s helical corruption, keyed
//!   by record instead of by file offset, so it holds even where replicas
//!   lay their records out differently. Bytes a node alone holds (stripe
//!   `None`, or bytes it never published) are damaged only in the
//!   `tolerance - 1` domains drawn for them, and those domains may damage
//!   every stripe too: a format may answer node-local damage by giving up
//!   the whole replica, so node-local damage counts against every stripe's
//!   tolerance;
//! - **rolling**: one domain at a time takes damage, anywhere on its disks.
//!   The turn stays on a domain while it holds damage, and moves to the
//!   next domain in order once that damage is repaired, read off the disk:
//!   every damaged sector rewritten, truncated or deleted, or outside every
//!   region its process publishes. A domain holding no damage passes the
//!   turn on as soon as another domain's disk is up for a fault.
//!
//! In each, a record's copies hold damage in at most `tolerance` domains at
//! once, and so does node-local data: by construction, with no ledger.

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
/// `level` of one process group (or, without [`group`](Self::group), the
/// whole topology). A record (a stripe) holds damage in at most
/// `tolerance` domains at once, and so does node-local data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicatedFaults {
    level: DomainLevel,
    tolerance: usize,
    group: Option<&'static str>,
}

impl ReplicatedFaults {
    /// Spread damage over the domains at `level`, a record holding damage
    /// in at most one of them at once.
    #[must_use]
    pub fn new(level: DomainLevel) -> Self {
        Self {
            level,
            tolerance: 1,
            group: None,
        }
    }

    /// How many domains may hold damage to one record at once (at least
    /// 1). It is clamped below the number of domains a seed draws, so one
    /// domain always keeps every record.
    #[must_use]
    pub fn tolerance(mut self, domains: usize) -> Self {
        self.tolerance = domains.max(1);
        self
    }

    /// Cover only the process group `name` (its processes'
    /// [`Process::name`](crate::Process::name)): its own replicated system,
    /// with its own pattern, turn and records. Processes outside every
    /// covered group keep plain storage chaos.
    #[must_use]
    pub fn group(mut self, name: &'static str) -> Self {
        self.group = Some(name);
        self
    }

    /// The failure-domain level damage is spread over.
    #[must_use]
    pub fn level(&self) -> DomainLevel {
        self.level
    }

    /// The process group covered, or `None` for the whole topology.
    #[must_use]
    pub fn group_name(&self) -> Option<&'static str> {
        self.group
    }
}

/// The pattern one seed drew for one group (see [`ReplicatedFaults`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultPattern {
    /// Only these domains take damage.
    Minority {
        /// The damaged domains' ids.
        domains: Vec<String>,
    },
    /// Every domain takes damage on the stripes its key rotates to; the
    /// `local` domains take damage anywhere.
    ///
    /// Node-local damage may cost a node its whole replica (a format that
    /// refuses to open on it), which damages every stripe it holds. So a
    /// domain allowed node-local damage counts as damaged for every stripe,
    /// and stripe `s` is damaged only in `local` plus the one domain it
    /// rotates to: at most `tolerance` domains. At `tolerance = 1`, `local`
    /// is empty and node-local bytes are never damaged under this pattern.
    Striped {
        /// The domains, in order. Stripe `s` may be damaged in the
        /// `s mod n`-th domain outside `local`, with `n` their count.
        domains: Vec<String>,
        /// The `tolerance - 1` domains whose bytes, node-local or striped,
        /// may all be damaged.
        local: Vec<String>,
    },
    /// One window of `tolerance` domains at a time takes damage; the
    /// window moves on once its damage is repaired.
    Rolling {
        /// The domains, in turn order.
        domains: Vec<String>,
    },
    /// The group has fewer than two domains: no placement keeps a clean
    /// copy elsewhere, so no process in it takes damage.
    Spared,
}

/// Which pattern a draw makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatternKind {
    Minority,
    Striped,
    Rolling,
}

impl PatternKind {
    /// The kind numbered `n` of three.
    pub(crate) fn nth(n: usize) -> Self {
        match n % 3 {
            0 => Self::Minority,
            1 => Self::Striped,
            _ => Self::Rolling,
        }
    }
}

/// One seed's plan for one group: the pattern, and each member's domain.
#[derive(Debug, Clone)]
pub(crate) struct ReplicationPlan {
    pattern: FaultPattern,
    tolerance: usize,
    /// Domain index (into the pattern's rotation) per member.
    domain_of: BTreeMap<IpAddr, usize>,
    /// For a minority: whether each domain index is damaged. For a stripe
    /// rotation: whether each domain index may be damaged anywhere.
    marked: Vec<bool>,
}

impl ReplicationPlan {
    /// Draw a plan over `localities` (the group's members) with `pick(n)`
    /// returning a uniform index below `n` (the simulation stream). With
    /// fewer than two domains the plan is [`FaultPattern::Spared`].
    pub(crate) fn draw(
        config: ReplicatedFaults,
        localities: &BTreeMap<IpAddr, LocalityInfo>,
        kind: PatternKind,
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
        let mut marked = vec![false; domains.len()];
        let pattern = if kind == PatternKind::Rolling {
            FaultPattern::Rolling { domains }
        } else {
            // Distinct domains, drawn without replacement: `tolerance` for a
            // minority, one fewer for a stripe rotation, whose stripes each
            // take the last domain of their tolerance.
            let picks = if kind == PatternKind::Striped {
                tolerance - 1
            } else {
                tolerance
            };
            let mut pool: Vec<usize> = (0..domains.len()).collect();
            for _ in 0..picks {
                let at = pick(pool.len());
                marked[pool.swap_remove(at)] = true;
            }
            let chosen: Vec<String> = domains
                .iter()
                .zip(&marked)
                .filter(|(_, marked)| **marked)
                .map(|(id, _)| id.clone())
                .collect();
            if kind == PatternKind::Striped {
                FaultPattern::Striped {
                    domains,
                    local: chosen,
                }
            } else {
                FaultPattern::Minority { domains: chosen }
            }
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

    pub(crate) fn is_rolling(&self) -> bool {
        matches!(self.pattern, FaultPattern::Rolling { .. })
    }

    /// `ip`'s domain index, if it is a member.
    pub(crate) fn domain(&self, ip: IpAddr) -> Option<usize> {
        self.domain_of.get(&ip).copied()
    }

    pub(crate) fn domain_count(&self) -> usize {
        self.marked.len()
    }

    /// Whether `domain` is among the `tolerance` domains starting at
    /// `first` in rotation order.
    fn within(&self, domain: usize, first: u64) -> bool {
        let count = self.domain_count() as u64;
        let offset = (domain as u64 + count - first % count) % count;
        offset < self.tolerance as u64
    }

    /// Whether `stripe` rotates to `domain`: the `stripe mod n`-th domain
    /// outside the marked ones, with `n` their count.
    fn rotates_to(&self, domain: usize, stripe: u64) -> bool {
        if self.marked[domain] {
            return false;
        }
        let rotation = self.marked.iter().filter(|marked| !**marked).count() as u64;
        let rank = self.marked[..domain].iter().filter(|marked| !**marked).count() as u64;
        stripe % rotation == rank
    }

    /// Whether `domain` holds the rolling turn when it starts at `turn`.
    pub(crate) fn in_window(&self, domain: usize, turn: usize) -> bool {
        self.within(domain, turn as u64)
    }

    /// The ids of the domains holding the turn when it starts at `turn`.
    pub(crate) fn window_ids(&self, turn: usize) -> Vec<String> {
        let FaultPattern::Rolling { domains } = &self.pattern else {
            return Vec::new();
        };
        (0..domains.len())
            .filter(|domain| self.in_window(*domain, turn))
            .map(|domain| domains[domain].clone())
            .collect()
    }

    /// Whether a random fault may damage `sector` of `owner`'s file at
    /// `path`, given what `owner` published and, for a rolling pattern,
    /// where the turn starts. A process outside the group is not
    /// constrained.
    pub(crate) fn allows(
        &self,
        owner: IpAddr,
        layout: Option<&LayoutIndex>,
        path: &str,
        sector: u64,
        turn: usize,
    ) -> bool {
        let Some(&domain) = self.domain_of.get(&owner) else {
            return true;
        };
        match &self.pattern {
            FaultPattern::Spared => false,
            FaultPattern::Minority { .. } => self.marked[domain],
            FaultPattern::Rolling { .. } => self.in_window(domain, turn),
            FaultPattern::Striped { .. } => {
                if self.marked[domain] {
                    // Counted as damaged for every stripe already.
                    return true;
                }
                let mut stripes = layout
                    .map(|layout| layout.stripes(path, sector))
                    .unwrap_or_default();
                if stripes.is_empty() || stripes.contains(&None) {
                    // Bytes this node alone holds, or never described.
                    return false;
                }
                stripes.dedup();
                stripes
                    .into_iter()
                    .flatten()
                    .all(|stripe| self.rotates_to(domain, stripe))
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

    /// Whether any published region shares a byte with `sector`.
    pub(crate) fn covers(&self, path: &str, sector: u64) -> bool {
        !self.stripes(path, sector).is_empty()
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
    fn stripes_rotate_one_domain_each() {
        let zones = localities(&["a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            PatternKind::Striped,
            |_| 0,
        );
        // One stripe per sector.
        let regions: Vec<LayoutRegion> = (0..9)
            .map(|s| region(s * 512..(s + 1) * 512, Some(s)))
            .collect();
        let layout = LayoutIndex::new(&regions);
        for stripe in 0..9 {
            let damaged: Vec<u8> = (1..=3)
                .filter(|n| plan.allows(ip(*n), Some(&layout), "wal/seg", stripe, 0))
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
            PatternKind::Striped,
            |_| 0,
        );
        // Stripes 0 and 1 share sector 0: no domain may damage it.
        let layout = LayoutIndex::new(&[region(0..256, Some(0)), region(256..512, Some(1))]);
        assert!((1..=3).all(|n| !plan.allows(ip(n), Some(&layout), "wal/seg", 0, 0)));
    }

    #[test]
    fn node_local_bytes_are_damaged_only_in_the_drawn_domains() {
        let zones = localities(&["a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone).tolerance(2),
            &zones,
            PatternKind::Striped,
            |_| 1,
        );
        let layout = LayoutIndex::new(&[region(0..512, None)]);
        let damaged: Vec<u8> = (1..=3)
            .filter(|n| plan.allows(ip(*n), Some(&layout), "wal/seg", 0, 0))
            .collect();
        assert_eq!(damaged, vec![2], "domain index 1 is zone b");
        // Bytes never published count as node-local too.
        assert!(plan.allows(ip(2), None, "other", 7, 0));
        assert!(!plan.allows(ip(1), None, "other", 7, 0));
    }

    #[test]
    fn at_tolerance_one_no_domain_takes_node_local_damage() {
        let zones = localities(&["a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            PatternKind::Striped,
            |_| 0,
        );
        let FaultPattern::Striped { local, .. } = plan.pattern() else {
            panic!("a stripe rotation");
        };
        assert!(local.is_empty());
        let layout = LayoutIndex::new(&[region(0..512, None)]);
        assert!((1..=3).all(|n| !plan.allows(ip(n), Some(&layout), "wal/seg", 0, 0)));
    }

    #[test]
    fn a_node_local_domain_counts_against_every_stripe() {
        let zones = localities(&["a", "b", "c", "d"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone).tolerance(2),
            &zones,
            PatternKind::Striped,
            |_| 0,
        );
        let regions: Vec<LayoutRegion> = (0..12)
            .map(|s| region(s * 512..(s + 1) * 512, Some(s)))
            .collect();
        let layout = LayoutIndex::new(&regions);
        for stripe in 0..12 {
            let damaged: Vec<u8> = (1..=4)
                .filter(|n| plan.allows(ip(*n), Some(&layout), "wal/seg", stripe, 0))
                .collect();
            assert_eq!(damaged.len(), 2, "stripe {stripe}: {damaged:?}");
            assert!(damaged.contains(&1), "zone a is node-local: {damaged:?}");
        }
    }

    #[test]
    fn a_minority_spares_every_other_domain() {
        let zones = localities(&["a", "a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone).tolerance(5),
            &zones,
            PatternKind::Minority,
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
            .filter(|n| !plan.allows(ip(*n), None, "x", 0, 0))
            .count();
        assert!(spared >= 1, "one domain keeps every record");
    }

    #[test]
    fn one_domain_spares_every_process_in_it() {
        let zones = localities(&["a", "a"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            PatternKind::Striped,
            |_| 0,
        );
        assert_eq!(plan.pattern(), &FaultPattern::Spared);
        assert!(!plan.allows(ip(1), None, "x", 0, 0));
        assert!(plan.allows(ip(9), None, "x", 0, 0), "outside the topology");
    }

    #[test]
    fn a_rolling_turn_damages_one_domain_anywhere() {
        let zones = localities(&["a", "b", "c"]);
        let plan = ReplicationPlan::draw(
            ReplicatedFaults::new(DomainLevel::Zone),
            &zones,
            PatternKind::Rolling,
            |_| 0,
        );
        let layout = LayoutIndex::new(&[region(0..512, Some(4)), region(512..1024, None)]);
        for turn in 0..3 {
            for sector in 0..2 {
                let damaged: Vec<u8> = (1..=3)
                    .filter(|n| plan.allows(ip(*n), Some(&layout), "wal/seg", sector, turn))
                    .collect();
                let expected = u8::try_from(turn + 1).expect("small");
                assert_eq!(damaged, vec![expected], "turn {turn}, sector {sector}");
            }
        }
        assert_eq!(plan.window_ids(4), vec!["b".to_string()], "the turn wraps");
    }
}
