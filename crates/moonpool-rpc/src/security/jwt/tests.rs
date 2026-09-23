//! The JWT/JWKS verifier against real signatures, with an injected UTC.

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{EncodingKey, Header, encode};
use serde_json::json;

use super::{Algorithm, JwksError, JwksKeys, JwtConfig, JwtVerifier};
use crate::endpoint::{AccessClass, EndpointToken};
use crate::interface::InterfaceId;
use crate::protocol::{MethodId, SchemaVersion};
use crate::security::{AccessRequest, CredentialError, RequestOrigin, RequestVerifier, UtcTime};

const NOW: u64 = 1_900_000_000;

/// An Ed25519 key from a fixed seed (PKCS#8 v1 DER), and its JWK.
fn ed_key(seed: u8, kid: &str) -> (EncodingKey, Jwk) {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend([seed; 32]);
    let key = EncodingKey::from_ed_der(&der);
    super::install_provider();
    let mut jwk = Jwk::from_encoding_key(&key, Algorithm::EdDSA).expect("jwk");
    jwk.common.key_id = Some(kid.into());
    (key, jwk)
}

/// A fresh P-256 key and its JWK.
fn ec_key(kid: &str) -> (EncodingKey, Jwk) {
    let pair = rcgen::KeyPair::generate().expect("key");
    let key = EncodingKey::from_ec_der(&pair.serialize_der());
    super::install_provider();
    let mut jwk = Jwk::from_encoding_key(&key, Algorithm::ES256).expect("jwk");
    jwk.common.key_id = Some(kid.into());
    (key, jwk)
}

fn token(
    key: &EncodingKey,
    alg: Algorithm,
    kid: Option<&str>,
    claims: &serde_json::Value,
) -> Vec<u8> {
    let mut header = Header::new(alg);
    header.kid = kid.map(Into::into);
    encode(&header, claims, key).expect("sign").into_bytes()
}

fn claims(exp: u64) -> serde_json::Value {
    json!({"sub": "alice", "iss": "moonpool", "aud": "rpc", "exp": exp, "scope": "read write"})
}

fn verifier(set: Vec<Jwk>, tweak: impl FnOnce(&mut JwtConfig)) -> JwtVerifier {
    let mut config = JwtConfig::new("moonpool", "rpc");
    tweak(&mut config);
    let keys = JwksKeys::from_set(&JwkSet { keys: set }).expect("keys");
    JwtVerifier::new(keys, config).expect("config")
}

fn at(now: Option<u64>) -> AccessRequest<'static> {
    AccessRequest {
        origin: RequestOrigin::Local,
        access: AccessClass::Private,
        token: EndpointToken::from_parts(1, 1),
        interface: (InterfaceId::new(0), SchemaVersion::new(0)),
        method: MethodId::new(1),
        now: now.map(UtcTime::from_unix_seconds),
    }
}

fn check(verifier: &JwtVerifier, token: &[u8], now: u64) -> Result<String, CredentialError> {
    verifier
        .verify(&at(Some(now)), token)
        .map(|principal| principal.subject().to_string())
}

#[test]
fn valid_tokens_become_principals() {
    let (ed, edwards_jwk) = ed_key(1, "ed");
    let (ec, p256_jwk) = ec_key("ec");
    let verifier = verifier(vec![edwards_jwk, p256_jwk], |_| {});
    for token in [
        token(&ed, Algorithm::EdDSA, Some("ed"), &claims(NOW + 60)),
        token(&ec, Algorithm::ES256, Some("ec"), &claims(NOW + 60)),
    ] {
        let principal = verifier.verify(&at(Some(NOW)), &token).expect("valid");
        assert_eq!(principal.subject(), "alice");
        assert_eq!(principal.issuer(), Some("moonpool"));
        assert!(principal.has_scope("write"));
        assert_eq!(
            principal.expires_at(),
            Some(UtcTime::from_unix_seconds(NOW + 60))
        );
    }
    // Deterministic signatures: the same claims sign to the same token.
    assert_eq!(
        token(&ed, Algorithm::EdDSA, Some("ed"), &claims(1)),
        token(&ed, Algorithm::EdDSA, Some("ed"), &claims(1))
    );
}

#[test]
fn expiry_and_not_before_follow_the_injected_clock() {
    let (ed, jwk) = ed_key(2, "k");
    let strict = verifier(vec![jwk.clone()], |_| {});
    let lenient = verifier(vec![jwk], |config| config.leeway_seconds = 30);
    let expiring = token(&ed, Algorithm::EdDSA, Some("k"), &claims(NOW));
    assert_eq!(check(&strict, &expiring, NOW - 1), Ok("alice".into()));
    assert_eq!(
        check(&strict, &expiring, NOW),
        Err(CredentialError::Expired)
    );
    assert_eq!(check(&lenient, &expiring, NOW + 29), Ok("alice".into()));
    assert_eq!(
        check(&lenient, &expiring, NOW + 30),
        Err(CredentialError::Expired)
    );
    let mut later = claims(NOW + 600);
    later["nbf"] = json!(NOW + 100);
    let later = token(&ed, Algorithm::EdDSA, Some("k"), &later);
    assert_eq!(
        check(&strict, &later, NOW + 99),
        Err(CredentialError::NotYetValid)
    );
    assert_eq!(check(&strict, &later, NOW + 100), Ok("alice".into()));
    assert_eq!(check(&lenient, &later, NOW + 70), Ok("alice".into()));
    assert_eq!(
        strict.verify(&at(None), &later).map(|_| ()),
        Err(CredentialError::ClockUnavailable),
        "no clock, no acceptance"
    );
}

#[test]
fn wrong_claims_signatures_keys_and_algorithms_are_refused() {
    let (ed, edwards_jwk) = ed_key(3, "a");
    let (other, _) = ed_key(4, "b");
    let (ec, p256_jwk) = ec_key("ec");
    let verifier = verifier(vec![edwards_jwk, p256_jwk], |_| {});
    let mut wrong_issuer = claims(NOW + 60);
    wrong_issuer["iss"] = json!("mallory");
    let mut wrong_audience = claims(NOW + 60);
    wrong_audience["aud"] = json!("other");
    let mut no_subject = claims(NOW + 60);
    no_subject
        .as_object_mut()
        .map(|claims| claims.remove("sub"));
    let cases = [
        (
            token(&ed, Algorithm::EdDSA, Some("a"), &wrong_issuer),
            CredentialError::WrongIssuer,
        ),
        (
            token(&ed, Algorithm::EdDSA, Some("a"), &wrong_audience),
            CredentialError::WrongAudience,
        ),
        (
            token(&ed, Algorithm::EdDSA, Some("a"), &no_subject),
            CredentialError::Malformed,
        ),
        // Signed by another key claiming to be `a`.
        (
            token(&other, Algorithm::EdDSA, Some("a"), &claims(NOW + 60)),
            CredentialError::BadSignature,
        ),
        (
            token(&ed, Algorithm::EdDSA, Some("nope"), &claims(NOW + 60)),
            CredentialError::UnknownKey,
        ),
        (
            token(&ed, Algorithm::EdDSA, None, &claims(NOW + 60)),
            CredentialError::Malformed,
        ),
        // The EdDSA token names the P-256 key: algorithm mismatch.
        (
            token(&ed, Algorithm::EdDSA, Some("ec"), &claims(NOW + 60)),
            CredentialError::UnsupportedAlgorithm,
        ),
        // Symmetric-key confusion: HS256 is never allowed.
        (
            token(
                &EncodingKey::from_secret(b"a"),
                Algorithm::HS256,
                Some("a"),
                &claims(NOW + 60),
            ),
            CredentialError::UnsupportedAlgorithm,
        ),
        (b"not.a.jwt".to_vec(), CredentialError::Malformed),
        (b"garbage".to_vec(), CredentialError::Malformed),
    ];
    for (token, expected) in cases {
        assert_eq!(check(&verifier, &token, NOW), Err(expected));
    }
    // ES256 allowed but a key's algorithm outside the allow list is not.
    let ed_only = self::verifier(vec![ed_key(3, "a").1], |config| {
        config.algorithms = vec![Algorithm::EdDSA];
    });
    let ec_token = token(&ec, Algorithm::ES256, Some("ec"), &claims(NOW + 60));
    assert_eq!(
        check(&ed_only, &ec_token, NOW),
        Err(CredentialError::UnsupportedAlgorithm)
    );
}

#[test]
fn tokens_are_bounded_before_parsing() {
    let (ed, jwk) = ed_key(5, "k");
    let verifier = verifier(vec![jwk], |config| config.max_token_bytes = 64);
    let token = token(&ed, Algorithm::EdDSA, Some("k"), &claims(NOW + 60));
    assert!(token.len() > 64);
    assert_eq!(
        check(&verifier, &token, NOW),
        Err(CredentialError::TooLarge)
    );
}

#[test]
fn rotation_revokes_keys_and_invalidates_the_cache() {
    let (old, old_jwk) = ed_key(6, "old");
    let (new, new_jwk) = ed_key(7, "new");
    let verifier = verifier(vec![old_jwk], |_| {});
    let old_token = token(&old, Algorithm::EdDSA, Some("old"), &claims(NOW + 60));
    let new_token = token(&new, Algorithm::EdDSA, Some("new"), &claims(NOW + 60));
    assert_eq!(check(&verifier, &old_token, NOW), Ok("alice".into()));
    assert_eq!(verifier.cached(), 1);
    assert_eq!(
        check(&verifier, &new_token, NOW),
        Err(CredentialError::UnknownKey)
    );
    let keys = verifier.keys().clone();
    let set = serde_json::to_string(&JwkSet {
        keys: vec![new_jwk],
    })
    .expect("json");
    assert_eq!(keys.replace_json(&set), Ok(2));
    assert_eq!(keys.key_ids(), ["new"]);
    assert_eq!(
        check(&verifier, &old_token, NOW),
        Err(CredentialError::UnknownKey),
        "a cached token of a revoked key is refused"
    );
    assert_eq!(check(&verifier, &new_token, NOW), Ok("alice".into()));
    // A cache hit still expires on time.
    assert_eq!(
        check(&verifier, &new_token, NOW + 60),
        Err(CredentialError::Expired)
    );
}

#[test]
fn the_cache_is_bounded() {
    let (ed, jwk) = ed_key(8, "k");
    let verifier = verifier(vec![jwk], |config| config.cache_capacity = 2);
    for exp in 1..=5 {
        let token = token(&ed, Algorithm::EdDSA, Some("k"), &claims(NOW + exp));
        assert_eq!(check(&verifier, &token, NOW), Ok("alice".into()));
        assert!(verifier.cached() <= 2);
    }
    assert_eq!(verifier.cached(), 2);
    let uncached = self::verifier(vec![ed_key(8, "k").1], |config| config.cache_capacity = 0);
    let token = token(&ed, Algorithm::EdDSA, Some("k"), &claims(NOW + 60));
    assert_eq!(check(&uncached, &token, NOW), Ok("alice".into()));
    assert_eq!(uncached.cached(), 0);
}

#[test]
fn unusable_key_sets_are_refused_whole() {
    let (_, good) = ed_key(9, "good");
    let keys = JwksKeys::from_set(&JwkSet {
        keys: vec![good.clone()],
    })
    .expect("keys");
    let secret = json!({"keys": [{"kty": "oct", "kid": "s", "k": "c2VjcmV0"}]}).to_string();
    assert!(matches!(
        keys.replace_json(&secret),
        Err(JwksError::Unsupported { .. })
    ));
    let mut anonymous = good.clone();
    anonymous.common.key_id = None;
    assert_eq!(
        keys.replace_set(&JwkSet {
            keys: vec![anonymous]
        }),
        Err(JwksError::KeyId)
    );
    assert_eq!(
        keys.replace_set(&JwkSet {
            keys: vec![good.clone(), good.clone()]
        }),
        Err(JwksError::KeyId)
    );
    assert!(matches!(keys.replace_json("{"), Err(JwksError::Parse(_))));
    let huge = " ".repeat(super::MAX_JWKS_BYTES + 1);
    assert!(matches!(
        keys.replace_json(&huge),
        Err(JwksError::TooLarge(_))
    ));
    assert_eq!(
        keys.replace_set(&JwkSet {
            keys: vec![good; super::MAX_JWKS_KEYS + 1]
        }),
        Err(JwksError::TooManyKeys(super::MAX_JWKS_KEYS + 1))
    );
    assert_eq!(keys.key_ids(), ["good"], "a refused set changes nothing");
    assert_eq!(keys.generation(), 1);
}

#[test]
fn configurations_that_cannot_verify_are_refused() {
    let keys = || JwksKeys::from_set(&JwkSet { keys: Vec::new() }).expect("empty set");
    let mut symmetric = JwtConfig::new("i", "a");
    symmetric.algorithms.push(Algorithm::HS256);
    assert!(JwtVerifier::new(keys(), symmetric).is_err());
    let mut no_issuer = JwtConfig::new("i", "a");
    no_issuer.issuers.clear();
    assert!(JwtVerifier::new(keys(), no_issuer).is_err());
    let mut nothing = JwtConfig::new("i", "a");
    nothing.algorithms.clear();
    assert!(JwtVerifier::new(keys(), nothing).is_err());
}

/// Base64url without padding, for hand-made JWKs.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let value =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        for index in 0..=chunk.len() {
            let shift = 18 - 6 * index;
            out.push(char::from(ALPHABET[((value >> shift) & 0x3f) as usize]));
        }
    }
    out
}

/// Regression: tokens with an empty subject, an unexpected `typ` or a
/// critical header extension were accepted.
#[test]
fn empty_subjects_foreign_types_and_critical_extensions_are_refused() {
    let (ed, jwk) = ed_key(10, "k");
    let verifier = verifier(vec![jwk], |_| {});
    let mut empty = claims(NOW + 60);
    empty["sub"] = json!("");
    assert_eq!(
        check(
            &verifier,
            &token(&ed, Algorithm::EdDSA, Some("k"), &empty),
            NOW
        ),
        Err(CredentialError::Malformed)
    );
    let typed = |typ: Option<&str>, crit: Option<Vec<String>>| {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("k".into());
        header.typ = typ.map(Into::into);
        header.crit = crit;
        encode(&header, &claims(NOW + 60), &ed)
            .expect("sign")
            .into_bytes()
    };
    for accepted in [None, Some("JWT"), Some("jwt"), Some("at+jwt")] {
        assert_eq!(
            check(&verifier, &typed(accepted, None), NOW),
            Ok("alice".into()),
            "{accepted:?}"
        );
    }
    assert_eq!(
        check(&verifier, &typed(Some("dpop+jwt"), None), NOW),
        Err(CredentialError::Malformed)
    );
    assert_eq!(
        check(&verifier, &typed(None, Some(vec!["exp".into()])), NOW),
        Err(CredentialError::Malformed)
    );
}

/// Regression: RSA keys of any size were published.
#[test]
fn short_rsa_keys_are_refused() {
    let rsa = |bits: usize| {
        let mut modulus = vec![0xff_u8; bits / 8];
        modulus[0] = 0xc1;
        serde_json::json!({"keys": [{
            "kty": "RSA", "kid": format!("rsa{bits}"), "alg": "RS256",
            "n": b64url(&modulus), "e": "AQAB",
        }]})
        .to_string()
    };
    assert!(matches!(
        JwksKeys::from_json(&rsa(1024)),
        Err(JwksError::Unsupported { .. })
    ));
    assert!(JwksKeys::from_json(&rsa(2048)).is_ok());
    assert_eq!(super::modulus_bits(&[0, 0, 1]), 1);
    assert_eq!(super::modulus_bits(&[0x80, 0]), 16);
    assert_eq!(super::modulus_bits(&[]), 0);
}

/// Seeded mutations of a valid token (bit flips, byte overwrites,
/// truncations, insertions, swapped segments, random bytes): never a
/// panic, never a principal other than the signed one, and the cache stays
/// within its bound whatever arrives.
#[test]
fn mutated_tokens_never_panic_nor_verify_as_someone_else() {
    let (ed, jwk) = ed_key(9, "k");
    let verifier = verifier(vec![jwk], |config| config.cache_capacity = 8);
    let valid = token(&ed, Algorithm::EdDSA, Some("k"), &claims(NOW + 600));
    let mut state = 0x5eed_0006_u64;
    let mut next = |bound: usize| -> usize {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state % u64::try_from(bound.max(1)).unwrap_or(1)).unwrap_or(0)
    };
    let mut refused = 0;
    for _ in 0..1000 {
        let mut input = valid.clone();
        match next(6) {
            0 => {
                let at = next(input.len());
                input[at] ^= 1 << next(8);
            }
            1 => {
                let at = next(input.len());
                input[at] = u8::try_from(next(256)).unwrap_or(0);
            }
            2 => input.truncate(next(input.len())),
            3 => {
                let at = next(input.len());
                let byte = [b'.', b'=', b'A', 0, 0xff][next(5)];
                input.insert(at, byte);
            }
            4 => {
                // Swap two of the three segments.
                let text = String::from_utf8(input.clone()).unwrap_or_default();
                let mut parts: Vec<&str> = text.split('.').collect();
                if parts.len() == 3 {
                    let (a, b) = (next(3), next(3));
                    parts.swap(a, b);
                }
                input = parts.join(".").into_bytes();
            }
            _ => {
                input = (0..next(300))
                    .map(|_| u8::try_from(next(256)).unwrap_or(0))
                    .collect();
            }
        }
        match check(&verifier, &input, NOW) {
            Ok(subject) => assert_eq!(subject, "alice", "only the signed claims verify"),
            Err(_) => refused += 1,
        }
        assert!(verifier.cached() <= 8, "the cache stays bounded");
    }
    assert!(refused > 750, "mutations are refused: {refused}");
}
