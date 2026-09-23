//! The request credential section (protocol version 2).
//!
//! A request's metadata section (its `u16`-length-prefixed bytes, see
//! [`wire`](super::wire)) holds zero or more entries:
//!
//! ```text
//! entry := kind u8 | len u16 LE | value (len bytes)
//! kind 1: bearer credential (at most one; never empty)
//! ```
//!
//! Every other kind is reserved: a section carrying one is malformed and
//! the request is refused (fail closed), never partly understood. Under
//! protocol version 1 the section is carried and ignored, as version 1
//! specifies: credentials only count on a version 2 session.

use crate::security::CredentialError;

/// Entry kind of a bearer credential.
pub const ENTRY_BEARER: u8 = 1;

/// The bytes of an entry's fixed header.
pub const ENTRY_HEADER_LEN: usize = 3;

/// The section carrying `credential` as its only entry, or `None` when it
/// does not fit an entry (more than `u16::MAX` bytes) or is empty.
#[must_use]
pub fn encode_bearer(credential: &[u8]) -> Option<Vec<u8>> {
    let len = u16::try_from(credential.len())
        .ok()
        .filter(|len| *len > 0)?;
    // The whole section must also fit its own u16 length prefix.
    if credential.len() + ENTRY_HEADER_LEN > usize::from(u16::MAX) {
        return None;
    }
    let mut section = Vec::with_capacity(ENTRY_HEADER_LEN + credential.len());
    section.push(ENTRY_BEARER);
    section.extend_from_slice(&len.to_le_bytes());
    section.extend_from_slice(credential);
    Some(section)
}

/// The bearer credential in `section`, if any, refusing a section larger
/// than `max_bytes` before looking at it.
///
/// # Errors
///
/// [`CredentialError::TooLarge`] above the bound;
/// [`CredentialError::Malformed`] for a truncated entry, an unknown kind, a
/// repeated bearer entry or an empty credential.
pub fn bearer(section: &[u8], max_bytes: usize) -> Result<Option<&[u8]>, CredentialError> {
    if section.len() > max_bytes {
        return Err(CredentialError::TooLarge);
    }
    let mut rest = section;
    let mut found = None;
    while let Some((&kind, tail)) = rest.split_first() {
        let (len, tail) = tail
            .split_first_chunk::<2>()
            .ok_or(CredentialError::Malformed)?;
        let len = usize::from(u16::from_le_bytes(*len));
        if tail.len() < len {
            return Err(CredentialError::Malformed);
        }
        let (value, tail) = tail.split_at(len);
        match kind {
            ENTRY_BEARER if found.is_none() && !value.is_empty() => found = Some(value),
            _ => return Err(CredentialError::Malformed),
        }
        rest = tail;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::{ENTRY_BEARER, bearer, encode_bearer};
    use crate::security::CredentialError;

    #[test]
    fn a_bearer_entry_round_trips_and_malformed_sections_are_refused() {
        let section = encode_bearer(b"token").expect("fits");
        assert_eq!(section, [ENTRY_BEARER, 5, 0, b't', b'o', b'k', b'e', b'n']);
        assert_eq!(bearer(&section, 64), Ok(Some(&b"token"[..])));
        assert_eq!(bearer(&[], 64), Ok(None));
        assert_eq!(bearer(&section, 7), Err(CredentialError::TooLarge));
        let mut twice = section.clone();
        twice.extend_from_slice(&section);
        assert_eq!(bearer(&twice, 64), Err(CredentialError::Malformed));
        assert_eq!(bearer(&section[..6], 64), Err(CredentialError::Malformed));
        assert_eq!(
            bearer(&[ENTRY_BEARER, 0], 64),
            Err(CredentialError::Malformed)
        );
        assert_eq!(
            bearer(&[ENTRY_BEARER, 0, 0], 64),
            Err(CredentialError::Malformed)
        );
        assert_eq!(bearer(&[9, 1, 0, 1], 64), Err(CredentialError::Malformed));
        assert_eq!(encode_bearer(b""), None);
        assert_eq!(encode_bearer(&vec![0; usize::from(u16::MAX)]), None);
        assert!(encode_bearer(&vec![0; usize::from(u16::MAX) - 3]).is_some());
    }
}
