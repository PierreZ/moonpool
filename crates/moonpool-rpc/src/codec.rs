//! Payload codecs: how request and reply bodies become bytes.
//!
//! The transport never looks inside a body. It frames, bounds, routes and
//! rejects requests using only the hand-written envelope
//! ([`protocol`](crate::protocol)); a body is opaque bytes plus the
//! [`CodecId`] that produced them. [`Wire`] is the one seam between a Rust
//! message type and those bytes.
//!
//! # Codec selection (closed by #213)
//!
//! | Criterion | prost (chosen) | postcard | bincode 2 | rmp-serde |
//! |---|---|---|---|---|
//! | Rust-defined messages | `#[derive(prost::Message)]` on Rust structs, no `.proto`/codegen step | serde derive | serde / own derive | serde derive |
//! | Bounded decode | decodes from a length-checked slice | same | configurable limit | same |
//! | wasm / no native deps | yes | yes | yes | yes |
//! | Deterministic encoding | fields in tag order (use `btree_map` for maps) | yes (no `HashMap`) | yes | yes |
//! | Schema evolution | field tags: add optional fields, never reuse/renumber a tag | none: any shape change is a new schema | none | self-describing |
//! | Maintenance / license | tokio-rs, Apache-2.0 | active, MIT/Apache-2.0 | upstream archived 2025, MIT | MIT |
//!
//! **prost is the codec**: it is the only candidate whose wire format is
//! designed for independent evolution of the two ends, which a rolling
//! upgrade needs, while messages stay ordinary Rust structs. It is behind
//! the default-on `prost` feature so the transport itself builds without
//! it. The [`Wire`] trait keeps the door open for another codec later
//! (under its own [`CodecId`]); no other codec ships today.
//!
//! # Evolution rules
//!
//! - Never reuse or renumber a prost field tag; new fields must be optional
//!   (or have a harmless default); remove a field by reserving its tag.
//!   Unknown fields are skipped, so old and new ends interoperate.
//! - A change the tag rules cannot express is a new
//!   [`SchemaId`](crate::SchemaId).
//! - Method and schema identifiers are application constants, never derived
//!   from Rust type names, declaration order or layout
//!   ([`RpcMethod`](crate::RpcMethod)).
//! - The codec is part of an endpoint's interface: a request whose
//!   [`CodecId`] differs from the one the endpoint registered is rejected
//!   with [`RpcError::CodecMismatch`](crate::RpcError::CodecMismatch) before
//!   any decode, never parsed as garbage.

use thiserror::Error;

/// Identifies the encoding of a message body.
///
/// Carried in every request and reply envelope and in every published
/// [`ServiceRef`](crate::ServiceRef). Values `1..=0x7FFF` are reserved for
/// codecs defined by this crate; applications use `0x8000..=0xFFFF`. Zero is
/// never a valid codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CodecId(u16);

impl CodecId {
    /// Protocol Buffers through prost.
    pub const PROST: Self = Self(1);
    /// This crate's own fixed layout (routing types such as
    /// [`ServiceRef`](crate::ServiceRef)).
    pub const RPC: Self = Self(2);

    /// Wrap a raw codec identifier.
    #[must_use]
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    /// The raw identifier.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for CodecId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::PROST => f.write_str("prost"),
            Self::RPC => f.write_str("rpc"),
            Self(other) => write!(f, "codec:{other:#06x}"),
        }
    }
}

/// A message body could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("encode failed: {0}")]
pub struct EncodeError(pub String);

/// A message body could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("decode failed: {0}")]
pub struct DecodeError(pub String);

/// A message type the transport can carry as a request or reply body.
///
/// Implementations must be deterministic (the same value always encodes to
/// the same bytes) and total over their input when decoding: any byte
/// string yields a value or a [`DecodeError`], never a panic. The input to
/// [`decode`](Self::decode) is already bounded by the frame limit.
///
/// With the default `prost` feature every `prost::Message + Default` type
/// implements `Wire`:
///
/// ```
/// # #[cfg(feature = "prost")] {
/// use moonpool_rpc::{CodecId, Wire};
///
/// #[derive(Clone, PartialEq, prost::Message)]
/// struct Greeting {
///     #[prost(string, tag = "1")]
///     text: String,
/// }
///
/// assert_eq!(<Greeting as Wire>::CODEC, CodecId::PROST);
/// # }
/// ```
pub trait Wire: Sized + Send + 'static {
    /// The codec that produced the bytes; part of the endpoint's interface.
    const CODEC: CodecId;

    /// Append this value's encoding to `buf`.
    ///
    /// # Errors
    ///
    /// When the value cannot be represented by the codec.
    fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError>;

    /// Decode one value from exactly `bytes`.
    ///
    /// # Errors
    ///
    /// When `bytes` is not a valid encoding.
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
}

#[cfg(feature = "prost")]
impl<T: prost::Message + Default + 'static> Wire for T {
    const CODEC: CodecId = CodecId::PROST;

    fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
        prost::Message::encode(self, buf).map_err(|error| EncodeError(error.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        <T as prost::Message>::decode(bytes).map_err(|error| DecodeError(error.to_string()))
    }
}

/// Encode a value into a fresh buffer.
pub(crate) fn encode_to_vec<T: Wire>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::new();
    value.encode(&mut buf)?;
    Ok(buf)
}

#[cfg(all(test, feature = "prost"))]
mod prost_tests {
    use super::{CodecId, Wire, encode_to_vec};

    #[derive(Clone, PartialEq, prost::Message)]
    struct V1 {
        #[prost(uint64, tag = "1")]
        id: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct V2 {
        #[prost(uint64, tag = "1")]
        id: u64,
        #[prost(string, optional, tag = "2")]
        note: Option<String>,
    }

    #[test]
    fn prost_messages_are_wire_with_the_prost_codec() {
        assert_eq!(<V1 as Wire>::CODEC, CodecId::PROST);
        let bytes = encode_to_vec(&V1 { id: 300 }).expect("encodes");
        // Golden: tag 1 varint, 300 = 0xAC 0x02.
        assert_eq!(bytes, [0x08, 0xAC, 0x02]);
        assert_eq!(V1::decode(&bytes).expect("decodes"), V1 { id: 300 });
        assert!(V1::decode(&[0x08]).is_err(), "truncated varint");
        assert_eq!(String::CODEC, CodecId::PROST, "prost wrapper types work");
    }

    /// Schema-evolution fixture: adding an optional field under a new tag
    /// keeps both directions readable.
    #[test]
    fn adding_an_optional_tag_is_compatible_both_ways() {
        let old = encode_to_vec(&V1 { id: 9 }).expect("encodes");
        assert_eq!(V2::decode(&old).expect("new reads old").note, None);
        let new = encode_to_vec(&V2 {
            id: 9,
            note: Some("x".into()),
        })
        .expect("encodes");
        assert_eq!(
            V1::decode(&new).expect("old skips unknown tag"),
            V1 { id: 9 }
        );
    }
}
