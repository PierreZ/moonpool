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
        match address {
            IpAddr::V4(_) => (prefix <= 32).then_some(Self { address, prefix }),
            // An IPv4-mapped subnet whose prefix covers the mapping is the
            // IPv4 subnet it maps (its prefix counted in IPv4 bits).
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                _ if prefix > 128 => None,
                Some(v4) if prefix >= 96 => Some(Self {
                    address: IpAddr::V4(v4),
                    prefix: prefix - 96,
                }),
                _ => Some(Self { address, prefix }),
            },
        }
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
        match self.address {
            // An IPv4 subnet (prefix <= 32) matches IPv4 peers, mapped ones
            // included.
            IpAddr::V4(net) => match canonical(ip) {
                IpAddr::V4(ip) => prefix_matches(&net.octets(), &ip.octets(), self.prefix),
                IpAddr::V6(_) => false,
            },
            // An IPv6 subnet (prefix <= 128) compares 128-bit addresses; an
            // IPv4 peer is compared as its mapped form.
            IpAddr::V6(net) => {
                let ip = match ip {
                    IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                    IpAddr::V6(v6) => v6,
                };
                prefix_matches(&net.octets(), &ip.octets(), self.prefix)
            }
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

/// Whether the first `prefix` bits agree; a prefix beyond the addresses
/// never matches (and never panics).
fn prefix_matches(net: &[u8], ip: &[u8], prefix: u8) -> bool {
    let whole = usize::from(prefix / 8);
    let rest = prefix % 8;
    let (Some(net_head), Some(ip_head)) = (net.get(..whole), ip.get(..whole)) else {
        return false;
    };
    if net_head != ip_head {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = u8::MAX << (8 - rest);
    match (net.get(whole), ip.get(whole)) {
        (Some(net), Some(ip)) => (net & mask) == (ip & mask),
        _ => false,
    }
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

    /// Regression: `::ffff:10.0.0.0/104` was accepted and then compared
    /// IPv4 octets with a 104-bit prefix, panicking on every IPv4 accept.
    #[test]
    fn mapped_subnets_convert_their_prefix_and_nothing_panics() {
        let mapped = Subnet::parse("::ffff:10.0.0.0/104").expect("subnet");
        assert_eq!(mapped.to_string(), "10.0.0.0/8");
        assert!(mapped.contains("10.9.9.9".parse().expect("ip")));
        assert!(!mapped.contains("11.0.0.1".parse().expect("ip")));
        let wide = Subnet::parse("::ffff:0.0.0.0/80").expect("subnet");
        assert!(wide.contains("10.9.9.9".parse().expect("ip")));
        // Every prefix against every kind of peer: no panic, whatever the
        // answer.
        let nets = ["::ffff:10.0.0.0", "10.0.0.0", "2001:db8::", "::"];
        let peers = ["10.1.2.3", "::ffff:10.1.2.3", "2001:db8::1", "::1", "0.0.0.0"];
        for net in nets {
            for prefix in 0..=130u8 {
                let Ok(subnet) = Subnet::parse(&format!("{net}/{prefix}")) else {
                    continue;
                };
                for peer in peers {
                    let _ = subnet.contains(peer.parse().expect("ip"));
                    let list = IpAllowList::of([subnet]);
                    let _ = list.allows_peer(&format!("[{peer}]:1"));
                    let _ = list.allows_peer(&format!("{peer}:1"));
                }
            }
        }
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
