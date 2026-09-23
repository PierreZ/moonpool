//! Alternative sets: typed, incarnation-specific references with an
//! explicit, versioned identity (`FoundationDB`'s `MultiInterface` of
//! `ReferencedInterface`s, without its storage-server schema).

use std::collections::BTreeSet;

use super::locality::Locality;
use crate::endpoint::Endpoint;
use crate::interface::ServiceRef;
use crate::protocol::RpcMethod;

/// The application-chosen version of an [`AlternativeSet`].
///
/// A balanced client only ever moves to a strictly larger version, so a
/// delayed, older set can never replace a newer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SetVersion(u64);

impl SetVersion {
    /// Wrap a raw version number.
    #[must_use]
    pub const fn new(version: u64) -> Self {
        Self(version)
    }

    /// The raw version number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for SetVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// One alternative: a typed reference to one endpoint incarnation, and
/// where it runs.
pub struct Alternative<M: RpcMethod> {
    /// The incarnation-specific reference. It is never refreshed: a
    /// restarted server is a new alternative in a new set.
    pub target: ServiceRef<M>,
    /// Where it runs, for the distance ranking.
    pub locality: Locality,
}

// Manual impls: derives would demand the same traits of the marker `M`.
impl<M: RpcMethod> Clone for Alternative<M> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            locality: self.locality.clone(),
        }
    }
}

impl<M: RpcMethod> PartialEq for Alternative<M> {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.locality == other.locality
    }
}

impl<M: RpcMethod> Eq for Alternative<M> {}

impl<M: RpcMethod> std::fmt::Debug for Alternative<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Alternative")
            .field("target", &self.target)
            .field("locality", &self.locality)
            .finish()
    }
}

impl<M: RpcMethod> Alternative<M> {
    /// An alternative with a locality.
    #[must_use]
    pub fn new(target: ServiceRef<M>, locality: Locality) -> Self {
        Self { target, locality }
    }

    /// An alternative without locality information.
    #[must_use]
    pub fn anywhere(target: ServiceRef<M>) -> Self {
        Self::new(target, Locality::unknown())
    }
}

/// Why an [`AlternativeSet`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidSet {
    /// A reference is malformed or names another method.
    InvalidReference {
        /// Its position in the set.
        index: usize,
        /// Why.
        detail: String,
    },
    /// Two alternatives name the same endpoint incarnation.
    DuplicateEndpoint(Endpoint),
}

impl std::fmt::Display for InvalidSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidReference { index, detail } => {
                write!(f, "alternative {index} is not a valid reference: {detail}")
            }
            Self::DuplicateEndpoint(endpoint) => {
                write!(f, "endpoint {endpoint} appears twice in the set")
            }
        }
    }
}

impl std::error::Error for InvalidSet {}

/// A versioned set of alternatives serving the same method.
///
/// Immutable once built. Every call snapshots the set installed when it
/// starts; replacing the set ([`BalancedClient::replace`](super::BalancedClient::replace))
/// never touches a call already running, whose attempts keep their
/// original references. An empty set is valid: calls on it fail with
/// [`BalanceFailure::NoAlternatives`](super::BalanceFailure::NoAlternatives).
pub struct AlternativeSet<M: RpcMethod> {
    version: SetVersion,
    alternatives: Vec<Alternative<M>>,
}

impl<M: RpcMethod> Clone for AlternativeSet<M> {
    fn clone(&self) -> Self {
        Self {
            version: self.version,
            alternatives: self.alternatives.clone(),
        }
    }
}

impl<M: RpcMethod> PartialEq for AlternativeSet<M> {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version && self.alternatives == other.alternatives
    }
}

impl<M: RpcMethod> Eq for AlternativeSet<M> {}

impl<M: RpcMethod> std::fmt::Debug for AlternativeSet<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlternativeSet")
            .field("version", &self.version)
            .field("alternatives", &self.alternatives)
            .finish()
    }
}

impl<M: RpcMethod> AlternativeSet<M> {
    /// Build a set, checking every reference and refusing duplicates.
    ///
    /// # Errors
    ///
    /// [`InvalidSet`] for a malformed reference or a repeated endpoint.
    pub fn new(version: SetVersion, alternatives: Vec<Alternative<M>>) -> Result<Self, InvalidSet> {
        let mut seen = BTreeSet::new();
        for (index, alternative) in alternatives.iter().enumerate() {
            alternative
                .target
                .check()
                .map_err(|error| InvalidSet::InvalidReference {
                    index,
                    detail: error.0,
                })?;
            let endpoint = alternative.target.endpoint();
            if !seen.insert(endpoint) {
                return Err(InvalidSet::DuplicateEndpoint(endpoint));
            }
        }
        Ok(Self {
            version,
            alternatives,
        })
    }

    /// An empty set at `version`.
    #[must_use]
    pub fn empty(version: SetVersion) -> Self {
        Self {
            version,
            alternatives: Vec::new(),
        }
    }

    /// The set's version.
    #[must_use]
    pub fn version(&self) -> SetVersion {
        self.version
    }

    /// The alternatives, in the order they were given.
    #[must_use]
    pub fn alternatives(&self) -> &[Alternative<M>] {
        &self.alternatives
    }

    /// How many alternatives the set has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.alternatives.len()
    }

    /// Whether the set has no alternative.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.alternatives.is_empty()
    }
}

/// A replacement set that was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplaceError {
    /// The offered version is not newer than the installed one.
    NotNewer {
        /// The installed version.
        installed: SetVersion,
        /// The refused version.
        offered: SetVersion,
    },
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNewer { installed, offered } => write!(
                f,
                "alternative set {offered} is not newer than the installed {installed}"
            ),
        }
    }
}

impl std::error::Error for ReplaceError {}

/// What a balanced client currently knows about its installed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetStatus {
    /// The installed set's version.
    pub version: SetVersion,
    /// How many alternatives it has.
    pub alternatives: usize,
    /// A call found every alternative permanently gone (endpoint not
    /// found or a stale incarnation): only a replacement can help.
    pub stale: bool,
    /// A call found no alternative reachable for its whole wait bound.
    /// Cleared by the next call that reaches one, or by a replacement.
    pub all_failed: bool,
}
