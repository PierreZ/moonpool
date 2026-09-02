//! Process groups: the independent `.processes()` / `.cluster()` registrations
//! of one builder, and the IP → group registry.
//!
//! A builder may register several server-process groups, one per **role** of the
//! system under test (acceptors and matchmakers, a primary tier and a spare
//! pool). Each group draws its own per-seed process count from its own
//! [`ProcessCount`](super::builder::ProcessCount), boots from its own factory,
//! and is named after its process type ([`Process::name`](super::process::Process::name)).
//!
//! Groups are laid out on disjoint IP ranges so a group's members are contiguous
//! and recognisable at a glance: the *g*-th group registered (0-based) owns
//! `10.0.{g + 1}.{1..=N}`. A single-group builder therefore keeps its historical
//! `10.0.1.{1..=N}` addresses, and workloads keep `10.0.0.{1..=N}`.
//!
//! Groups are **orthogonal to tags** ([`super::tags`]) and to locality
//! ([`super::locality`]): tags round-robin independent dimensions *within* a
//! group, and a `.cluster()` group carries its own failure-domain topology.

use std::collections::BTreeMap;
use std::net::IpAddr;

/// Registry mapping process IPs to the group they were registered in.
///
/// Parallel to [`TagRegistry`](super::tags::TagRegistry) and
/// [`MachineRegistry`](super::locality::MachineRegistry): built once per
/// iteration by the builder, cloned into every
/// [`WorkloadTopology`](super::topology::WorkloadTopology) and into the
/// [`FaultContext`](super::fault_injector::FaultContext).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupRegistry {
    /// Group names in registration order.
    names: Vec<String>,
    /// Each process IP's group, as an index into `names`.
    ip_group: BTreeMap<IpAddr, usize>,
}

impl GroupRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `group`, returning its index. Groups are ordered by first
    /// declaration, which the builder performs in `.processes()` /
    /// `.cluster()` call order.
    ///
    /// A group is listed even when it drew zero processes this seed (a
    /// `0..=3` count on a plain seed), so `groups()` describes the builder,
    /// not the draw.
    pub fn declare(&mut self, group: &str) -> usize {
        self.names
            .iter()
            .position(|name| name == group)
            .unwrap_or_else(|| {
                self.names.push(group.to_string());
                self.names.len() - 1
            })
    }

    /// Register `ip` as a member of `group`, declaring the group on first use.
    pub fn register(&mut self, ip: IpAddr, group: &str) {
        let index = self.declare(group);
        self.ip_group.insert(ip, index);
    }

    /// Whether any process has been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ip_group.is_empty()
    }

    /// All group names, in registration order.
    #[must_use]
    pub fn groups(&self) -> &[String] {
        &self.names
    }

    /// The group `ip` belongs to, if it is a registered process.
    #[must_use]
    pub fn group_for(&self, ip: IpAddr) -> Option<&str> {
        self.ip_group
            .get(&ip)
            .map(|&index| self.names[index].as_str())
    }

    /// Every IP registered in `group`, ascending (an unknown group is empty).
    ///
    /// Because a group owns one contiguous IP range, ascending order is also
    /// registration order within the group.
    #[must_use]
    pub fn ips_in_group(&self, group: &str) -> Vec<IpAddr> {
        let Some(wanted) = self.names.iter().position(|name| name == group) else {
            return Vec::new();
        };
        self.ip_group
            .iter()
            .filter(|&(_, &index)| index == wanted)
            .map(|(ip, _)| *ip)
            .collect()
    }

    /// `ip`'s `(index, size)` within its own group: its position among the
    /// group's members in ascending IP order, and the group's member count.
    #[must_use]
    pub fn position_in_group(&self, ip: IpAddr) -> Option<(usize, usize)> {
        let &wanted = self.ip_group.get(&ip)?;
        let mut index = 0;
        let mut size = 0;
        for (&member, &group) in &self.ip_group {
            if group != wanted {
                continue;
            }
            if member < ip {
                index += 1;
            }
            size += 1;
        }
        Some((index, size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(group: u8, n: u8) -> IpAddr {
        IpAddr::from([10, 0, group, n])
    }

    #[test]
    fn groups_keep_registration_order_and_membership() {
        let mut registry = GroupRegistry::new();
        registry.register(ip(1, 1), "acceptor");
        registry.register(ip(1, 2), "acceptor");
        registry.register(ip(2, 1), "matchmaker");
        registry.register(ip(1, 3), "acceptor");

        assert_eq!(registry.groups(), ["acceptor", "matchmaker"]);
        assert_eq!(
            registry.ips_in_group("acceptor"),
            vec![ip(1, 1), ip(1, 2), ip(1, 3)]
        );
        assert_eq!(registry.ips_in_group("matchmaker"), vec![ip(2, 1)]);
        assert!(registry.ips_in_group("spare").is_empty());
        assert_eq!(registry.group_for(ip(2, 1)), Some("matchmaker"));
        assert_eq!(registry.group_for(ip(3, 1)), None);
        assert!(!registry.is_empty());
    }

    #[test]
    fn an_empty_group_is_still_listed() {
        let mut registry = GroupRegistry::new();
        registry.declare("acceptor");
        registry.declare("matchmaker");
        registry.register(ip(1, 1), "acceptor");

        assert_eq!(registry.groups(), ["acceptor", "matchmaker"]);
        assert!(registry.ips_in_group("matchmaker").is_empty());
    }

    #[test]
    fn position_in_group_is_per_group() {
        let mut registry = GroupRegistry::new();
        registry.register(ip(1, 1), "acceptor");
        registry.register(ip(1, 2), "acceptor");
        registry.register(ip(2, 1), "matchmaker");
        registry.register(ip(2, 2), "matchmaker");
        registry.register(ip(2, 3), "matchmaker");

        assert_eq!(registry.position_in_group(ip(1, 2)), Some((1, 2)));
        assert_eq!(registry.position_in_group(ip(2, 1)), Some((0, 3)));
        assert_eq!(registry.position_in_group(ip(2, 3)), Some((2, 3)));
        assert_eq!(registry.position_in_group(ip(3, 1)), None);
    }
}
