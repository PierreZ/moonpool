//! Address allow lists: which peers may open a session at all.
//!
//! `FoundationDB`'s `IPAllowList` (`fdbrpc/IPAllowList.cpp`): a list of
//! subnets, where an **empty list allows every address**. An allow list
//! restricts reachability. It is not authentication: an address is not an
//! identity, and passing the list grants no access to a private endpoint.

use std::net::{IpAddr, SocketAddr};

use thiserror::Error;

/// A subnet: an address and a prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Subnet {
    address: IpAddr,
    prefix: u8,
}

/// A subnet that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid subnet {0:?}: expected ip or ip/prefix")]
pub struct InvalidSubnet(pub String);

impl Subnet {
    /// The subnet `address/prefix`; `None` when the prefix is longer than
    /// the address.
    #[must_use]
    pub fn new(address: IpAddr, prefix: u8) -> Option<Self> {
        let bits = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        (prefix <= bits).then_some(Self { address, prefix })
    }

    /// Parse `ip` (a single address) or `ip/prefix`.
    ///
    /// # Errors
    ///
    /// [`InvalidSubnet`] for anything else.
    pub fn parse(text: &str) -> Result<Self, InvalidSubnet> {
        let invalid = || InvalidSubnet(text.to_string());
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (
                address.trim().parse::<IpAddr>().map_err(|_| invalid())?,
                Some(prefix.trim().parse::<u8>().map_err(|_| invalid())?),
            ),
            None => (text.trim().parse::<IpAddr>().map_err(|_| invalid())?, None),
        };
        let full = if address.is_ipv4() { 32 } else { 128 };
        Self::new(address, prefix.unwrap_or(full)).ok_or_else(invalid)
    }

    /// Whether `ip` lies in this subnet. An IPv4 address never matches an
    /// IPv6 subnet or the reverse (IPv4-mapped IPv6 addresses are compared
    /// as the IPv4 address they map).
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        match (canonical(self.address), ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_matches(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_matches(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn prefix_matches(net: &[u8], ip: &[u8], prefix: u8) -> bool {
    let whole = usize::from(prefix / 8);
    let rest = prefix % 8;
    if net[..whole] != ip[..whole] {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = u8::MAX << (8 - rest);
    (net[whole] & mask) == (ip[whole] & mask)
}

/// The peers allowed to open a session: every address when empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpAllowList {
    subnets: Vec<Subnet>,
}

impl IpAllowList {
    /// An empty list: every address is allowed.
    #[must_use]
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Only the given subnets.
    #[must_use]
    pub fn of(subnets: impl IntoIterator<Item = Subnet>) -> Self {
        Self {
            subnets: subnets.into_iter().collect(),
        }
    }

    /// Parse a comma-separated list of `ip` or `ip/prefix` entries.
    ///
    /// # Errors
    ///
    /// The first entry that does not parse.
    pub fn parse(text: &str) -> Result<Self, InvalidSubnet> {
        text.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(Subnet::parse)
            .collect::<Result<Vec<_>, _>>()
            .map(|subnets| Self { subnets })
    }

    /// Whether the list is empty (allows every address).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subnets.is_empty()
    }

    /// Whether a peer at `ip` may connect.
    #[must_use]
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.subnets.is_empty() || self.subnets.iter().any(|subnet| subnet.contains(ip))
    }

    /// Whether a peer at transport address `peer` (`ip:port`) may connect.
    /// An address that does not parse is refused unless the list is empty:
    /// fail closed.
    #[must_use]
    pub fn allows_peer(&self, peer: &str) -> bool {
        self.subnets.is_empty()
            || peer
                .parse::<SocketAddr>()
                .is_ok_and(|address| self.allows(address.ip()))
    }
}

#[cfg(test)]
mod tests {
    use super::{IpAllowList, Subnet};

    #[test]
    fn subnets_match_by_prefix_and_family() {
        let net = Subnet::parse("10.1.0.0/16").expect("subnet");
        assert!(net.contains("10.1.200.3".parse().expect("ip")));
        assert!(!net.contains("10.2.0.1".parse().expect("ip")));
        assert!(!net.contains("::1".parse().expect("ip")));
        let odd = Subnet::parse("192.168.1.128/25").expect("subnet");
        assert!(odd.contains("192.168.1.200".parse().expect("ip")));
        assert!(!odd.contains("192.168.1.127".parse().expect("ip")));
        let host = Subnet::parse("::1").expect("subnet");
        assert!(host.contains("::1".parse().expect("ip")));
        assert!(!host.contains("::2".parse().expect("ip")));
        let mapped = Subnet::parse("10.0.0.0/8").expect("subnet");
        assert!(mapped.contains("::ffff:10.9.9.9".parse().expect("ip")));
        assert!(
            Subnet::parse("0.0.0.0/0")
                .is_ok_and(|any| any.contains("1.2.3.4".parse().expect("ip")))
        );
        for bad in ["10.0.0.0/33", "::/129", "nope", "10.0.0.0/x", ""] {
            assert!(Subnet::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(net.to_string(), "10.1.0.0/16");
    }

    #[test]
    fn an_empty_list_allows_everything_and_bad_addresses_fail_closed() {
        let all = IpAllowList::allow_all();
        assert!(all.allows_peer("garbage"));
        let list = IpAllowList::parse("10.0.1.0/24, 127.0.0.1").expect("list");
        assert!(list.allows_peer("10.0.1.9:4500"));
        assert!(list.allows_peer("127.0.0.1:1"));
        assert!(!list.allows_peer("10.0.2.1:4500"));
        assert!(!list.allows_peer("not-an-address"));
        assert!(IpAllowList::parse("10.0.0.0/8,bad").is_err());
        assert!(IpAllowList::parse("").is_ok_and(|list| list.is_empty()));
    }
}
