//! The live membership of one iteration: which processes exist now.
//!
//! The builder draws the boot shape of every group per seed. A workload can
//! then change it in the middle of the run through the
//! [`Deployment`](super::deployment::Deployment) handle: spawn a process in a
//! group, start a new group, or retire a process for good. [`Membership`] is
//! the one shared value that records those changes. Every
//! [`WorkloadTopology`](super::topology::WorkloadTopology), every
//! [`FaultContext`](super::fault_injector::FaultContext) and the process
//! manager read it live, so topology queries, attrition, outages, gray
//! failure and locality see a new member as soon as it boots.
//!
//! IPs are never reused: a retired IP stays dead, and a group's next spawn
//! takes the next free host of its `/24`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::locality::{DomainLevel, LocalityInfo};
use crate::{SimulationError, SimulationResult};

use super::groups::GroupRegistry;
use super::locality::MachineRegistry;
use super::tags::{ProcessTags, TagDistribution, TagRegistry};

/// The highest host octet a spawn may take in a group's `/24`.
const MAX_HOST: usize = 254;

/// Where a spawned process goes in a [`.cluster()`](crate::SimulationBuilder::cluster)
/// group's failure-domain topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainSpec {
    /// Co-locate on an existing machine (by machine id, e.g. `dc1-z1-m1`).
    Machine(String),
    /// A new machine in an existing zone (by zone id, e.g. `dc1-z1`).
    NewMachineIn {
        /// The zone id.
        zone: String,
    },
    /// A new zone, with one new machine, in an existing datacenter.
    NewZoneIn {
        /// The datacenter id (e.g. `dc1`).
        datacenter: String,
    },
    /// A new datacenter, with one new zone and one new machine.
    NewDatacenter,
}

/// What the membership knows about one group beyond its members: how to tag
/// and place the next one.
#[derive(Debug, Clone, Default)]
pub(crate) struct GroupPlan {
    /// The group's round-robin tags; a spawn takes the tags of its host index.
    pub(crate) tags: TagDistribution,
    /// Whether the group is a `.cluster()` group (its members carry a locality).
    pub(crate) cluster: bool,
    /// The next free host octet of the group's `/24`, 1-based.
    pub(crate) next_host: usize,
}

/// Spawned and retired processes of one iteration, by group, in call order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeploymentChanges {
    /// Processes spawned mid-run, as `group -> IPs`.
    pub spawned: BTreeMap<String, Vec<String>>,
    /// Processes retired mid-run, as `group -> IPs`.
    pub retired: BTreeMap<String, Vec<String>>,
}

impl DeploymentChanges {
    /// Whether the run changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spawned.is_empty() && self.retired.is_empty()
    }

    /// How many processes were spawned, all groups together.
    #[must_use]
    pub fn spawned_count(&self) -> usize {
        self.spawned.values().map(Vec::len).sum()
    }

    /// How many processes were retired, all groups together.
    #[must_use]
    pub fn retired_count(&self) -> usize {
        self.retired.values().map(Vec::len).sum()
    }
}

/// A spawn reserved by [`Membership::reserve`], booted by its event.
#[derive(Debug, Clone)]
struct PendingSpawn {
    group: String,
    tags: ProcessTags,
    locality: Option<LocalityInfo>,
}

#[derive(Debug, Default)]
struct MembershipInner {
    /// +1 on every spawn that boots and every retire.
    version: u64,
    groups: GroupRegistry,
    tags: TagRegistry,
    machines: MachineRegistry,
    plans: BTreeMap<String, GroupPlan>,
    /// Every live process (booted, not retired), boot order then spawn order.
    process_ips: Vec<IpAddr>,
    /// Reserved by a spawn call, not yet booted by its event.
    pending: BTreeMap<IpAddr, PendingSpawn>,
    /// Every process spawned mid-run, live or retired.
    spawned: BTreeSet<IpAddr>,
    /// Every retired process, with its group.
    retired: BTreeMap<IpAddr, String>,
    /// Every locality ever placed, so a new domain id is never reused.
    placed: Vec<LocalityInfo>,
    changes: DeploymentChanges,
    /// Spawns still allowed in this run.
    spawns_left: usize,
}

/// The run's live process membership, shared by every context of one
/// iteration. Cheap to clone: every clone sees the same state.
#[derive(Debug, Clone, Default)]
pub struct Membership(Arc<RwLock<MembershipInner>>);

impl Membership {
    /// The membership of a run that boots `process_ips` with these
    /// registries, and allows `max_spawns` spawns.
    pub(crate) fn new(
        groups: GroupRegistry,
        tags: TagRegistry,
        machines: MachineRegistry,
        plans: BTreeMap<String, GroupPlan>,
        process_ips: Vec<IpAddr>,
        max_spawns: usize,
    ) -> Self {
        let placed = process_ips
            .iter()
            .filter_map(|ip| machines.locality_for(*ip).cloned())
            .collect();
        Self(Arc::new(RwLock::new(MembershipInner {
            groups,
            tags,
            machines,
            plans,
            process_ips,
            placed,
            spawns_left: max_spawns,
            ..MembershipInner::default()
        })))
    }

    fn read(&self) -> RwLockReadGuard<'_, MembershipInner> {
        self.0.read().expect("RwLock poisoned: prior task panicked")
    }

    fn write(&self) -> RwLockWriteGuard<'_, MembershipInner> {
        self.0
            .write()
            .expect("RwLock poisoned: prior task panicked")
    }

    /// The membership version: +1 on every spawned process that boots and
    /// every retire. A harness compares it to notice a change.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.read().version
    }

    /// Every live process IP, boot order then spawn order.
    #[must_use]
    pub fn process_ips(&self) -> Vec<String> {
        self.read()
            .process_ips
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Every group name, in registration order (groups started by
    /// [`Deployment::spawn_group`](super::deployment::Deployment::spawn_group)
    /// come last).
    #[must_use]
    pub fn groups(&self) -> Vec<String> {
        self.read().groups.groups().to_vec()
    }

    /// The live members of `group`, ascending.
    #[must_use]
    pub fn ips_in_group(&self, group: &str) -> Vec<IpAddr> {
        self.read().groups.ips_in_group(group)
    }

    /// The group of the live process at `ip`.
    #[must_use]
    pub fn group_for(&self, ip: IpAddr) -> Option<String> {
        self.read().groups.group_for(ip).map(str::to_string)
    }

    /// `ip`'s `(rank, size)` in its group now (see
    /// [`GroupRegistry::position_in_group`]).
    #[must_use]
    pub fn position_in_group(&self, ip: IpAddr) -> Option<(usize, usize)> {
        self.read().groups.position_in_group(ip)
    }

    /// The tags of the live process at `ip`.
    #[must_use]
    pub fn tags_for(&self, ip: IpAddr) -> Option<ProcessTags> {
        self.read().tags.tags_for(ip).cloned()
    }

    /// The live processes tagged `key=value`, ascending.
    #[must_use]
    pub fn ips_tagged(&self, key: &str, value: &str) -> Vec<IpAddr> {
        self.read().tags.ips_tagged(key, value)
    }

    /// The locality of the live process at `ip`.
    #[must_use]
    pub fn locality_for(&self, ip: IpAddr) -> Option<LocalityInfo> {
        self.read().machines.locality_for(ip).cloned()
    }

    /// A copy of the live group registry.
    #[must_use]
    pub fn group_registry(&self) -> GroupRegistry {
        self.read().groups.clone()
    }

    /// A copy of the live tag registry.
    #[must_use]
    pub fn tag_registry(&self) -> TagRegistry {
        self.read().tags.clone()
    }

    /// A copy of the live machine registry.
    #[must_use]
    pub fn machine_registry(&self) -> MachineRegistry {
        self.read().machines.clone()
    }

    /// Whether `ip` was retired: it never runs again.
    #[must_use]
    pub fn is_retired(&self, ip: IpAddr) -> bool {
        self.read().retired.contains_key(&ip)
    }

    /// Whether `ip` was spawned in the middle of the run.
    #[must_use]
    pub fn was_spawned(&self, ip: IpAddr) -> bool {
        self.read().spawned.contains(&ip)
    }

    /// The spawns and retires of this run so far.
    #[must_use]
    pub fn changes(&self) -> DeploymentChanges {
        self.read().changes.clone()
    }

    /// Declare a new group for `spawn_group`.
    pub(crate) fn declare_group(&self, name: &str) -> SimulationResult<()> {
        let mut inner = self.write();
        if inner.groups.index_of(name).is_some() {
            return Err(SimulationError::InvalidState(format!(
                "spawn_group: group '{name}' already exists"
            )));
        }
        if inner.groups.groups().len() >= 255 {
            return Err(SimulationError::InvalidState(
                "spawn_group: at most 255 process groups fit the 10.0.{group}.x address plan"
                    .to_string(),
            ));
        }
        inner.groups.declare(name);
        inner.plans.insert(
            name.to_string(),
            GroupPlan {
                next_host: 1,
                ..GroupPlan::default()
            },
        );
        Ok(())
    }

    /// Reserve the next IP of `group` for a spawn, with its tags and
    /// placement. The IP is taken now, so two spawns never collide; the
    /// process joins the live set when its spawn event boots it
    /// ([`Self::activate`]).
    pub(crate) fn reserve(
        &self,
        group: &str,
        domain: Option<DomainSpec>,
    ) -> SimulationResult<IpAddr> {
        let mut inner = self.write();
        let Some(index) = inner.groups.index_of(group) else {
            return Err(SimulationError::InvalidState(format!(
                "spawn: unknown process group '{group}'"
            )));
        };
        if inner.spawns_left == 0 {
            return Err(SimulationError::InvalidState(
                "spawn: the run's max_spawns cap is reached".to_string(),
            ));
        }
        let plan = inner.plans.get(group).cloned().unwrap_or_default();
        if plan.next_host > MAX_HOST {
            return Err(SimulationError::InvalidState(format!(
                "spawn: group '{group}' has no free IP left in its /24"
            )));
        }
        let locality = match (plan.cluster, domain) {
            (false, None) => None,
            (false, Some(_)) => {
                return Err(SimulationError::InvalidState(format!(
                    "spawn_in: group '{group}' has no failure-domain topology (use .cluster())"
                )));
            }
            (true, domain) => Some(inner.place(group, domain)?),
        };
        let host = u8::try_from(plan.next_host).expect("host octet checked against MAX_HOST");
        let group_octet = u8::try_from(index + 1).expect("at most 255 groups");
        let ip = IpAddr::from([10, 0, group_octet, host]);
        let tags = plan.tags.resolve(plan.next_host - 1);
        if let Some(plan) = inner.plans.get_mut(group) {
            plan.next_host += 1;
        }
        inner.spawns_left -= 1;
        if let Some(locality) = &locality {
            inner.placed.push(locality.clone());
        }
        inner.pending.insert(
            ip,
            PendingSpawn {
                group: group.to_string(),
                tags,
                locality,
            },
        );
        Ok(ip)
    }

    /// Boot-time half of a spawn: `ip` joins the live set. Returns its group,
    /// or `None` when `ip` was never reserved (or already activated).
    pub(crate) fn activate(&self, ip: IpAddr) -> Option<String> {
        let mut inner = self.write();
        let pending = inner.pending.remove(&ip)?;
        inner.groups.register(ip, &pending.group);
        inner.tags.register(ip, pending.tags);
        if let Some(locality) = pending.locality {
            inner.machines.register(ip, locality);
        }
        inner.process_ips.push(ip);
        inner.spawned.insert(ip);
        inner.version += 1;
        inner
            .changes
            .spawned
            .entry(pending.group.clone())
            .or_default()
            .push(ip.to_string());
        Some(pending.group)
    }

    /// Retire the live process at `ip`: it leaves every registry and victim
    /// pool now, and never comes back. Returns its group.
    pub(crate) fn retire(&self, ip: IpAddr) -> SimulationResult<String> {
        let mut inner = self.write();
        if inner.retired.contains_key(&ip) {
            return Err(SimulationError::InvalidState(format!(
                "retire: {ip} is already retired"
            )));
        }
        let Some(group) = inner.groups.group_for(ip).map(str::to_string) else {
            return Err(SimulationError::InvalidState(format!(
                "retire: {ip} is not a live process"
            )));
        };
        inner.groups.remove(ip);
        inner.tags.remove(ip);
        inner.machines.remove(ip);
        inner.process_ips.retain(|live| *live != ip);
        inner.retired.insert(ip, group.clone());
        inner.version += 1;
        inner
            .changes
            .retired
            .entry(group.clone())
            .or_default()
            .push(ip.to_string());
        Ok(group)
    }
}

impl MembershipInner {
    /// The locality of the next process of the cluster group `group`.
    ///
    /// Without a `domain`, a spawn gets a new machine in the zone of the
    /// group's highest live member, or a new datacenter when the group is
    /// empty.
    fn place(&self, group: &str, domain: Option<DomainSpec>) -> SimulationResult<LocalityInfo> {
        let domain = domain.unwrap_or_else(|| {
            self.groups
                .ips_in_group(group)
                .last()
                .and_then(|ip| self.machines.locality_for(*ip))
                .map_or(DomainSpec::NewDatacenter, |locality| {
                    DomainSpec::NewMachineIn {
                        zone: locality.zone().to_string(),
                    }
                })
        });
        match domain {
            DomainSpec::Machine(machine) => self
                .placed
                .iter()
                .find(|locality| locality.machine() == machine)
                .cloned()
                .ok_or_else(|| {
                    SimulationError::InvalidState(format!("spawn_in: unknown machine '{machine}'"))
                }),
            DomainSpec::NewMachineIn { zone } => {
                let Some(datacenter) = self
                    .placed
                    .iter()
                    .find(|locality| locality.zone() == zone)
                    .map(|locality| locality.datacenter().to_string())
                else {
                    return Err(SimulationError::InvalidState(format!(
                        "spawn_in: unknown zone '{zone}'"
                    )));
                };
                let machine = self.next_id(DomainLevel::Machine, &format!("{zone}-m"));
                Ok(LocalityInfo::new(datacenter, zone, machine))
            }
            DomainSpec::NewZoneIn { datacenter } => {
                if !self
                    .placed
                    .iter()
                    .any(|locality| locality.datacenter() == datacenter)
                {
                    return Err(SimulationError::InvalidState(format!(
                        "spawn_in: unknown datacenter '{datacenter}'"
                    )));
                }
                let zone = self.next_id(DomainLevel::Zone, &format!("{datacenter}-z"));
                let machine = format!("{zone}-m1");
                Ok(LocalityInfo::new(datacenter, zone, machine))
            }
            DomainSpec::NewDatacenter => {
                let datacenter = self.next_id(DomainLevel::Datacenter, "dc");
                let zone = format!("{datacenter}-z1");
                let machine = format!("{zone}-m1");
                Ok(LocalityInfo::new(datacenter, zone, machine))
            }
        }
    }

    /// `{prefix}{n}` for the first `n >= 1` no placed domain at `level` uses.
    fn next_id(&self, level: DomainLevel, prefix: &str) -> String {
        let used: BTreeSet<&str> = self
            .placed
            .iter()
            .map(|locality| locality.id_for(level))
            .collect();
        // `placed` holds fewer ids than `placed.len() + 1`, so one of the
        // first `placed.len() + 1` numbers is free.
        (1..=self.placed.len() + 1)
            .map(|n| format!("{prefix}{n}"))
            .find(|id| !used.contains(id.as_str()))
            .unwrap_or_else(|| format!("{prefix}{}", self.placed.len() + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(group: u8, n: u8) -> IpAddr {
        IpAddr::from([10, 0, group, n])
    }

    fn plain(members: usize) -> Membership {
        let mut groups = GroupRegistry::new();
        let mut ips = Vec::new();
        for n in 0..members {
            let host = u8::try_from(n + 1).expect("small");
            groups.register(ip(1, host), "server");
            ips.push(ip(1, host));
        }
        let mut plans = BTreeMap::new();
        plans.insert(
            "server".to_string(),
            GroupPlan {
                next_host: members + 1,
                ..GroupPlan::default()
            },
        );
        Membership::new(
            groups,
            TagRegistry::new(),
            MachineRegistry::new(),
            plans,
            ips,
            16,
        )
    }

    #[test]
    fn a_spawn_takes_the_next_host_and_joins_at_activation() {
        let membership = plain(2);
        let spawned = membership.reserve("server", None).expect("reserve");
        assert_eq!(spawned, ip(1, 3));
        assert_eq!(membership.ips_in_group("server").len(), 2);
        assert_eq!(membership.version(), 0);
        assert_eq!(membership.activate(spawned).as_deref(), Some("server"));
        assert_eq!(membership.ips_in_group("server").len(), 3);
        assert_eq!(membership.position_in_group(spawned), Some((2, 3)));
        assert_eq!(membership.version(), 1);
        assert!(membership.was_spawned(spawned));
    }

    #[test]
    fn a_retired_ip_is_never_reused() {
        let membership = plain(2);
        membership.retire(ip(1, 2)).expect("retire");
        assert!(membership.is_retired(ip(1, 2)));
        assert_eq!(membership.ips_in_group("server"), vec![ip(1, 1)]);
        assert!(membership.retire(ip(1, 2)).is_err());
        assert_eq!(membership.reserve("server", None).ok(), Some(ip(1, 3)));
    }

    #[test]
    fn spawns_stop_at_the_cap() {
        let mut groups = GroupRegistry::new();
        groups.declare("server");
        let mut plans = BTreeMap::new();
        plans.insert(
            "server".to_string(),
            GroupPlan {
                next_host: 1,
                ..GroupPlan::default()
            },
        );
        let membership = Membership::new(
            groups,
            TagRegistry::new(),
            MachineRegistry::new(),
            plans,
            Vec::new(),
            1,
        );
        assert!(membership.reserve("server", None).is_ok());
        assert!(membership.reserve("server", None).is_err());
    }

    #[test]
    fn a_new_group_takes_the_next_range() {
        let membership = plain(1);
        membership.declare_group("spare").expect("declare");
        assert!(membership.declare_group("spare").is_err());
        assert_eq!(membership.reserve("spare", None).ok(), Some(ip(2, 1)));
        assert_eq!(membership.reserve("spare", None).ok(), Some(ip(2, 2)));
    }

    #[test]
    fn cluster_placement_allocates_fresh_domains() {
        let mut groups = GroupRegistry::new();
        let mut machines = MachineRegistry::new();
        groups.register(ip(1, 1), "node");
        machines.register(ip(1, 1), LocalityInfo::new("dc1", "dc1-z1", "dc1-z1-m1"));
        let mut plans = BTreeMap::new();
        plans.insert(
            "node".to_string(),
            GroupPlan {
                cluster: true,
                next_host: 2,
                ..GroupPlan::default()
            },
        );
        let membership = Membership::new(
            groups,
            TagRegistry::new(),
            machines,
            plans,
            vec![ip(1, 1)],
            16,
        );
        let a = membership.reserve("node", None).expect("default placement");
        membership.activate(a);
        assert_eq!(
            membership.locality_for(a),
            Some(LocalityInfo::new("dc1", "dc1-z1", "dc1-z1-m2"))
        );
        let b = membership
            .reserve(
                "node",
                Some(DomainSpec::NewZoneIn {
                    datacenter: "dc1".to_string(),
                }),
            )
            .expect("new zone");
        membership.activate(b);
        assert_eq!(
            membership.locality_for(b),
            Some(LocalityInfo::new("dc1", "dc1-z2", "dc1-z2-m1"))
        );
        let c = membership
            .reserve("node", Some(DomainSpec::NewDatacenter))
            .expect("new dc");
        membership.activate(c);
        assert_eq!(
            membership.locality_for(c),
            Some(LocalityInfo::new("dc2", "dc2-z1", "dc2-z1-m1"))
        );
        let d = membership
            .reserve("node", Some(DomainSpec::Machine("dc1-z1-m1".to_string())))
            .expect("co-locate");
        membership.activate(d);
        assert_eq!(
            membership.locality_for(d),
            Some(LocalityInfo::new("dc1", "dc1-z1", "dc1-z1-m1"))
        );
        assert!(
            membership
                .reserve(
                    "node",
                    Some(DomainSpec::NewMachineIn {
                        zone: "dc9-z1".to_string(),
                    })
                )
                .is_err()
        );
    }
}
