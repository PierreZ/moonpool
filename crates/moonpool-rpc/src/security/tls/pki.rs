//! A small certificate authority for [`Tls`](super::Tls) sessions (feature
//! `tls-pki`).
//!
//! It makes the certificates a deployment needs and nothing more: a
//! self-signed root ([`Authority`]) and the leaves it signs
//! ([`Authority::issue`]), for servers, clients or both (mutual TLS). Keys
//! are Ed25519 only: fixed-size signatures keep every handshake the same
//! size, which a deterministic simulation needs, and signing needs no
//! randomness.
//!
//! The same code serves operators and simulations. A key comes from a
//! [`KeySeed`]: [`KeySeed::Random`] draws operating-system entropy (what a
//! command line uses); [`KeySeed::Bytes`] derives the key from 32 bytes (what
//! a simulation draws from its seed). Validity is explicit UTC, never the
//! host clock.
//!
//! **Simulation fault sites.** [`Authority::issue`] carries buggify sites that
//! issue a flawed leaf as a real operator might: valid for a moment only,
//! not valid yet (clock skew), without its first name, or for the wrong
//! use. The flaw is recorded in [`Issued::flaw`] and the validity and names
//! in [`Issued`] are what was really signed, so a harness judges outcomes
//! from the certificate, not from what it asked for. Outside a simulation
//! the sites are inert.

use std::net::IpAddr;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ED25519, SanType,
};
use time::OffsetDateTime;

use super::{CertificateDer, PrivateKeyDer, TlsError};
use crate::security::UtcTime;

/// Where a key comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeySeed {
    /// Operating-system entropy.
    Random,
    /// Derived from these bytes (an Ed25519 seed). The same bytes always
    /// give the same key: use it for simulations and tests only.
    Bytes([u8; 32]),
}

impl std::fmt::Debug for KeySeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Random => f.write_str("Random"),
            Self::Bytes(_) => f.write_str("Bytes(..)"),
        }
    }
}

/// `PKCS#8` v1 framing of an Ed25519 private key around its 32-byte seed
/// (RFC 8410).
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

impl KeySeed {
    fn key(self) -> Result<KeyPair, TlsError> {
        let key = match self {
            Self::Random => KeyPair::generate_for(&PKCS_ED25519),
            Self::Bytes(seed) => {
                let mut pkcs8 = ED25519_PKCS8_PREFIX.to_vec();
                pkcs8.extend_from_slice(&seed);
                KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8.into(), &PKCS_ED25519)
            }
        };
        key.map_err(|error| TlsError(format!("key: {error}")))
    }
}

/// A validity window, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    /// The first second it is valid.
    pub not_before: UtcTime,
    /// The last second it is valid.
    pub not_after: UtcTime,
}

impl Validity {
    /// From `not_before`, for `seconds`.
    #[must_use]
    pub fn starting(not_before: UtcTime, seconds: u64) -> Self {
        Self {
            not_before,
            not_after: UtcTime::from_unix_seconds(
                not_before.unix_seconds().saturating_add(seconds),
            ),
        }
    }

    /// Whether `now` is inside the window.
    #[must_use]
    pub fn contains(&self, now: UtcTime) -> bool {
        self.not_before <= now && now <= self.not_after
    }
}

fn date(time: UtcTime) -> Result<OffsetDateTime, TlsError> {
    i64::try_from(time.unix_seconds())
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .ok_or_else(|| TlsError(format!("time {} out of range", time.unix_seconds())))
}

/// A name a leaf proves (a subject alternative name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectName {
    /// A DNS name, such as `node-1.example.org`.
    Dns(String),
    /// A URI, such as `paros://universe/machine/node-1`.
    Uri(String),
    /// An IP address.
    Ip(IpAddr),
}

impl SubjectName {
    fn san(&self) -> Result<SanType, TlsError> {
        let ia5 = |text: &str| {
            text.to_string()
                .try_into()
                .map_err(|error| TlsError(format!("name {text}: {error}")))
        };
        Ok(match self {
            Self::Dns(name) => SanType::DnsName(ia5(name)?),
            Self::Uri(uri) => SanType::URI(ia5(uri)?),
            Self::Ip(ip) => SanType::IpAddress(*ip),
        })
    }
}

impl std::fmt::Display for SubjectName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dns(name) => write!(f, "dns:{name}"),
            Self::Uri(uri) => write!(f, "uri:{uri}"),
            Self::Ip(ip) => write!(f, "ip:{ip}"),
        }
    }
}

/// What a leaf may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    /// A server only.
    Server,
    /// A client only (mutual TLS).
    Client,
    /// Both: a process that accepts and dials, as a cell's machines do.
    Both,
}

impl Usage {
    fn purposes(self) -> Vec<ExtendedKeyUsagePurpose> {
        match self {
            Self::Server => vec![ExtendedKeyUsagePurpose::ServerAuth],
            Self::Client => vec![ExtendedKeyUsagePurpose::ClientAuth],
            Self::Both => vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
        }
    }

    /// Whether a leaf for this use may serve.
    #[must_use]
    pub fn serves(self) -> bool {
        matches!(self, Self::Server | Self::Both)
    }

    /// Whether a leaf for this use may dial as a client.
    #[must_use]
    pub fn dials(self) -> bool {
        matches!(self, Self::Client | Self::Both)
    }
}

/// What to issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafSpec {
    /// The names it proves, the identifying one first.
    pub names: Vec<SubjectName>,
    /// When it is valid.
    pub validity: Validity,
    /// What it is for.
    pub usage: Usage,
    /// Where its key comes from.
    pub seed: KeySeed,
}

/// A flaw a simulation fault site put in an issued leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flaw {
    /// Valid for one minute from the asked start only.
    ShortLived,
    /// Its window starts later than asked (clock skew at the issuer).
    NotYetValid,
    /// Its first name is missing.
    NameDropped,
    /// Issued for the other use (a server leaf for a client or the
    /// reverse).
    WrongUsage,
}

/// An issued leaf: the chain to serve or present (leaf first, then the
/// authority's certificate) and its key, plus what was really signed.
#[derive(Debug)]
pub struct Issued {
    /// The chain, leaf first.
    pub chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    /// The signed validity.
    pub validity: Validity,
    /// The signed names.
    pub names: Vec<SubjectName>,
    /// The signed use.
    pub usage: Usage,
    /// The simulation flaw, if a fault site put one in.
    pub flaw: Option<Flaw>,
}

impl Clone for Issued {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            key: self.key.clone_key(),
            validity: self.validity,
            names: self.names.clone(),
            usage: self.usage,
            flaw: self.flaw,
        }
    }
}

impl Issued {
    /// The private key.
    #[must_use]
    pub fn key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }

    /// The chain as PEM.
    #[must_use]
    pub fn chain_pem(&self) -> String {
        self.chain
            .iter()
            .map(|certificate| pem_block("CERTIFICATE", certificate))
            .collect()
    }

    /// The private key as PEM.
    #[must_use]
    pub fn key_pem(&self) -> String {
        pem_block("PRIVATE KEY", self.key.secret_der())
    }
}

/// A self-signed root that signs leaves.
pub struct Authority {
    name: String,
    certificate: CertificateDer<'static>,
    validity: Validity,
    key_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("name", &self.name)
            .field("validity", &self.validity)
            .finish_non_exhaustive()
    }
}

fn authority_params(name: &str, validity: Validity) -> Result<CertificateParams, TlsError> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|error| TlsError(format!("authority: {error}")))?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = date(validity.not_before)?;
    params.not_after = date(validity.not_after)?;
    Ok(params)
}

impl Authority {
    /// A new root named `name` (its common name), valid over `validity`.
    ///
    /// # Errors
    ///
    /// [`TlsError`] when the key or the certificate cannot be made.
    pub fn new(name: &str, validity: Validity, seed: KeySeed) -> Result<Self, TlsError> {
        let key = seed.key()?;
        let params = authority_params(name, validity)?;
        let certificate = params
            .self_signed(&key)
            .map_err(|error| TlsError(format!("authority: {error}")))?;
        Ok(Self {
            name: name.to_string(),
            certificate: certificate.der().clone(),
            validity,
            key_pem: key.serialize_pem(),
            issuer: Issuer::new(params, key),
        })
    }

    /// Load a root saved with [`certificate_pem`](Self::certificate_pem) and
    /// [`key_pem`](Self::key_pem).
    ///
    /// # Errors
    ///
    /// [`TlsError`] when either does not parse or they do not match.
    pub fn from_pem(certificate_pem: &str, key_pem: &str) -> Result<Self, TlsError> {
        let key = KeyPair::from_pem(key_pem).map_err(|error| TlsError(format!("key: {error}")))?;
        let certificate = read_certificates(certificate_pem.as_bytes())?
            .into_iter()
            .next()
            .ok_or_else(|| TlsError("no certificate in the authority file".into()))?;
        let names = super::identity::CertificateNames::of(&certificate);
        let issuer = Issuer::from_ca_cert_der(&certificate, key)
            .map_err(|error| TlsError(format!("authority: {error}")))?;
        let parsed = x509_summary(&certificate)?;
        if !names.uris.is_empty() || !names.dns.is_empty() {
            return Err(TlsError(
                "the authority file holds a leaf, not an authority".into(),
            ));
        }
        Ok(Self {
            name: parsed.0,
            certificate,
            validity: parsed.1,
            key_pem: key_pem.to_string(),
            issuer,
        })
    }

    /// Its common name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its validity.
    #[must_use]
    pub fn validity(&self) -> Validity {
        self.validity
    }

    /// Its certificate: the trust anchor to give clients and servers.
    #[must_use]
    pub fn certificate(&self) -> &CertificateDer<'static> {
        &self.certificate
    }

    /// Its certificate as PEM.
    #[must_use]
    pub fn certificate_pem(&self) -> String {
        pem_block("CERTIFICATE", &self.certificate)
    }

    /// Its private key as PEM: keep it offline.
    #[must_use]
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// Issue a leaf for `spec`.
    ///
    /// In a simulation a fault site may issue it with a [`Flaw`]; the
    /// returned [`Issued`] says what was really signed.
    ///
    /// # Errors
    ///
    /// [`TlsError`] when the spec names nothing or the certificate cannot be
    /// made.
    pub fn issue(&self, spec: &LeafSpec) -> Result<Issued, TlsError> {
        if spec.names.is_empty() {
            return Err(TlsError("a leaf needs at least one name".into()));
        }
        let mut validity = spec.validity;
        let mut names = spec.names.clone();
        let mut usage = spec.usage;
        let flaw = match moonpool_buggify::buggify_pick!(0.1, 4) {
            Some(0) => {
                validity = Validity::starting(validity.not_before, 60);
                Some(Flaw::ShortLived)
            }
            Some(1) => {
                let late = validity.not_before.unix_seconds().saturating_add(3600);
                validity.not_before = UtcTime::from_unix_seconds(late);
                validity.not_after = validity.not_after.max(validity.not_before);
                Some(Flaw::NotYetValid)
            }
            Some(2) if names.len() > 1 => {
                names.remove(0);
                Some(Flaw::NameDropped)
            }
            Some(3) => {
                usage = match usage {
                    Usage::Server => Usage::Client,
                    Usage::Client | Usage::Both => Usage::Server,
                };
                Some(Flaw::WrongUsage)
            }
            _ => None,
        };
        let key = spec.seed.key()?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|error| TlsError(format!("leaf: {error}")))?;
        params.subject_alt_names = names
            .iter()
            .map(SubjectName::san)
            .collect::<Result<_, _>>()?;
        params
            .distinguished_name
            .push(DnType::CommonName, names[0].to_string());
        params.not_before = date(validity.not_before)?;
        params.not_after = date(validity.not_after)?;
        params.extended_key_usages = usage.purposes();
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.use_authority_key_identifier_extension = true;
        let leaf = params
            .signed_by(&key, &self.issuer)
            .map_err(|error| TlsError(format!("leaf: {error}")))?;
        Ok(Issued {
            chain: vec![leaf.der().clone(), self.certificate.clone()],
            key: PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            validity,
            names,
            usage,
            flaw,
        })
    }
}

/// The common name and validity of an authority certificate.
fn x509_summary(certificate: &CertificateDer<'_>) -> Result<(String, Validity), TlsError> {
    let (_, parsed) = x509_parser::parse_x509_certificate(certificate)
        .map_err(|error| TlsError(format!("certificate: {error}")))?;
    let name = parsed
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or_default()
        .to_string();
    let seconds = |time: x509_parser::time::ASN1Time| {
        UtcTime::from_unix_seconds(u64::try_from(time.timestamp()).unwrap_or(0))
    };
    let validity = Validity {
        not_before: seconds(parsed.validity().not_before),
        not_after: seconds(parsed.validity().not_after),
    };
    Ok((name, validity))
}

fn pem_block(label: &str, der: &[u8]) -> String {
    use std::fmt::Write as _;
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(der.len().div_ceil(3) * 4);
    for chunk in der.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let triple = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (triple >> (18 - 6 * index)) & 0x3f;
                encoded.push(char::from(ALPHABET[sextet as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        let _ = writeln!(out, "{}", String::from_utf8_lossy(line));
    }
    let _ = writeln!(out, "-----END {label}-----");
    out
}

/// Every certificate in a PEM file, in order.
///
/// # Errors
///
/// [`TlsError`] when the file holds no certificate or a block does not
/// parse.
pub fn read_certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    use futures_rustls::rustls::pki_types::pem::PemObject as _;
    let certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError(format!("certificate PEM: {error}")))?;
    if certificates.is_empty() {
        return Err(TlsError("no certificate in the PEM file".into()));
    }
    Ok(certificates)
}

/// The private key in a PEM file.
///
/// # Errors
///
/// [`TlsError`] when the file holds no key.
pub fn read_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, TlsError> {
    use futures_rustls::rustls::pki_types::pem::PemObject as _;
    PrivateKeyDer::from_pem_slice(pem).map_err(|error| TlsError(format!("key PEM: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::tls::identity::CertificateNames;

    fn window() -> Validity {
        Validity::starting(UtcTime::from_unix_seconds(1_900_000_000), 86_400)
    }

    #[test]
    fn seeded_keys_are_reproducible() {
        let a = Authority::new("ca", window(), KeySeed::Bytes([7; 32])).expect("ca");
        let b = Authority::new("ca", window(), KeySeed::Bytes([7; 32])).expect("ca");
        assert_eq!(a.certificate(), b.certificate());
        let spec = LeafSpec {
            names: vec![SubjectName::Uri("paros://u/machine/a".into())],
            validity: window(),
            usage: Usage::Both,
            seed: KeySeed::Bytes([9; 32]),
        };
        let one = a.issue(&spec).expect("leaf");
        let two = b.issue(&spec).expect("leaf");
        assert_eq!(one.chain, two.chain);
        assert_eq!(one.flaw, None);
    }

    #[test]
    fn a_leaf_proves_its_names() {
        let ca = Authority::new("ca", window(), KeySeed::Random).expect("ca");
        let leaf = ca
            .issue(&LeafSpec {
                names: vec![
                    SubjectName::Uri("paros://u/machine/a".into()),
                    SubjectName::Dns("a.test".into()),
                    SubjectName::Ip("10.0.0.1".parse().expect("ip")),
                ],
                validity: window(),
                usage: Usage::Server,
                seed: KeySeed::Random,
            })
            .expect("leaf");
        let names = CertificateNames::of(&leaf.chain[0]);
        assert_eq!(names.uris, vec!["paros://u/machine/a".to_string()]);
        assert_eq!(names.dns, vec!["a.test".to_string()]);
        assert_eq!(names.ips, vec!["10.0.0.1".parse::<IpAddr>().expect("ip")]);
        assert_eq!(names.primary().as_deref(), Some("paros://u/machine/a"));
        assert!(CertificateNames::of(ca.certificate()).primary().is_none());
    }

    #[test]
    fn pem_round_trips() {
        let ca = Authority::new("paros-ca", window(), KeySeed::Random).expect("ca");
        let again = Authority::from_pem(&ca.certificate_pem(), ca.key_pem()).expect("reload");
        assert_eq!(again.certificate(), ca.certificate());
        assert_eq!(again.name(), "paros-ca");
        assert_eq!(again.validity(), window());
        let leaf = again
            .issue(&LeafSpec {
                names: vec![SubjectName::Dns("a.test".into())],
                validity: window(),
                usage: Usage::Client,
                seed: KeySeed::Random,
            })
            .expect("leaf");
        let chain = read_certificates(leaf.chain_pem().as_bytes()).expect("chain");
        assert_eq!(chain, leaf.chain);
        let key = read_key(leaf.key_pem().as_bytes()).expect("key");
        assert_eq!(key.secret_der(), leaf.key().secret_der());
        assert!(read_certificates(b"nothing").is_err());
    }
}
