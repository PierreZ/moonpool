//! The run's trust material, outside every runtime: the scripted UTC, the
//! rotating Ed25519 key ring and its published JWKS, and token issuance.
//!
//! Keys are derived from fixed seeds and `EdDSA` signs deterministically, so
//! a seed replays the same tokens byte for byte. None of this is secret or
//! secure: it is a deterministic stand-in for an identity provider, used
//! to drive the production verifier through key and time transitions.
//!
//! **Keep simulated signing deterministic.** `EdDSA` (and HMAC) signatures
//! are functions of the key and the message; `jsonwebtoken`'s ECDSA signer
//! uses RFC 6979 nonces as well, but its RSA (`RS*`/`PS*`) signing draws
//! from `thread_rng`, the host's entropy: a seed would no longer replay the
//! same tokens and the determinism canary would trip. Never sign with RSA
//! here.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use moonpool_rpc::security::jwt::{JwksKeys, JwtConfig, JwtVerifier};
use moonpool_rpc::security::{FixedUtc, UtcTime};
use moonpool_sim::{SimulationError, SimulationResult, StateHandle};

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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("Mutex poisoned: prior task panicked")
}

/// The key id of key `index`.
#[must_use]
pub fn kid(index: u32) -> String {
    if index == STRANGER {
        "stranger".to_string()
    } else {
        format!("k{index}")
    }
}

/// Key `index`: an Ed25519 key from a fixed seed (PKCS#8 v1 DER).
fn signing_key(index: u32) -> EncodingKey {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    let mut seed = [0x5e_u8; 32];
    seed[..4].copy_from_slice(&index.to_le_bytes());
    der.extend_from_slice(&seed);
    EncodingKey::from_ed_der(&der)
}

fn install_provider() {
    let _ = jsonwebtoken::crypto::CryptoProvider::install_default(
        &jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER,
    );
}

fn public_jwk(index: u32) -> Option<Jwk> {
    install_provider();
    let mut jwk = Jwk::from_encoding_key(&signing_key(index), Algorithm::EdDSA).ok()?;
    jwk.common.key_id = Some(kid(index));
    Some(jwk)
}

fn jwks(indices: &[u32]) -> JwkSet {
    JwkSet {
        keys: indices
            .iter()
            .filter_map(|index| public_jwk(*index))
            .collect(),
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
    /// HS256 with the newest key's id: algorithm confusion.
    Symmetric,
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
        Self::Symmetric,
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
    /// Published key indices, oldest first.
    published: Vec<u32>,
    /// Keys ever retired.
    retired: Vec<u32>,
    next: u32,
    /// Published key indices per JWKS generation.
    history: BTreeMap<u64, Vec<u32>>,
}

/// The run's trust material. Clones share it.
#[derive(Clone)]
pub struct Trust {
    /// The UTC the script is at (always known to the script).
    utc: Arc<AtomicU64>,
    /// What servers read: the script's UTC, or unknown while forgotten.
    clock: FixedUtc,
    forgotten: Arc<AtomicBool>,
    keys: JwksKeys,
    ring: Arc<Mutex<Ring>>,
}

impl Trust {
    /// The run's trust material, created on first use: key 0 published,
    /// UTC at [`START_UTC`].
    ///
    /// # Errors
    ///
    /// If the fixed key 0 cannot be published (a broken dependency).
    pub fn of(state: &StateHandle) -> SimulationResult<Self> {
        if let Some(trust) = state.get::<Self>(TRUST_KEY) {
            return Ok(trust);
        }
        let keys = JwksKeys::from_set(&jwks(&[0]))
            .map_err(|error| SimulationError::InvalidState(format!("key 0: {error}")))?;
        let trust = Self {
            utc: Arc::new(AtomicU64::new(START_UTC)),
            clock: FixedUtc::new(UtcTime::from_unix_seconds(START_UTC)),
            forgotten: Arc::new(AtomicBool::new(false)),
            ring: Arc::new(Mutex::new(Ring {
                published: vec![0],
                retired: Vec::new(),
                next: 1,
                history: BTreeMap::from([(keys.generation(), vec![0])]),
            })),
            keys,
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

    /// The current JWKS generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.keys.generation()
    }

    /// A verifier over the shared, rotating key set.
    ///
    /// # Errors
    ///
    /// If the fixed configuration is refused (a broken dependency).
    pub fn verifier(&self, cache_capacity: usize) -> SimulationResult<JwtVerifier> {
        let mut config = JwtConfig::new(ISSUER, AUDIENCE);
        config.algorithms = vec![Algorithm::EdDSA];
        config.cache_capacity = cache_capacity;
        JwtVerifier::new(self.keys.clone(), config)
            .map_err(|error| SimulationError::InvalidState(error.to_string()))
    }

    /// Move UTC `seconds` later (servers see it unless it is forgotten).
    pub fn advance(&self, seconds: u64) {
        let now = self.utc.fetch_add(seconds, Ordering::Relaxed) + seconds;
        if !self.forgotten.load(Ordering::Relaxed) {
            self.clock.set(UtcTime::from_unix_seconds(now));
        }
    }

    /// Servers lose the time (every expiring credential then fails
    /// closed).
    pub fn forget(&self) {
        self.forgotten.store(true, Ordering::Relaxed);
        self.clock.forget();
    }

    /// Whether servers currently know the time.
    #[must_use]
    pub fn is_forgotten(&self) -> bool {
        self.forgotten.load(Ordering::Relaxed)
    }

    /// Servers see the script's UTC again.
    pub fn restore(&self) {
        self.forgotten.store(false, Ordering::Relaxed);
        self.clock.set(UtcTime::from_unix_seconds(self.utc()));
    }

    /// Publish a fresh key and retire the oldest beyond
    /// [`PUBLISHED_KEYS`]; returns the new generation.
    ///
    /// # Errors
    ///
    /// If a fixed key cannot be published (a broken dependency).
    pub fn rotate(&self) -> SimulationResult<u64> {
        let mut ring = lock(&self.ring);
        let fresh = ring.next;
        ring.next += 1;
        ring.published.push(fresh);
        while ring.published.len() > PUBLISHED_KEYS {
            let retired = ring.published.remove(0);
            ring.retired.push(retired);
        }
        let generation = self
            .keys
            .replace_set(&jwks(&ring.published))
            .map_err(|error| SimulationError::InvalidState(format!("rotation: {error}")))?;
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

    /// Whether key `index` is published now.
    #[must_use]
    pub fn is_published(&self, index: u32) -> bool {
        lock(&self.ring).published.contains(&index)
    }

    /// Mint a credential of `kind` for `subject` at the script's UTC; `ttl`
    /// and `delay` (seconds) shape the time window where it applies.
    #[must_use]
    pub fn mint(&self, kind: Kind, subject: &str, ttl: u64, delay: u64) -> Minted {
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
            Kind::Symmetric => {
                let mut header = Header::new(Algorithm::HS256);
                header.kid = Some(kid(newest));
                let token = encode(
                    &header,
                    &claims(subject, ISSUER, AUDIENCE, now - 5, now + 300),
                    &EncodingKey::from_secret(b"not-a-public-key"),
                )
                .ok()
                .map(String::into_bytes);
                return Minted {
                    kind,
                    token,
                    key: Some(newest),
                    not_before: now - 5,
                    expires: now + 300,
                };
            }
            _ => Plan::of(kind, now, newest, retired, ttl, delay),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(kid(plan.named));
        let token = encode(
            &header,
            &claims(
                subject,
                plan.issuer,
                plan.audience,
                plan.not_before,
                plan.expires,
            ),
            &signing_key(plan.signer),
        )
        .ok()
        .map(String::into_bytes);
        Minted {
            kind: plan.kind,
            token,
            key: Some(plan.named),
            not_before: plan.not_before,
            expires: plan.expires,
        }
    }
}

/// How an `EdDSA` token of some kind is signed and what it says.
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
            Kind::Anonymous | Kind::Garbage | Kind::Symmetric => proper,
        }
    }
}

fn claims(
    subject: &str,
    issuer: &str,
    audience: &str,
    not_before: u64,
    expires: u64,
) -> serde_json::Value {
    serde_json::json!({
        "sub": subject,
        "iss": issuer,
        "aud": audience,
        "nbf": not_before,
        "exp": expires,
    })
}
