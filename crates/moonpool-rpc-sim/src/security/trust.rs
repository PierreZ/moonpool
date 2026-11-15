//! The run's trust material, outside every runtime: the scripted UTC, the
//! rotating key ring, token issuance and the verifier servers use.
//!
//! The tokens are the campaign's own format, not a standard one:
//! `alg.kid.sub.iss.aud.nbf.exp.mac`, where `mac` is a keyed XXH3-64 of
//! everything before it under the secret of the key that signed it. None of
//! this is secret or secure: it is a deterministic stand-in for an identity
//! provider, used to drive the runtime's credential path (the
//! [`RequestVerifier`] seam, denials, audit, metrics) through key and time
//! transitions. A seed replays the same tokens byte for byte.
//!
//! The verifier checks in the order a signed-token verifier does: the
//! format, the algorithm, the key id against the published keys, the
//! signature, the issuer and the audience, then the time. The oracle in
//! [`super::state`] relies on that order.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::lock;
use moonpool_rpc::security::{
    AccessRequest, CredentialError, FixedUtc, Principal, RequestVerifier, UtcTime,
};
use moonpool_sim::{SimulationResult, StateHandle};

const TRUST_KEY: &str = "rpc.security.trust";

/// Where the campaign's tokens come from.
pub const ISSUER: &str = "moonpool-sim-issuer";
/// Who they are for.
pub const AUDIENCE: &str = "rpc-security";
/// The scripted UTC at the start of every run (2030-01-01T00:00:00Z).
pub const START_UTC: u64 = 1_893_456_000;
/// Keys a rotation keeps published (the newest ones).
const PUBLISHED_KEYS: usize = 2;
/// The index of a key that is never published.
const STRANGER: u32 = u32::MAX;
/// The one algorithm the verifier accepts.
const ALGORITHM: &str = "sim-mac";

/// The key id of key `index`.
#[must_use]
pub fn kid(index: u32) -> String {
    if index == STRANGER {
        "stranger".to_string()
    } else {
        format!("k{index}")
    }
}

/// The key index a key id names, if it names one.
fn index_of(kid: &str) -> Option<u32> {
    if kid == "stranger" {
        return Some(STRANGER);
    }
    kid.strip_prefix('k')?.parse().ok()
}

/// Key `index`'s secret: a fixed function of the index.
fn secret(index: u32) -> u64 {
    0x5e5e_5e5e_0000_0000 ^ u64::from(index).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// The signature of `payload` under key `index`.
fn mac(payload: &str, index: u32) -> String {
    format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64_with_seed(payload.as_bytes(), secret(index))
    )
}

/// One token's fields, before signing.
struct Claims<'a> {
    algorithm: &'a str,
    kid: String,
    subject: &'a str,
    issuer: &'a str,
    audience: &'a str,
    not_before: u64,
    expires: u64,
}

impl Claims<'_> {
    /// The token, signed by key `signer`.
    fn sign(&self, signer: u32) -> Vec<u8> {
        let payload = format!(
            "{}.{}.{}.{}.{}.{}.{}",
            self.algorithm,
            self.kid,
            self.subject,
            self.issuer,
            self.audience,
            self.not_before,
            self.expires
        );
        let signature = mac(&payload, signer);
        format!("{payload}.{signature}").into_bytes()
    }
}

/// How a request's credential was made: what the oracle expects of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// No credential.
    Anonymous,
    /// Signed by the newest published key, valid for minutes.
    Valid,
    /// Signed by the newest key, valid for a few seconds of UTC.
    ShortLived,
    /// Signed by the newest key, valid only from a later UTC.
    NotYetValid,
    /// Signed by a key the issuer already retired.
    RotatedOut,
    /// Names the newest key but is signed by a key never published.
    Forged,
    /// Names and is signed by a key never published.
    UnknownKey,
    /// For another audience.
    WrongAudience,
    /// From another issuer.
    WrongIssuer,
    /// Names the newest key under an algorithm the verifier refuses.
    WrongAlgorithm,
    /// Not a token at all.
    Garbage,
    /// Minted afresh by the caller's credential source for every attempt.
    Refreshing,
}

impl Kind {
    /// Kinds a caller picks from, in draw order.
    pub const DRAWN: [Self; 11] = [
        Self::Anonymous,
        Self::Valid,
        Self::ShortLived,
        Self::NotYetValid,
        Self::RotatedOut,
        Self::Forged,
        Self::UnknownKey,
        Self::WrongAudience,
        Self::WrongIssuer,
        Self::WrongAlgorithm,
        Self::Garbage,
    ];

    /// A properly signed token whose acceptance depends on time and keys.
    #[must_use]
    pub fn is_signed_by_the_issuer(self) -> bool {
        matches!(
            self,
            Self::Valid
                | Self::ShortLived
                | Self::NotYetValid
                | Self::RotatedOut
                | Self::Refreshing
        )
    }
}

/// A minted credential and what the ledger needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minted {
    /// The kind actually minted (a rotated-out key before any rotation is
    /// minted as an unknown key).
    pub kind: Kind,
    /// The token bytes (none for an anonymous call).
    pub token: Option<Vec<u8>>,
    /// The key it names.
    pub key: Option<u32>,
    /// Its not-before, in UTC seconds.
    pub not_before: u64,
    /// Its expiry, in UTC seconds.
    pub expires: u64,
}

struct Ring {
    /// The key set's generation: bumped by every rotation.
    generation: u64,
    /// Published key indices, oldest first.
    published: Vec<u32>,
    /// Keys ever retired.
    retired: Vec<u32>,
    next: u32,
    /// Published key indices per generation.
    history: BTreeMap<u64, Vec<u32>>,
}

/// The run's trust material. Clones share it.
#[derive(Clone)]
pub struct Trust {
    /// The UTC the script is at (always known to the script).
    utc: Arc<AtomicU64>,
    /// What servers read: the script's UTC, or unknown while forgotten.
    clock: FixedUtc,
    /// The clock epoch: incremented at each forget and each restore, so
    /// the servers have no UTC exactly when it is odd.
    clock_epoch: Arc<AtomicU64>,
    ring: Arc<Mutex<Ring>>,
    /// Every token minted in the run, by any caller or source.
    tokens: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Trust {
    /// The run's trust material, created on first use: key 0 published,
    /// UTC at [`START_UTC`].
    ///
    /// # Errors
    ///
    /// Never: kept fallible so callers need not change if creation ever
    /// can fail.
    pub fn of(state: &StateHandle) -> SimulationResult<Self> {
        if let Some(trust) = state.get::<Self>(TRUST_KEY) {
            return Ok(trust);
        }
        let trust = Self {
            utc: Arc::new(AtomicU64::new(START_UTC)),
            clock: FixedUtc::new(UtcTime::from_unix_seconds(START_UTC)),
            clock_epoch: Arc::new(AtomicU64::new(0)),
            ring: Arc::new(Mutex::new(Ring {
                generation: 1,
                published: vec![0],
                retired: Vec::new(),
                next: 1,
                history: BTreeMap::from([(1, vec![0])]),
            })),
            tokens: Arc::new(Mutex::new(Vec::new())),
        };
        state.publish(TRUST_KEY, trust.clone());
        Ok(trust)
    }

    /// The script's UTC now.
    #[must_use]
    pub fn utc(&self) -> u64 {
        self.utc.load(Ordering::Relaxed)
    }

    /// The clock servers validate with.
    #[must_use]
    pub fn clock(&self) -> FixedUtc {
        self.clock.clone()
    }

    /// The current key set generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        lock(&self.ring).generation
    }

    /// A verifier over the shared, rotating key set.
    #[must_use]
    pub fn verifier(&self) -> Verifier {
        Verifier {
            ring: Arc::clone(&self.ring),
        }
    }

    /// Move UTC `seconds` later (servers see it unless it is forgotten).
    pub fn advance(&self, seconds: u64) {
        let now = self.utc.fetch_add(seconds, Ordering::Relaxed) + seconds;
        if !self.is_forgotten() {
            self.clock.set(UtcTime::from_unix_seconds(now));
        }
    }

    /// Servers lose the time (every expiring credential then fails
    /// closed).
    pub fn forget(&self) {
        if !self.is_forgotten() {
            self.clock_epoch.fetch_add(1, Ordering::Relaxed);
        }
        self.clock.forget();
    }

    /// Whether servers currently know the time.
    #[must_use]
    pub fn is_forgotten(&self) -> bool {
        !self.clock_epoch().is_multiple_of(2)
    }

    /// The clock epoch now: it changes at each forget and each restore,
    /// and it is odd while servers have no UTC. The change and the clock
    /// change happen together (no await between them), so the epochs of
    /// two moments tell whether the clock was absent between them.
    #[must_use]
    pub fn clock_epoch(&self) -> u64 {
        self.clock_epoch.load(Ordering::Relaxed)
    }

    /// Servers see the script's UTC again.
    pub fn restore(&self) {
        if self.is_forgotten() {
            self.clock_epoch.fetch_add(1, Ordering::Relaxed);
        }
        self.clock.set(UtcTime::from_unix_seconds(self.utc()));
    }

    /// Publish a fresh key and retire the oldest ones, so at most two keys
    /// stay published; returns the new generation. Like `FoundationDB`'s
    /// `applyPublicKeySet`, a rotation replaces the whole set: a key left
    /// out is refused for every later request.
    ///
    /// # Errors
    ///
    /// Never: kept fallible for the callers' sake.
    pub fn rotate(&self) -> SimulationResult<u64> {
        let mut ring = lock(&self.ring);
        let fresh = ring.next;
        ring.next += 1;
        ring.published.push(fresh);
        while ring.published.len() > PUBLISHED_KEYS {
            let retired = ring.published.remove(0);
            ring.retired.push(retired);
        }
        ring.generation += 1;
        let generation = ring.generation;
        let published = ring.published.clone();
        ring.history.insert(generation, published);
        Ok(generation)
    }

    /// Whether key `index` was published at some generation in `low..=high`.
    #[must_use]
    pub fn published_between(&self, index: u32, low: u64, high: u64) -> bool {
        let ring = lock(&self.ring);
        // Every generation is recorded; start from the one in force at
        // `low` all the same.
        let start = ring
            .history
            .range(..=low)
            .next_back()
            .map_or(low, |(generation, _)| *generation);
        ring.history
            .range(start..=high.max(start))
            .any(|(_, keys)| keys.contains(&index))
    }

    /// Mint a credential of `kind` for `subject` at the script's UTC; `ttl`
    /// and `delay` (seconds) shape the time window where it applies.
    #[must_use]
    pub fn mint(&self, kind: Kind, subject: &str, ttl: u64, delay: u64) -> Minted {
        let minted = self.mint_unrecorded(kind, subject, ttl, delay);
        if let Some(token) = &minted.token {
            lock(&self.tokens).push(token.clone());
        }
        minted
    }

    /// Every token minted so far in the run: each caller's and each one a
    /// refreshing source made.
    #[must_use]
    pub fn tokens(&self) -> Vec<Vec<u8>> {
        lock(&self.tokens).clone()
    }

    fn mint_unrecorded(&self, kind: Kind, subject: &str, ttl: u64, delay: u64) -> Minted {
        let now = self.utc();
        let (newest, retired) = {
            let ring = lock(&self.ring);
            (
                ring.published.last().copied().unwrap_or(0),
                ring.retired.last().copied(),
            )
        };
        let unsigned = |token: Option<Vec<u8>>| Minted {
            kind,
            token,
            key: None,
            not_before: 0,
            expires: 0,
        };
        let plan = match kind {
            Kind::Anonymous => return unsigned(None),
            Kind::Garbage => return unsigned(Some(b"not a token".to_vec())),
            Kind::WrongAlgorithm => {
                let token = Claims {
                    algorithm: "none",
                    kid: kid(newest),
                    subject,
                    issuer: ISSUER,
                    audience: AUDIENCE,
                    not_before: now - 5,
                    expires: now + 300,
                }
                .sign(newest);
                return Minted {
                    kind,
                    token: Some(token),
                    key: Some(newest),
                    not_before: now - 5,
                    expires: now + 300,
                };
            }
            _ => Plan::of(kind, now, newest, retired, ttl, delay),
        };
        let token = Claims {
            algorithm: ALGORITHM,
            kid: kid(plan.named),
            subject,
            issuer: plan.issuer,
            audience: plan.audience,
            not_before: plan.not_before,
            expires: plan.expires,
        }
        .sign(plan.signer);
        Minted {
            kind: plan.kind,
            token: Some(token),
            key: Some(plan.named),
            not_before: plan.not_before,
            expires: plan.expires,
        }
    }
}

/// How a token of some kind is signed and what it says.
struct Plan {
    kind: Kind,
    signer: u32,
    named: u32,
    issuer: &'static str,
    audience: &'static str,
    not_before: u64,
    expires: u64,
}

impl Plan {
    fn of(kind: Kind, now: u64, newest: u32, retired: Option<u32>, ttl: u64, delay: u64) -> Self {
        let proper = Self {
            kind,
            signer: newest,
            named: newest,
            issuer: ISSUER,
            audience: AUDIENCE,
            not_before: now - 5,
            expires: now + 300,
        };
        match kind {
            Kind::Valid | Kind::Refreshing => Self {
                expires: now + ttl.max(30),
                ..proper
            },
            Kind::ShortLived => Self {
                expires: now + ttl.clamp(1, 8),
                ..proper
            },
            Kind::NotYetValid => {
                let from = now + delay.max(5);
                Self {
                    not_before: from,
                    expires: from + 300,
                    ..proper
                }
            }
            // Before any rotation there is no retired key: an unknown one.
            Kind::RotatedOut => match retired {
                Some(old) => Self {
                    signer: old,
                    named: old,
                    ..proper
                },
                None => Self {
                    kind: Kind::UnknownKey,
                    signer: STRANGER,
                    named: STRANGER,
                    ..proper
                },
            },
            Kind::Forged => Self {
                signer: STRANGER,
                ..proper
            },
            Kind::UnknownKey => Self {
                signer: STRANGER,
                named: STRANGER,
                ..proper
            },
            Kind::WrongAudience => Self {
                audience: "elsewhere",
                ..proper
            },
            Kind::WrongIssuer => Self {
                issuer: "mallory",
                ..proper
            },
            Kind::Anonymous | Kind::Garbage | Kind::WrongAlgorithm => proper,
        }
    }
}

/// The servers' verifier: the campaign's tokens against the key set in
/// force at the moment of the check. Clones share the key ring.
#[derive(Clone)]
pub struct Verifier {
    ring: Arc<Mutex<Ring>>,
}

impl Verifier {
    fn published(&self) -> BTreeSet<u32> {
        lock(&self.ring).published.iter().copied().collect()
    }
}

impl RequestVerifier for Verifier {
    fn verify(
        &self,
        request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError> {
        let token = std::str::from_utf8(credential).map_err(|_| CredentialError::Malformed)?;
        let fields: Vec<&str> = token.split('.').collect();
        let [
            algorithm,
            kid,
            subject,
            issuer,
            audience,
            not_before,
            expires,
            signature,
        ] = fields[..]
        else {
            return Err(CredentialError::Malformed);
        };
        let not_before: u64 = not_before.parse().map_err(|_| CredentialError::Malformed)?;
        let expires: u64 = expires.parse().map_err(|_| CredentialError::Malformed)?;
        if algorithm != ALGORITHM {
            return Err(CredentialError::UnsupportedAlgorithm);
        }
        let key = index_of(kid)
            .filter(|index| self.published().contains(index))
            .ok_or(CredentialError::UnknownKey)?;
        let payload = &token[..token.len() - signature.len() - 1];
        if mac(payload, key) != signature {
            return Err(CredentialError::BadSignature);
        }
        if issuer != ISSUER {
            return Err(CredentialError::WrongIssuer);
        }
        if audience != AUDIENCE {
            return Err(CredentialError::WrongAudience);
        }
        let now = request
            .now
            .ok_or(CredentialError::ClockUnavailable)?
            .unix_seconds();
        if now >= expires {
            return Err(CredentialError::Expired);
        }
        if now < not_before {
            return Err(CredentialError::NotYetValid);
        }
        Ok(Principal::new(subject)
            .with_issuer(issuer)
            .with_expiry(UtcTime::from_unix_seconds(expires)))
    }
}
