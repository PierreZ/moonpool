//! The names a verified certificate proves: its subject alternative names.
//!
//! rustls and webpki verify the chain but expose only DNS names; the URI
//! and IP names an application uses as identities need the extension
//! itself. This is a minimal DER reader for exactly that path
//! (`tbsCertificate.extensions` → `subjectAltName`). It reads a certificate
//! that the handshake already verified, and it fails closed: anything it
//! does not understand yields no name.

use std::net::IpAddr;

/// The subject alternative names of a certificate, in certificate order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CertificateNames {
    /// `uniformResourceIdentifier` names (for example `spiffe://…` or
    /// `paros://…`).
    pub uris: Vec<String>,
    /// `dNSName` names.
    pub dns: Vec<String>,
    /// `iPAddress` names.
    pub ips: Vec<IpAddr>,
}

impl CertificateNames {
    /// The names of `certificate` (DER). Empty when it has none or cannot
    /// be read.
    #[must_use]
    pub fn of(certificate: &[u8]) -> Self {
        read(certificate).unwrap_or_default()
    }

    /// The name that identifies the peer: the first URI, else the first DNS
    /// name, else the first IP address.
    #[must_use]
    pub fn primary(&self) -> Option<String> {
        self.uris
            .first()
            .cloned()
            .or_else(|| self.dns.first().cloned())
            .or_else(|| self.ips.first().map(ToString::to_string))
    }
}

/// `2.5.29.17`, the subject alternative name extension.
const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

const SEQUENCE: u8 = 0x30;
const OID: u8 = 0x06;
const BOOLEAN: u8 = 0x01;
const OCTET_STRING: u8 = 0x04;
/// `[3] EXPLICIT`, the extensions of a version 3 `tbsCertificate`.
const EXTENSIONS: u8 = 0xa3;
const DNS_NAME: u8 = 0x82;
const URI: u8 = 0x86;
const IP_ADDRESS: u8 = 0x87;

/// One DER element: its tag and contents, and what follows it.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, rest) = rest.split_at(count);
        let length = bytes
            .iter()
            .fold(0usize, |length, &byte| (length << 8) | usize::from(byte));
        (length, rest)
    };
    if rest.len() < length {
        return None;
    }
    let (contents, rest) = rest.split_at(length);
    Some((tag, contents, rest))
}

fn contents_of(input: &[u8], expected: u8) -> Option<&[u8]> {
    match element(input)? {
        (tag, contents, _) if tag == expected => Some(contents),
        _ => None,
    }
}

fn read(certificate: &[u8]) -> Option<CertificateNames> {
    let certificate = contents_of(certificate, SEQUENCE)?;
    let mut tbs = contents_of(certificate, SEQUENCE)?;
    // Skip to `[3] extensions`: every field before it is one element.
    let extensions = loop {
        let (tag, contents, rest) = element(tbs)?;
        if tag == EXTENSIONS {
            break contents_of(contents, SEQUENCE)?;
        }
        tbs = rest;
    };
    let mut remaining = extensions;
    while !remaining.is_empty() {
        let (tag, extension, rest) = element(remaining)?;
        remaining = rest;
        if tag != SEQUENCE {
            return None;
        }
        let (tag, oid, mut fields) = element(extension)?;
        if tag != OID || oid != SUBJECT_ALT_NAME {
            continue;
        }
        if let Some((BOOLEAN, _, rest)) = element(fields) {
            fields = rest;
        }
        let value = contents_of(fields, OCTET_STRING)?;
        return Some(general_names(contents_of(value, SEQUENCE)?));
    }
    Some(CertificateNames::default())
}

fn general_names(mut input: &[u8]) -> CertificateNames {
    let mut names = CertificateNames::default();
    while let Some((tag, contents, rest)) = element(input) {
        input = rest;
        match tag {
            DNS_NAME | URI => {
                let Ok(text) = std::str::from_utf8(contents) else {
                    continue;
                };
                if tag == DNS_NAME {
                    names.dns.push(text.to_string());
                } else {
                    names.uris.push(text.to_string());
                }
            }
            IP_ADDRESS => {
                if let Ok(octets) = <[u8; 4]>::try_from(contents) {
                    names.ips.push(IpAddr::from(octets));
                } else if let Ok(octets) = <[u8; 16]>::try_from(contents) {
                    names.ips.push(IpAddr::from(octets));
                }
            }
            _ => {}
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_has_no_names() {
        assert_eq!(CertificateNames::of(&[]), CertificateNames::default());
        assert_eq!(
            CertificateNames::of(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]),
            CertificateNames::default()
        );
        assert_eq!(CertificateNames::default().primary(), None);
    }

    #[test]
    fn the_primary_name_prefers_a_uri() {
        let names = CertificateNames {
            uris: vec!["paros://u/machine/a".into()],
            dns: vec!["a.test".into()],
            ips: vec![IpAddr::from([10, 0, 0, 1])],
        };
        assert_eq!(names.primary().as_deref(), Some("paros://u/machine/a"));
        let names = CertificateNames {
            uris: vec![],
            ..names
        };
        assert_eq!(names.primary().as_deref(), Some("a.test"));
        let names = CertificateNames {
            dns: vec![],
            ..names
        };
        assert_eq!(names.primary().as_deref(), Some("10.0.0.1"));
    }
}
