//! JWT/JWKS request verification (feature `jwt`).
//!
//! A [`JwtVerifier`] is a [`RequestVerifier`] for signed JSON Web Tokens
//! (RFC 7519) whose public keys come from a JSON Web Key Set (RFC 7517),
//! built on the `jsonwebtoken` crate's pure-Rust backend. It implements no
//! cryptography itself.
//!
//! # What is checked, in order
//!
//! 1. Size: a token above [`JwtConfig::max_token_bytes`] is
//!    [`CredentialError::TooLarge`] before it is parsed.
//! 2. Header: it parses, declares no critical extension (`crit`), its
//!    `typ`, when present, is accepted ([`JwtConfig::accepted_types`]), its
//!    `alg` is in the allow list
//!    ([`JwtConfig::algorithms`]; only public-key algorithms can be allowed,
//!    so an `HS256` token presented against a public key is refused), and it
//!    names a key id (`kid`). Keys embedded in the token (`jwk`, `jku`,
//!    `x5u`, `x5c`) are never used.
//! 3. Key: the `kid` is in the **current** key set
//!    ([`JwksKeys`]; unknown or rotated out: [`CredentialError::UnknownKey`])
//!    and the token's `alg` is the key's own algorithm.
//! 4. Signature, then issuer and audience ([`JwtConfig::issuers`],
//!    [`JwtConfig::audiences`]); `sub` (non-empty), `iss`, `aud` and `exp`
//!    are required.
//! 5. Time, against the injected [`UtcClock`](super::UtcClock) (never the
//!    host clock): `exp` must be after now and `nbf`, when present, not
//!    after now, both within [`JwtConfig::leeway_seconds`]. An unknown time
//!    is [`CredentialError::ClockUnavailable`].
//!
//! The principal is the `sub` claim, with the `iss`, the space-separated
//! `scope` claim as scopes and `exp` as its expiry. What a subject or a
//! scope means is the application's business ([`AccessPolicy`](super::AccessPolicy)).
//!
//! # Keys and rotation
//!
//! [`JwksKeys::replace_json`] swaps the whole set, as `FoundationDB`'s
//! `applyPublicKeySet` does after re-reading its JWKS file: a key left out is
//! revoked for every later request. A set that does not parse, is larger
//! than [`MAX_JWKS_BYTES`], has more than [`MAX_JWKS_KEYS`] keys, or holds a
//! key without an id, with a symmetric (`oct`) key, an RSA modulus below
//! [`MIN_RSA_MODULUS_BITS`] or an unsupported algorithm is refused as a
//! whole and the current set stays.
//!
//! # Cache
//!
//! Verified tokens are cached (bounded by [`JwtConfig::cache_capacity`],
//! oldest first out), keyed by the **whole token** (never a hash, which an
//! attacker could collide) and by the key set's generation: a key
//! replacement invalidates every cached entry. Time is checked again on
//! every hit, so a cached token still expires on time.
//!
//! # Dependencies
//!
//! `jsonwebtoken` with `rust_crypto` pulls the `rsa` crate, which carries
//! RUSTSEC-2023-0071 (a timing side channel in RSA **private-key**
//! operations). This module only verifies signatures with public keys.
//!
//! `jsonwebtoken` 11 selects its crypto backend through a **process-wide**
//! default provider, and panics on first use without one. [`JwtVerifier::new`]
//! and [`JwksKeys`] install the pure-Rust provider as that default if none
//! was installed first; an application that installs another provider
//! (`aws_lc_rs`) before them keeps it, for this module too.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

pub use jsonwebtoken::Algorithm;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm};
use jsonwebtoken::{
    DecodingKey, DecodingKeyKind, Validation, decode, decode_header, errors::ErrorKind,
};
use serde::Deserialize;

use super::{AccessRequest, CredentialError, Principal, RequestVerifier, RotatingKeys, UtcTime};

/// The largest JWKS document [`JwksKeys`] parses (64 KiB).
pub const MAX_JWKS_BYTES: usize = 64 * 1024;

/// The most keys one JWKS may hold.
pub const MAX_JWKS_KEYS: usize = 64;

/// The smallest RSA modulus a JWKS key may have, in bits.
pub const MIN_RSA_MODULUS_BITS: usize = 2048;

/// One verification key: the public key and the only algorithm it verifies.
#[derive(Clone)]
pub struct VerificationKey {
    key: DecodingKey,
    algorithm: Algorithm,
}

impl VerificationKey {
    /// The algorithm this key verifies.
    #[must_use]
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }
}

impl std::fmt::Debug for VerificationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerificationKey")
            .field("algorithm", &self.algorithm)
            .finish_non_exhaustive()
    }
}

/// A JWKS that could not be used; the current key set is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum JwksError {
    /// The document is larger than [`MAX_JWKS_BYTES`].
    #[error("JWKS document of {0} bytes exceeds {MAX_JWKS_BYTES}")]
    TooLarge(usize),
    /// The document holds more than [`MAX_JWKS_KEYS`] keys.
    #[error("JWKS holds {0} keys, more than {MAX_JWKS_KEYS}")]
    TooManyKeys(usize),
    /// Not a JWKS document.
    #[error("JWKS does not parse: {0}")]
    Parse(String),
    /// A key has no `kid`, or two keys share one.
    #[error("every JWKS key needs a unique kid")]
    KeyId,
    /// A key is symmetric, of an unsupported type or curve, or its
    /// algorithm is not a public-key signature algorithm.
    #[error("unsupported JWKS key {kid:?}: {detail}")]
    Unsupported {
        /// The key's id.
        kid: String,
        /// What is wrong with it.
        detail: String,
    },
}

/// The rotating verification key set of a [`JwtVerifier`]. Clones share
/// the set, so the application keeps one to rotate keys while the runtime
/// verifies with it.
#[derive(Clone, Debug)]
pub struct JwksKeys {
    keys: RotatingKeys<VerificationKey>,
}

impl JwksKeys {
    /// Keys from a JWKS JSON document.
    ///
    /// # Errors
    ///
    /// A [`JwksError`] if the document cannot be used.
    pub fn from_json(json: &str) -> Result<Self, JwksError> {
        Ok(Self {
            keys: RotatingKeys::new(parse_json(json)?),
        })
    }

    /// Keys from an already parsed set.
    ///
    /// # Errors
    ///
    /// A [`JwksError`] if a key cannot be used.
    pub fn from_set(set: &JwkSet) -> Result<Self, JwksError> {
        Ok(Self {
            keys: RotatingKeys::new(convert(set)?),
        })
    }

    /// Replace the whole set with a JWKS JSON document; returns the new
    /// generation. Keys left out are revoked from the next request on.
    ///
    /// # Errors
    ///
    /// A [`JwksError`]; the current set is then kept.
    pub fn replace_json(&self, json: &str) -> Result<u64, JwksError> {
        let keys = parse_json(json)?;
        Ok(self.keys.replace(keys))
    }

    /// Replace the whole set with a parsed one.
    ///
    /// # Errors
    ///
    /// A [`JwksError`]; the current set is then kept.
    pub fn replace_set(&self, set: &JwkSet) -> Result<u64, JwksError> {
        let keys = convert(set)?;
        Ok(self.keys.replace(keys))
    }

    /// The current generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.keys.current().generation()
    }

    /// The current key ids.
    #[must_use]
    pub fn key_ids(&self) -> Vec<String> {
        self.keys
            .current()
            .key_ids()
            .map(ToString::to_string)
            .collect()
    }
}

fn parse_json(json: &str) -> Result<Vec<(String, VerificationKey)>, JwksError> {
    if json.len() > MAX_JWKS_BYTES {
        return Err(JwksError::TooLarge(json.len()));
    }
    let set: JwkSet =
        serde_json::from_str(json).map_err(|error| JwksError::Parse(error.to_string()))?;
    convert(&set)
}

fn convert(set: &JwkSet) -> Result<Vec<(String, VerificationKey)>, JwksError> {
    if set.keys.len() > MAX_JWKS_KEYS {
        return Err(JwksError::TooManyKeys(set.keys.len()));
    }
    install_provider();
    let mut keys = BTreeMap::new();
    for jwk in &set.keys {
        let kid = jwk.common.key_id.clone().ok_or(JwksError::KeyId)?;
        let algorithm = key_algorithm(jwk).map_err(|detail| JwksError::Unsupported {
            kid: kid.clone(),
            detail,
        })?;
        let key = DecodingKey::from_jwk(jwk).map_err(|error| JwksError::Unsupported {
            kid: kid.clone(),
            detail: error.to_string(),
        })?;
        if let DecodingKeyKind::RsaModulusExponent { n, .. } = key.kind() {
            let bits = modulus_bits(n);
            if bits < MIN_RSA_MODULUS_BITS {
                return Err(JwksError::Unsupported {
                    kid: kid.clone(),
                    detail: format!("RSA modulus of {bits} bits, below {MIN_RSA_MODULUS_BITS}"),
                });
            }
        }
        if keys
            .insert(kid, VerificationKey { key, algorithm })
            .is_some()
        {
            return Err(JwksError::KeyId);
        }
    }
    Ok(keys.into_iter().collect())
}

/// The one algorithm a JWK verifies: its declared `alg`, or the only one
/// its type allows. Symmetric keys are refused: a JWKS publishes public
/// keys.
fn key_algorithm(jwk: &Jwk) -> Result<Algorithm, String> {
    let inferred = match &jwk.algorithm {
        AlgorithmParameters::EllipticCurve(params) => match params.curve {
            EllipticCurve::P256 => Algorithm::ES256,
            EllipticCurve::P384 => Algorithm::ES384,
            ref other => return Err(format!("curve {other:?}")),
        },
        AlgorithmParameters::OctetKeyPair(params) => match params.curve {
            EllipticCurve::Ed25519 => Algorithm::EdDSA,
            ref other => return Err(format!("curve {other:?}")),
        },
        AlgorithmParameters::RSA(_) => Algorithm::RS256,
        AlgorithmParameters::OctetKey(_) => return Err("symmetric key".into()),
        _ => return Err("unknown key type".into()),
    };
    let Some(declared) = jwk.common.key_algorithm else {
        return Ok(inferred);
    };
    let declared = match declared {
        KeyAlgorithm::ES256 => Algorithm::ES256,
        KeyAlgorithm::ES384 => Algorithm::ES384,
        KeyAlgorithm::EdDSA => Algorithm::EdDSA,
        KeyAlgorithm::RS256 => Algorithm::RS256,
        KeyAlgorithm::RS384 => Algorithm::RS384,
        KeyAlgorithm::RS512 => Algorithm::RS512,
        KeyAlgorithm::PS256 => Algorithm::PS256,
        KeyAlgorithm::PS384 => Algorithm::PS384,
        KeyAlgorithm::PS512 => Algorithm::PS512,
        other => return Err(format!("algorithm {other:?}")),
    };
    let rsa = |algorithm| {
        matches!(
            algorithm,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
        )
    };
    if declared == inferred || (rsa(declared) && rsa(inferred)) {
        Ok(declared)
    } else {
        Err(format!(
            "algorithm {declared:?} does not match the key type"
        ))
    }
}

/// The significant bits of a big-endian modulus.
fn modulus_bits(n: &[u8]) -> usize {
    let n = match n.iter().position(|byte| *byte != 0) {
        Some(first) => &n[first..],
        None => return 0,
    };
    n.len() * 8 - n.first().map_or(0, |top| top.leading_zeros() as usize)
}

/// Install `jsonwebtoken`'s pure-Rust provider as the process default
/// unless one is already installed (its first use panics without one).
fn install_provider() {
    let _ = jsonwebtoken::crypto::CryptoProvider::install_default(
        &jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER,
    );
}

fn is_public_key_algorithm(algorithm: Algorithm) -> bool {
    !matches!(
        algorithm,
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
    )
}

/// What a [`JwtVerifier`] accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtConfig {
    /// Accepted `iss` values; must not be empty.
    pub issuers: Vec<String>,
    /// Accepted `aud` values (the token's audience must contain one); must
    /// not be empty.
    pub audiences: Vec<String>,
    /// Allowed signature algorithms. Public-key algorithms only; the
    /// default is `ES256` and `EdDSA`.
    pub algorithms: Vec<Algorithm>,
    /// Clock skew tolerated on `exp` and `nbf`, in seconds (default 0).
    pub leeway_seconds: u64,
    /// The largest token accepted, in bytes (default 4 KiB).
    pub max_token_bytes: usize,
    /// Verified tokens remembered (default 1024; 0 disables the cache).
    pub cache_capacity: usize,
    /// Accepted values of the header's `typ`, compared ignoring ASCII
    /// case. A token without `typ` is accepted; one with any other value is
    /// [`CredentialError::Malformed`]. Default: `JWT`, `at+jwt` and
    /// `application/at+jwt` (RFC 9068 access tokens).
    pub accepted_types: Vec<String>,
}

impl JwtConfig {
    /// Accept tokens from `issuer` for `audience`, with the defaults.
    #[must_use]
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuers: vec![issuer.into()],
            audiences: vec![audience.into()],
            algorithms: vec![Algorithm::ES256, Algorithm::EdDSA],
            leeway_seconds: 0,
            max_token_bytes: 4 * 1024,
            cache_capacity: 1024,
            accepted_types: vec!["JWT".into(), "at+jwt".into(), "application/at+jwt".into()],
        }
    }
}

/// A [`JwtConfig`] that cannot verify anything.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid JWT configuration: {0}")]
pub struct InvalidJwtConfig(pub String);

#[derive(Deserialize)]
struct Claims {
    sub: String,
    iss: String,
    exp: u64,
    #[serde(default)]
    nbf: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

struct Cached {
    generation: u64,
    principal: Principal,
    not_before: Option<u64>,
    expires: u64,
}

#[derive(Default)]
struct Cache {
    entries: BTreeMap<Arc<[u8]>, Cached>,
    order: VecDeque<Arc<[u8]>>,
}

/// Verifies signed JWT bearer credentials against a rotating JWKS.
pub struct JwtVerifier {
    keys: JwksKeys,
    config: JwtConfig,
    validations: Vec<(Algorithm, Validation)>,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("config", &self.config)
            .field("keys", &self.keys.key_ids())
            .finish_non_exhaustive()
    }
}

impl JwtVerifier {
    /// A verifier of tokens signed by `keys` under `config`.
    ///
    /// # Errors
    ///
    /// [`InvalidJwtConfig`] without an issuer, an audience or an allowed
    /// algorithm, or with a symmetric algorithm allowed.
    pub fn new(keys: JwksKeys, config: JwtConfig) -> Result<Self, InvalidJwtConfig> {
        install_provider();
        if config.issuers.is_empty() || config.audiences.is_empty() {
            return Err(InvalidJwtConfig(
                "at least one issuer and one audience are required".into(),
            ));
        }
        if config.algorithms.is_empty() {
            return Err(InvalidJwtConfig("no algorithm allowed".into()));
        }
        if let Some(symmetric) = config
            .algorithms
            .iter()
            .find(|algorithm| !is_public_key_algorithm(**algorithm))
        {
            return Err(InvalidJwtConfig(format!(
                "{symmetric:?} is symmetric: a JWKS publishes public keys"
            )));
        }
        let validations = config
            .algorithms
            .iter()
            .map(|algorithm| {
                let mut validation = Validation::new(*algorithm);
                // Time is checked against the injected clock below; the
                // library would read the host clock.
                validation.validate_exp = false;
                validation.validate_nbf = false;
                validation.leeway = 0;
                validation.set_issuer(&config.issuers);
                validation.set_audience(&config.audiences);
                validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
                (*algorithm, validation)
            })
            .collect();
        Ok(Self {
            keys,
            config,
            validations,
            cache: Mutex::new(Cache::default()),
        })
    }

    /// The key set, to rotate.
    #[must_use]
    pub fn keys(&self) -> &JwksKeys {
        &self.keys
    }

    /// Tokens currently cached.
    #[must_use]
    pub fn cached(&self) -> usize {
        self.lock_cache().entries.len()
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    fn check_time(
        &self,
        now: Option<UtcTime>,
        not_before: Option<u64>,
        expires: u64,
    ) -> Result<(), CredentialError> {
        let now = now.ok_or(CredentialError::ClockUnavailable)?.unix_seconds();
        let leeway = self.config.leeway_seconds;
        if now >= expires.saturating_add(leeway) {
            return Err(CredentialError::Expired);
        }
        if let Some(not_before) = not_before
            && now.saturating_add(leeway) < not_before
        {
            return Err(CredentialError::NotYetValid);
        }
        Ok(())
    }

    fn verify_uncached(
        &self,
        token: &[u8],
        generation: &mut u64,
    ) -> Result<(Principal, Option<u64>, u64), CredentialError> {
        let header = decode_header(token).map_err(|_| CredentialError::Malformed)?;
        // No JWS extension is understood here, so a critical one is refused
        // (RFC 7515 §4.1.11).
        if header.crit.is_some() {
            return Err(CredentialError::Malformed);
        }
        if let Some(typ) = &header.typ
            && !self
                .config
                .accepted_types
                .iter()
                .any(|accepted| accepted.eq_ignore_ascii_case(typ))
        {
            return Err(CredentialError::Malformed);
        }
        let validation = self
            .validations
            .iter()
            .find(|(algorithm, _)| *algorithm == header.alg)
            .map(|(_, validation)| validation)
            .ok_or(CredentialError::UnsupportedAlgorithm)?;
        let kid = header.kid.ok_or(CredentialError::Malformed)?;
        let keys = self.keys.keys.current();
        *generation = keys.generation();
        let key = keys.get(&kid).ok_or(CredentialError::UnknownKey)?;
        if key.algorithm != header.alg {
            return Err(CredentialError::UnsupportedAlgorithm);
        }
        let data =
            decode::<Claims>(token, &key.key, validation).map_err(|error| match error.kind() {
                ErrorKind::InvalidSignature => CredentialError::BadSignature,
                ErrorKind::InvalidIssuer => CredentialError::WrongIssuer,
                ErrorKind::InvalidAudience => CredentialError::WrongAudience,
                ErrorKind::InvalidAlgorithm
                | ErrorKind::MissingAlgorithm
                | ErrorKind::UnsupportedAlgorithm => CredentialError::UnsupportedAlgorithm,
                ErrorKind::InvalidEcdsaKey
                | ErrorKind::InvalidEddsaKey
                | ErrorKind::InvalidRsaKey(_)
                | ErrorKind::InvalidKeyFormat => CredentialError::UnknownKey,
                _ => CredentialError::Malformed,
            })?;
        let claims = data.claims;
        if claims.sub.is_empty() {
            return Err(CredentialError::Malformed);
        }
        let mut principal = Principal::new(claims.sub)
            .with_issuer(claims.iss)
            .with_expiry(UtcTime::from_unix_seconds(claims.exp));
        if let Some(scope) = claims.scope {
            principal = principal.with_scopes(scope.split_whitespace());
        }
        Ok((principal, claims.nbf, claims.exp))
    }
}

impl RequestVerifier for JwtVerifier {
    fn verify(
        &self,
        request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError> {
        if credential.len() > self.config.max_token_bytes {
            return Err(CredentialError::TooLarge);
        }
        let generation = self.keys.generation();
        if self.config.cache_capacity > 0 {
            let cache = self.lock_cache();
            if let Some(hit) = cache.entries.get(credential)
                && hit.generation == generation
            {
                self.check_time(request.now, hit.not_before, hit.expires)?;
                return Ok(hit.principal.clone());
            }
        }
        let mut verified_under = generation;
        let (principal, not_before, expires) =
            self.verify_uncached(credential, &mut verified_under)?;
        self.check_time(request.now, not_before, expires)?;
        if self.config.cache_capacity > 0 {
            let mut cache = self.lock_cache();
            let token: Arc<[u8]> = Arc::from(credential);
            if cache
                .entries
                .insert(
                    Arc::clone(&token),
                    Cached {
                        generation: verified_under,
                        principal: principal.clone(),
                        not_before,
                        expires,
                    },
                )
                .is_none()
            {
                cache.order.push_back(token);
            }
            while cache.entries.len() > self.config.cache_capacity {
                let Some(oldest) = cache.order.pop_front() else {
                    break;
                };
                cache.entries.remove(&oldest);
            }
        }
        Ok(principal)
    }
}

#[cfg(test)]
mod tests;
