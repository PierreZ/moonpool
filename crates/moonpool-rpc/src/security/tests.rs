//! The security decision without a runtime: verification, policy, the
//! credential section, transports and versions.

use super::{
    AccessPolicy, AccessRequest, CredentialError, Denial, FixedUtc, Principal, RequestOrigin,
    RequestVerifier, SecurityConfig, UtcTime,
};
use crate::endpoint::{AccessClass, EndpointToken};
use crate::interface::InterfaceId;
use crate::protocol::metadata::encode_bearer;
use crate::protocol::{MethodId, SchemaVersion};
use crate::transport::upgrade::PeerContext;

/// Accepts `good:<subject>`, expiring at 1000; refuses anything else as a
/// bad signature. Not cryptography: a scripted verifier for the policy.
struct Scripted;

impl RequestVerifier for Scripted {
    fn verify(
        &self,
        request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError> {
        let now = request.now.ok_or(CredentialError::ClockUnavailable)?;
        if now >= UtcTime::from_unix_seconds(1000) {
            return Err(CredentialError::Expired);
        }
        let text = std::str::from_utf8(credential).map_err(|_| CredentialError::Malformed)?;
        let subject = text
            .strip_prefix("good:")
            .ok_or(CredentialError::BadSignature)?;
        Ok(Principal::new(subject).with_scopes(["read"]))
    }
}

/// Private endpoints need the `admin` scope.
struct NeedsAdmin;

impl AccessPolicy for NeedsAdmin {
    fn authorize(
        &self,
        request: &AccessRequest<'_>,
        principal: Option<&Principal>,
    ) -> Result<(), Denial> {
        match (request.access, principal) {
            (AccessClass::Public, _) => Ok(()),
            (AccessClass::Private, None) => Err(Denial::Unauthenticated(CredentialError::Missing)),
            (AccessClass::Private, Some(principal)) if principal.has_scope("admin") => Ok(()),
            (AccessClass::Private, Some(_)) => Err(Denial::PermissionDenied),
        }
    }
}

fn request(origin: RequestOrigin<'_>, access: AccessClass, now: u64) -> AccessRequest<'_> {
    AccessRequest {
        origin,
        access,
        token: EndpointToken::from_parts(1, 1),
        interface: (InterfaceId::new(0), SchemaVersion::new(0)),
        method: MethodId::new(1),
        now: Some(UtcTime::from_unix_seconds(now)),
    }
}

fn section(token: &str) -> Vec<u8> {
    encode_bearer(token.as_bytes()).expect("fits")
}

#[test]
fn the_default_admits_public_callers_and_refuses_every_private_one() {
    let config = SecurityConfig::default();
    let peer = PeerContext::new("10.0.0.1:1");
    let remote = RequestOrigin::Remote(&peer);
    for origin in [RequestOrigin::Local, remote] {
        assert_eq!(
            config.decide(&request(origin, AccessClass::Public, 1), Some(&[])),
            Ok(None)
        );
        assert_eq!(
            config.decide(&request(origin, AccessClass::Private, 1), Some(&[])),
            Err(Denial::Unauthenticated(CredentialError::Missing)),
            "private fails closed"
        );
        // A credential nothing can verify grants nothing.
        assert_eq!(
            config.decide(
                &request(origin, AccessClass::Private, 1),
                Some(&section("good:alice"))
            ),
            Err(Denial::Unauthenticated(CredentialError::Missing))
        );
    }
    assert!(!config.verifies_credentials());
}

#[test]
fn a_trusted_network_admits_everything_and_verifies_nothing() {
    let config = SecurityConfig::trusted_network();
    let peer = PeerContext::new("10.0.0.1:1");
    for access in [AccessClass::Public, AccessClass::Private] {
        for section in [None, Some(&[0xFF_u8][..])] {
            assert_eq!(
                config.decide(&request(RequestOrigin::Remote(&peer), access, 1), section),
                Ok(None)
            );
        }
    }
    assert!(config.is_trusted_network());
    assert!(!config.verifies_credentials());
}

#[test]
fn verified_credentials_open_private_endpoints_locally_and_remotely() {
    let config = SecurityConfig::enforced(Scripted).accept_credentials_over_plaintext();
    let plain = PeerContext::new("10.0.0.1:1");
    for origin in [RequestOrigin::Local, RequestOrigin::Remote(&plain)] {
        let principal = config
            .decide(
                &request(origin, AccessClass::Private, 5),
                Some(&section("good:alice")),
            )
            .expect("verified")
            .expect("a principal");
        assert_eq!(principal.subject(), "alice");
        assert!(principal.has_scope("read"));
        // Local and remote get the same answers.
        assert_eq!(
            config.decide(
                &request(origin, AccessClass::Private, 5),
                Some(&section("forged"))
            ),
            Err(Denial::Unauthenticated(CredentialError::BadSignature))
        );
        assert_eq!(
            config.decide(
                &request(origin, AccessClass::Private, 1000),
                Some(&section("good:alice"))
            ),
            Err(Denial::Unauthenticated(CredentialError::Expired))
        );
        assert_eq!(
            config.decide(&request(origin, AccessClass::Private, 5), Some(&[])),
            Err(Denial::Unauthenticated(CredentialError::Missing))
        );
        // A bad credential is refused even for a public endpoint: public
        // means "subject to verification", not "unchecked".
        assert_eq!(
            config.decide(
                &request(origin, AccessClass::Public, 5),
                Some(&section("forged"))
            ),
            Err(Denial::Unauthenticated(CredentialError::BadSignature))
        );
    }
    assert!(config.verifies_credentials());
}

#[test]
fn malformed_and_oversized_sections_are_refused_before_the_verifier() {
    let config = SecurityConfig::enforced(Scripted)
        .accept_credentials_over_plaintext()
        .with_max_credential_bytes(16);
    let origin = RequestOrigin::Local;
    assert_eq!(
        config.decide(
            &request(origin, AccessClass::Public, 5),
            Some(&section("good:a-much-longer-subject"))
        ),
        Err(Denial::Unauthenticated(CredentialError::TooLarge))
    );
    assert_eq!(
        config.decide(
            &request(origin, AccessClass::Public, 5),
            Some(&[7, 1, 0, 0])
        ),
        Err(Denial::Unauthenticated(CredentialError::Malformed))
    );
}

#[test]
fn bearer_credentials_need_an_encrypted_session_unless_opted_out() {
    let config = SecurityConfig::enforced(Scripted);
    let plain = PeerContext::new("10.0.0.1:1");
    let tls = PeerContext::new("10.0.0.1:1").with_protection("TLSv1_3");
    assert_eq!(
        config.decide(
            &request(RequestOrigin::Remote(&plain), AccessClass::Private, 5),
            Some(&section("good:alice"))
        ),
        Err(Denial::Unauthenticated(CredentialError::InsecureTransport))
    );
    assert!(
        config
            .decide(
                &request(RequestOrigin::Remote(&tls), AccessClass::Private, 5),
                Some(&section("good:alice"))
            )
            .is_ok_and(|principal| principal.is_some())
    );
    assert!(
        config
            .decide(
                &request(RequestOrigin::Local, AccessClass::Private, 5),
                Some(&section("good:alice"))
            )
            .is_ok(),
        "a local call never crosses a network"
    );
}

#[test]
fn a_version_one_session_carries_no_credential() {
    let config = SecurityConfig::enforced(Scripted).accept_credentials_over_plaintext();
    let peer = PeerContext::new("10.0.0.1:1");
    let origin = RequestOrigin::Remote(&peer);
    assert_eq!(
        config.decide(&request(origin, AccessClass::Private, 5), None),
        Err(Denial::Unauthenticated(CredentialError::Missing))
    );
    assert_eq!(
        config.decide(&request(origin, AccessClass::Public, 5), None),
        Ok(None)
    );
}

#[test]
fn the_policy_decides_after_verification() {
    let config = SecurityConfig::enforced(Scripted)
        .accept_credentials_over_plaintext()
        .with_policy(NeedsAdmin);
    let origin = RequestOrigin::Local;
    assert_eq!(
        config.decide(
            &request(origin, AccessClass::Private, 5),
            Some(&section("good:alice"))
        ),
        Err(Denial::PermissionDenied)
    );
    assert!(
        config
            .decide(
                &request(origin, AccessClass::Public, 5),
                Some(&section("good:alice"))
            )
            .is_ok()
    );
}

#[test]
fn the_clock_is_the_configured_one_and_unknown_time_fails_closed() {
    let clock = FixedUtc::new(UtcTime::from_unix_seconds(5));
    let config = SecurityConfig::enforced(Scripted)
        .accept_credentials_over_plaintext()
        .with_clock(clock.clone());
    assert_eq!(config.now_utc(), Some(UtcTime::from_unix_seconds(5)));
    clock.forget();
    let mut unknown = request(RequestOrigin::Local, AccessClass::Private, 0);
    unknown.now = config.now_utc();
    assert_eq!(
        config.decide(&unknown, Some(&section("good:alice"))),
        Err(Denial::Unauthenticated(CredentialError::ClockUnavailable))
    );
}

#[test]
fn credentials_and_configs_never_print_their_secrets() {
    let credential = super::Credential::bearer("s3cr3t-token");
    let printed = format!("{credential:?}");
    assert!(!printed.contains("s3cr3t"), "{printed}");
    assert!(printed.contains("12 bytes"));
    let config = SecurityConfig::enforced(Scripted).with_credentials(credential);
    let printed = format!("{config:?}");
    assert!(!printed.contains("s3cr3t"), "{printed}");
    assert_eq!(config.clone(), config);
    assert_ne!(config, SecurityConfig::default());
}
