//! Generic locality descriptors and the distance ranking between them
//! (`FoundationDB`'s `loadBalanceDistance`, without its `LocalityData`
//! schema or any simulation topology type).

/// Where a caller or an alternative runs, as far as balancing cares.
///
/// Both identifiers are opaque application strings (a host id, a rack, a
/// zone, a datacenter name). A missing identifier never matches anything,
/// so an alternative with no locality is always [`Distance::Distant`].
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Locality {
    /// The machine (or zone) identifier.
    pub machine: Option<String>,
    /// The datacenter identifier.
    pub datacenter: Option<String>,
}

impl Locality {
    /// No locality information: every distance is [`Distance::Distant`].
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            machine: None,
            datacenter: None,
        }
    }

    /// A locality in `datacenter` on `machine`.
    #[must_use]
    pub fn new(machine: impl Into<String>, datacenter: impl Into<String>) -> Self {
        Self {
            machine: Some(machine.into()),
            datacenter: Some(datacenter.into()),
        }
    }

    /// A locality known only by its datacenter.
    #[must_use]
    pub fn in_datacenter(datacenter: impl Into<String>) -> Self {
        Self {
            machine: None,
            datacenter: Some(datacenter.into()),
        }
    }

    /// How far `other` is from `self`: the same machine first, then the
    /// same datacenter, then anything else (`loadBalanceDistance`).
    #[must_use]
    pub fn distance_to(&self, other: &Self) -> Distance {
        if self.machine.is_some() && self.machine == other.machine {
            return Distance::SameMachine;
        }
        if self.datacenter.is_some() && self.datacenter == other.datacenter {
            return Distance::SameDatacenter;
        }
        Distance::Distant
    }
}

/// The distance ranking of an alternative from the caller; smaller is
/// preferred (`FoundationDB`'s `LBDistance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Distance {
    /// Same machine identifier.
    SameMachine,
    /// Same datacenter identifier, another machine.
    SameDatacenter,
    /// Anything else, including unknown locality.
    Distant,
}

#[cfg(test)]
mod tests {
    use super::{Distance, Locality};

    #[test]
    fn distance_ranks_machine_then_datacenter_then_distant() {
        let me = Locality::new("m1", "dc1");
        assert_eq!(
            me.distance_to(&Locality::new("m1", "dc1")),
            Distance::SameMachine
        );
        assert_eq!(
            me.distance_to(&Locality::new("m2", "dc1")),
            Distance::SameDatacenter
        );
        assert_eq!(
            me.distance_to(&Locality::in_datacenter("dc1")),
            Distance::SameDatacenter
        );
        assert_eq!(
            me.distance_to(&Locality::new("m3", "dc2")),
            Distance::Distant
        );
        assert_eq!(me.distance_to(&Locality::unknown()), Distance::Distant);
        // Unknown never matches unknown.
        assert_eq!(
            Locality::unknown().distance_to(&Locality::unknown()),
            Distance::Distant
        );
        let mut ranked = vec![
            Distance::Distant,
            Distance::SameMachine,
            Distance::SameDatacenter,
        ];
        ranked.sort();
        assert_eq!(
            ranked,
            vec![
                Distance::SameMachine,
                Distance::SameDatacenter,
                Distance::Distant
            ]
        );
    }
}
