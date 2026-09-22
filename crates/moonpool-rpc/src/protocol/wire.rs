//! The frame envelope: a hand-written, versioned, codec-independent layout.
//!
//! The transport routes, bounds and rejects messages using only these
//! fields; bodies are opaque bytes tagged with their [`CodecId`]. All
//! integers are little-endian and fixed-width.
//!
//! ```text
//! kind 0x01 HELLO    magic u32 | min_version u16 | max_version u16
//!                    | incarnation u128 | features u64 | max_frame_bytes u32
//!                    | listen: family u8 (0 none, 4, 6) | ip (0, 4 or 16 bytes) | port u16 (if family > 0)
//! kind 0x02 REQUEST  call_id u64 | incarnation u128 | token.index u64 | token.generation u32
//!                    | method u32 | schema u16 | codec u16 | flags u8
//!                    | metadata_len u16 | metadata | body (rest of the frame)
//! kind 0x03 REPLY    call_id u64 | status u8 | status 0:  codec u16 | body (rest)
//!                                            | status >0: detail u64 (nothing after)
//! kind 0x04 PING     nonce u64
//! kind 0x05 PONG     nonce u64
//! ```
//!
//! - HELLO: each side announces the range of envelope versions it speaks;
//!   the session runs at the highest common version ([`negotiate`]) and a
//!   peer with no common version is refused. `features` is reserved for
//!   capabilities negotiated by later packages (TLS, authentication,
//!   streams); this version sends zero and ignores unknown bits.
//!   `max_frame_bytes` is the largest frame payload the sender accepts:
//!   each side sends the peer nothing larger, so an oversized request or
//!   reply fails its own call instead of tearing the session down.
//!   `listen` is the sender's canonical listening address (none for a
//!   client-only runtime): the receiver of an inbound session uses it to
//!   share that session as its own connection to the sender and to settle
//!   simultaneous connects (the larger canonical address keeps the
//!   connection it dialed, as in `FoundationDB`).
//! - REQUEST: `metadata` is a reserved, length-prefixed section for request
//!   credentials (verified by the security package, #218). This version
//!   sends it empty and ignores what it receives; it is never passed to a
//!   handler. `flags` bit 0 marks a one-way request: the receiver never
//!   sends a reply or a rejection for it. Every other bit is reserved and
//!   must be zero.
//! - PING / PONG: connection liveness. A PING is answered with a PONG
//!   carrying the same nonce; any received byte counts as liveness.
//!
//! Reply status codes: `0` ok, `1` endpoint not found, `2` stale incarnation,
//! `3` method mismatch (detail: registered method), `4` schema mismatch
//! (detail: registered schema), `5` codec mismatch (detail: registered
//! codec), `6` malformed request, `7` overloaded, `8` broken promise, `9`
//! reply too large, `10` reply encoding failed. Detail is `0` when it
//! carries nothing.
//!
//! Changing this layout, adding a kind or a status code requires a new
//! [`PROTOCOL_VERSION`]; the golden vectors in the tests pin version 1.

use thiserror::Error;

use super::cursor::{Reader, Writer};
use super::schema::{MethodId, SchemaVersion};
use crate::codec::CodecId;
use crate::endpoint::{EndpointToken, Incarnation};

/// Magic number opening every connection's first frame (`"MPRC"`).
pub const PROTOCOL_MAGIC: u32 = 0x4d50_5243;

/// The newest envelope layout version this build speaks.
///
/// Version 1 was the unreleased first-package layout (no listen address,
/// request flags or liveness frames); nothing speaks it any more.
pub const PROTOCOL_VERSION: u16 = 2;

/// The oldest envelope layout version this build speaks.
///
/// A peer whose announced range does not overlap
/// `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION` is refused before any request
/// is admitted. Supporting a wider window for rolling upgrades is #218.
pub const MIN_PROTOCOL_VERSION: u16 = 2;

/// The highest version both ranges contain, if any.
#[must_use]
pub fn negotiate(local: (u16, u16), peer: (u16, u16)) -> Option<u16> {
    let low = local.0.max(peer.0);
    let high = local.1.min(peer.1);
    (low <= high).then_some(high)
}

const KIND_HELLO: u8 = 0x01;
const KIND_REQUEST: u8 = 0x02;
const KIND_REPLY: u8 = 0x03;
const KIND_PING: u8 = 0x04;
const KIND_PONG: u8 = 0x05;

/// `flags` bit of a one-way request: no reply or rejection is ever sent.
pub const REQUEST_FLAG_ONE_WAY: u8 = 0x01;
const REQUEST_FLAGS_KNOWN: u8 = REQUEST_FLAG_ONE_WAY;

/// One frame payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage {
    /// First frame in each direction of every connection.
    Hello {
        /// Must equal [`PROTOCOL_MAGIC`].
        magic: u32,
        /// The oldest envelope version the sender speaks.
        min_version: u16,
        /// The newest envelope version the sender speaks.
        max_version: u16,
        /// The sender's transport incarnation.
        incarnation: Incarnation,
        /// Reserved capability bits; zero in this version.
        features: u64,
        /// The largest frame payload the sender accepts.
        max_frame_bytes: u32,
        /// The sender's canonical listening address; `None` for a runtime
        /// that does not listen.
        listen: Option<std::net::SocketAddr>,
    },
    /// One request attempt for an endpoint.
    Request {
        /// Caller-allocated reply route, meaningful only on this connection.
        call_id: u64,
        /// The transport incarnation the caller's reference names.
        incarnation: Incarnation,
        /// The endpoint the caller's reference names.
        token: EndpointToken,
        /// The method the caller invokes.
        method: MethodId,
        /// The contract version the caller encoded with.
        schema: SchemaVersion,
        /// The codec that produced `body`.
        codec: CodecId,
        /// Request flags ([`REQUEST_FLAG_ONE_WAY`]); unknown bits are a
        /// protocol violation.
        flags: u8,
        /// Reserved credential/metadata section; empty in this version.
        metadata: Vec<u8>,
        /// The encoded request.
        body: Vec<u8>,
    },
    /// The single completion of one request attempt.
    Reply {
        /// The `call_id` of the request this answers, on this connection.
        call_id: u64,
        /// The handler's reply or the reason the request was not served.
        outcome: WireOutcome,
    },
    /// Liveness probe; answered with a [`WireMessage::Pong`].
    Ping {
        /// Echoed back in the pong.
        nonce: u64,
    },
    /// Answer to a [`WireMessage::Ping`].
    Pong {
        /// The ping's nonce.
        nonce: u64,
    },
}

/// A reply's payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireOutcome {
    /// The encoded reply body.
    Ok {
        /// The codec that produced `body`.
        codec: CodecId,
        /// The encoded reply.
        body: Vec<u8>,
    },
    /// The request was not served (or its responder was dropped).
    Err(WireError),
}

/// Server-side rejection reasons carried back to a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// No live endpoint holds the token.
    EndpointNotFound,
    /// The reference names another incarnation of this transport.
    StaleIncarnation,
    /// The endpoint serves a different method.
    MethodMismatch {
        /// The method the endpoint was registered for.
        registered: MethodId,
    },
    /// The endpoint serves a different contract version.
    SchemaMismatch {
        /// The contract the endpoint was registered with.
        registered: SchemaVersion,
    },
    /// The request body uses a codec the endpoint does not accept.
    CodecMismatch {
        /// The codec the endpoint was registered with.
        registered: CodecId,
    },
    /// The request body did not decode as the registered request type.
    MalformedRequest,
    /// The endpoint's queue was full.
    Overloaded,
    /// The handler dropped its reply handle without replying.
    BrokenPromise,
    /// The handler's reply exceeded the frame limit of the session.
    ReplyTooLarge,
    /// The handler's reply could not be encoded by its codec.
    ReplyEncodeFailed,
}

impl WireError {
    fn status_and_detail(self) -> (u8, u64) {
        match self {
            Self::EndpointNotFound => (1, 0),
            Self::StaleIncarnation => (2, 0),
            Self::MethodMismatch { registered } => (3, u64::from(registered.get())),
            Self::SchemaMismatch { registered } => (4, u64::from(registered.get())),
            Self::CodecMismatch { registered } => (5, u64::from(registered.get())),
            Self::MalformedRequest => (6, 0),
            Self::Overloaded => (7, 0),
            Self::BrokenPromise => (8, 0),
            Self::ReplyTooLarge => (9, 0),
            Self::ReplyEncodeFailed => (10, 0),
        }
    }

    fn from_status(status: u8, detail: u64) -> Result<Self, EnvelopeError> {
        Ok(match status {
            1 => Self::EndpointNotFound,
            2 => Self::StaleIncarnation,
            3 => Self::MethodMismatch {
                registered: MethodId::new(
                    u32::try_from(detail).map_err(|_| EnvelopeError::InvalidField("method"))?,
                ),
            },
            4 => Self::SchemaMismatch {
                registered: SchemaVersion::new(
                    u16::try_from(detail).map_err(|_| EnvelopeError::InvalidField("schema"))?,
                ),
            },
            5 => Self::CodecMismatch {
                registered: CodecId::new(
                    u16::try_from(detail).map_err(|_| EnvelopeError::InvalidField("codec"))?,
                ),
            },
            6 => Self::MalformedRequest,
            7 => Self::Overloaded,
            8 => Self::BrokenPromise,
            9 => Self::ReplyTooLarge,
            10 => Self::ReplyEncodeFailed,
            other => return Err(EnvelopeError::UnknownStatus(other)),
        })
    }
}

/// An envelope that does not parse. Always a protocol violation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvelopeError {
    /// The payload was empty.
    #[error("empty envelope")]
    Empty,
    /// The kind byte is not one this version defines.
    #[error("unknown envelope kind {0:#04x}")]
    UnknownKind(u8),
    /// The payload ended inside a fixed field.
    #[error("truncated envelope")]
    Truncated,
    /// A reply status code this version does not define.
    #[error("unknown reply status {0}")]
    UnknownStatus(u8),
    /// Bytes followed a message that must end there.
    #[error("trailing bytes after envelope")]
    TrailingBytes,
    /// A field holds a value that is never valid.
    #[error("invalid {0} field")]
    InvalidField(&'static str),
}

/// Encode one message into its envelope bytes.
#[must_use]
pub fn encode_message(message: &WireMessage) -> Vec<u8> {
    let mut out = Writer::new();
    match message {
        WireMessage::Hello {
            magic,
            min_version,
            max_version,
            incarnation,
            features,
            max_frame_bytes,
            listen,
        } => {
            out.u8(KIND_HELLO)
                .u32(*magic)
                .u16(*min_version)
                .u16(*max_version)
                .u128(incarnation.get())
                .u64(*features)
                .u32(*max_frame_bytes);
            match listen {
                Some(address) => out.socket_addr(*address),
                None => out.u8(0),
            };
        }
        WireMessage::Request {
            call_id,
            incarnation,
            token,
            method,
            schema,
            codec,
            flags,
            metadata,
            body,
        } => {
            // The metadata section is bounded by its u16 length prefix; the
            // encoder never produces more (callers keep it empty for now).
            let metadata = &metadata[..metadata.len().min(usize::from(u16::MAX))];
            out.u8(KIND_REQUEST)
                .u64(*call_id)
                .u128(incarnation.get())
                .u64(token.index())
                .u32(token.generation())
                .u32(method.get())
                .u16(schema.get())
                .u16(codec.get())
                .u8(*flags)
                .u16(u16::try_from(metadata.len()).unwrap_or(u16::MAX))
                .bytes(metadata)
                .bytes(body);
        }
        WireMessage::Reply { call_id, outcome } => {
            out.u8(KIND_REPLY).u64(*call_id);
            match outcome {
                WireOutcome::Ok { codec, body } => {
                    out.u8(0).u16(codec.get()).bytes(body);
                }
                WireOutcome::Err(error) => {
                    let (status, detail) = error.status_and_detail();
                    out.u8(status).u64(detail);
                }
            }
        }
        WireMessage::Ping { nonce } => {
            out.u8(KIND_PING).u64(*nonce);
        }
        WireMessage::Pong { nonce } => {
            out.u8(KIND_PONG).u64(*nonce);
        }
    }
    out.0
}

/// Size of the largest `Hello` envelope (one announcing an IPv6 listen
/// address).
pub const HELLO_ENVELOPE_LEN: usize = 1 + 4 + 2 + 2 + 16 + 8 + 4 + 1 + 16 + 2;

/// Size of a `Ping` or `Pong` envelope.
pub const LIVENESS_ENVELOPE_LEN: usize = 1 + 8;

/// Size of a rejection reply envelope (the largest fixed reply layout).
pub const REJECTION_ENVELOPE_LEN: usize = 1 + 8 + 1 + 8;

/// Size of a request envelope carrying a `body_len`-byte body.
#[must_use]
pub const fn request_envelope_len(body_len: usize) -> usize {
    1 + 8 + 16 + 8 + 4 + 4 + 2 + 2 + 1 + 2 + body_len
}

/// Size of an ok-reply envelope carrying a `body_len`-byte body.
#[must_use]
pub const fn reply_envelope_len(body_len: usize) -> usize {
    1 + 8 + 1 + 2 + body_len
}

/// Decode one frame payload.
///
/// # Errors
///
/// An [`EnvelopeError`] for anything that is not exactly one well-formed
/// message of this version.
pub fn decode_message(payload: &[u8]) -> Result<WireMessage, EnvelopeError> {
    let mut input = Reader::new(payload);
    let kind = input.u8().ok_or(EnvelopeError::Empty)?;
    let truncated = || EnvelopeError::Truncated;
    let message = match kind {
        KIND_HELLO => {
            let message = WireMessage::Hello {
                magic: input.u32().ok_or_else(truncated)?,
                min_version: input.u16().ok_or_else(truncated)?,
                max_version: input.u16().ok_or_else(truncated)?,
                incarnation: Incarnation::from_raw(input.u128().ok_or_else(truncated)?),
                features: input.u64().ok_or_else(truncated)?,
                max_frame_bytes: input.u32().ok_or_else(truncated)?,
                listen: match input.u8().ok_or_else(truncated)? {
                    0 => None,
                    family => Some(decode_listen(family, &mut input)?),
                },
            };
            if !input.is_empty() {
                return Err(EnvelopeError::TrailingBytes);
            }
            message
        }
        KIND_REQUEST => {
            let call_id = input.u64().ok_or_else(truncated)?;
            let incarnation = Incarnation::from_raw(input.u128().ok_or_else(truncated)?);
            let index = input.u64().ok_or_else(truncated)?;
            let generation = input.u32().ok_or_else(truncated)?;
            let method = MethodId::new(input.u32().ok_or_else(truncated)?);
            let schema = SchemaVersion::new(input.u16().ok_or_else(truncated)?);
            let codec = CodecId::new(input.u16().ok_or_else(truncated)?);
            let flags = input.u8().ok_or_else(truncated)?;
            if flags & !REQUEST_FLAGS_KNOWN != 0 {
                return Err(EnvelopeError::InvalidField("flags"));
            }
            let metadata_len = input.u16().ok_or_else(truncated)?;
            let metadata = input
                .slice(usize::from(metadata_len))
                .ok_or_else(truncated)?
                .to_vec();
            WireMessage::Request {
                call_id,
                incarnation,
                token: EndpointToken::from_parts(index, generation),
                method,
                schema,
                codec,
                flags,
                metadata,
                body: input.rest().to_vec(),
            }
        }
        KIND_REPLY => {
            let call_id = input.u64().ok_or_else(truncated)?;
            let status = input.u8().ok_or_else(truncated)?;
            let outcome = if status == 0 {
                WireOutcome::Ok {
                    codec: CodecId::new(input.u16().ok_or_else(truncated)?),
                    body: input.rest().to_vec(),
                }
            } else {
                let detail = input.u64().ok_or_else(truncated)?;
                if !input.is_empty() {
                    return Err(EnvelopeError::TrailingBytes);
                }
                WireOutcome::Err(WireError::from_status(status, detail)?)
            };
            WireMessage::Reply { call_id, outcome }
        }
        KIND_PING | KIND_PONG => {
            let nonce = input.u64().ok_or_else(truncated)?;
            if !input.is_empty() {
                return Err(EnvelopeError::TrailingBytes);
            }
            if kind == KIND_PING {
                WireMessage::Ping { nonce }
            } else {
                WireMessage::Pong { nonce }
            }
        }
        other => return Err(EnvelopeError::UnknownKind(other)),
    };
    Ok(message)
}

/// The listen address after its family byte (4 or 6).
fn decode_listen(
    family: u8,
    input: &mut Reader<'_>,
) -> Result<std::net::SocketAddr, EnvelopeError> {
    let ip = match family {
        4 => {
            let octets: [u8; 4] = input
                .slice(4)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(EnvelopeError::Truncated)?;
            std::net::IpAddr::from(octets)
        }
        6 => {
            let octets: [u8; 16] = input
                .slice(16)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(EnvelopeError::Truncated)?;
            std::net::IpAddr::from(octets)
        }
        _ => return Err(EnvelopeError::InvalidField("listen address family")),
    };
    let port = input.u16().ok_or(EnvelopeError::Truncated)?;
    Ok(std::net::SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::{
        EnvelopeError, HELLO_ENVELOPE_LEN, MIN_PROTOCOL_VERSION, PROTOCOL_MAGIC, PROTOCOL_VERSION,
        WireError, WireMessage, WireOutcome, decode_message, encode_message, negotiate,
        reply_envelope_len, request_envelope_len,
    };
    use crate::codec::CodecId;
    use crate::endpoint::{EndpointToken, Incarnation};
    use crate::protocol::{MethodId, SchemaVersion};

    fn request() -> WireMessage {
        WireMessage::Request {
            call_id: 5,
            incarnation: Incarnation::from_raw(0x0102),
            token: EndpointToken::from_parts(3, 1),
            method: MethodId::new(0x20),
            schema: SchemaVersion::new(0x10),
            codec: CodecId::PROST,
            flags: 0,
            metadata: Vec::new(),
            body: vec![0xAA, 0xBB],
        }
    }

    fn hello() -> WireMessage {
        WireMessage::Hello {
            magic: PROTOCOL_MAGIC,
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            incarnation: Incarnation::from_raw(1),
            features: 0,
            max_frame_bytes: 0x0001_0000,
            listen: None,
        }
    }

    fn hello_listening(listen: &str) -> WireMessage {
        let WireMessage::Hello {
            magic,
            min_version,
            max_version,
            incarnation,
            features,
            max_frame_bytes,
            ..
        } = hello()
        else {
            unreachable!("hello() builds a Hello")
        };
        WireMessage::Hello {
            magic,
            min_version,
            max_version,
            incarnation,
            features,
            max_frame_bytes,
            listen: listen.parse().ok(),
        }
    }

    /// Golden vectors: a change here is a wire-format change and needs a
    /// protocol version bump, not a fixture update.
    #[test]
    fn golden_encodings() {
        let mut expected = vec![0x01, 0x43, 0x52, 0x50, 0x4d, 2, 0, 2, 0, 1];
        expected.extend([0; 15]);
        expected.extend([0; 8]);
        expected.extend([0, 0, 1, 0]);
        expected.push(0);
        assert_eq!(encode_message(&hello()), expected);
        expected.pop();
        expected.extend([4, 10, 0, 1, 1, 0x94, 0x11]);
        assert_eq!(encode_message(&hello_listening("10.0.1.1:4500")), expected);
        assert_eq!(
            encode_message(&hello_listening("[::1]:4500")).len(),
            HELLO_ENVELOPE_LEN
        );
        assert_eq!(
            encode_message(&WireMessage::Ping { nonce: 7 }),
            [0x04, 7, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            encode_message(&WireMessage::Pong { nonce: 7 }),
            [0x05, 7, 0, 0, 0, 0, 0, 0, 0]
        );

        let mut expected = vec![0x02, 5, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x01];
        expected.extend([0; 14]);
        expected.extend([3, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend([1, 0, 0, 0]);
        expected.extend([0x20, 0, 0, 0]);
        expected.extend([0x10, 0]);
        expected.extend([1, 0]);
        expected.push(0);
        expected.extend([0, 0]);
        expected.extend([0xAA, 0xBB]);
        assert_eq!(encode_message(&request()), expected);

        let reply = WireMessage::Reply {
            call_id: 5,
            outcome: WireOutcome::Err(WireError::CodecMismatch {
                registered: CodecId::RPC,
            }),
        };
        assert_eq!(
            encode_message(&reply),
            [0x03, 5, 0, 0, 0, 0, 0, 0, 0, 5, 2, 0, 0, 0, 0, 0, 0, 0]
        );
        let ok = WireMessage::Reply {
            call_id: 1,
            outcome: WireOutcome::Ok {
                codec: CodecId::PROST,
                body: vec![9],
            },
        };
        assert_eq!(
            encode_message(&ok),
            [0x03, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 9]
        );
        assert_eq!(encode_message(&request()).len(), request_envelope_len(2));
        assert_eq!(encode_message(&ok).len(), reply_envelope_len(1));
    }

    #[test]
    fn metadata_section_round_trips_and_is_bounded() {
        let mut with_metadata = request();
        if let WireMessage::Request { metadata, .. } = &mut with_metadata {
            *metadata = vec![1, 2, 3];
        }
        let bytes = encode_message(&with_metadata);
        assert_eq!(decode_message(&bytes), Ok(with_metadata));
        // A metadata length running past the frame is truncation.
        let mut lying = encode_message(&request());
        let at = request_envelope_len(0) - 2;
        lying[at] = 0xFF;
        assert_eq!(decode_message(&lying), Err(EnvelopeError::Truncated));
    }

    #[test]
    fn liveness_listen_and_flags_round_trip_and_reject_garbage() {
        for message in [
            WireMessage::Ping { nonce: u64::MAX },
            WireMessage::Pong { nonce: 3 },
            hello_listening("10.0.1.1:4500"),
            hello_listening("[2001:db8::1]:9"),
        ] {
            assert_eq!(decode_message(&encode_message(&message)), Ok(message));
        }
        let mut one_way = request();
        if let WireMessage::Request { flags, .. } = &mut one_way {
            *flags = super::REQUEST_FLAG_ONE_WAY;
        }
        assert_eq!(
            decode_message(&encode_message(&one_way)),
            Ok(one_way.clone())
        );
        let mut unknown_flag = encode_message(&one_way);
        unknown_flag[request_envelope_len(0) - 3] = 0x80;
        assert_eq!(
            decode_message(&unknown_flag),
            Err(EnvelopeError::InvalidField("flags"))
        );
        let mut bad_family = encode_message(&hello());
        if let Some(last) = bad_family.last_mut() {
            *last = 5;
        }
        assert_eq!(
            decode_message(&bad_family),
            Err(EnvelopeError::InvalidField("listen address family"))
        );
        assert_eq!(decode_message(&[0x04, 1]), Err(EnvelopeError::Truncated));
        assert_eq!(
            decode_message(&[0x05, 1, 0, 0, 0, 0, 0, 0, 0, 9]),
            Err(EnvelopeError::TrailingBytes)
        );
    }

    #[test]
    fn versions_negotiate_to_the_highest_common_one() {
        assert_eq!(negotiate((1, 1), (1, 1)), Some(1));
        assert_eq!(negotiate((1, 3), (2, 5)), Some(3));
        assert_eq!(negotiate((1, 1), (2, 2)), None);
        assert_eq!(negotiate((4, 6), (1, 3)), None);
    }

    #[test]
    fn every_error_round_trips() {
        let errors = [
            WireError::EndpointNotFound,
            WireError::StaleIncarnation,
            WireError::MethodMismatch {
                registered: MethodId::new(u32::MAX),
            },
            WireError::SchemaMismatch {
                registered: SchemaVersion::new(7),
            },
            WireError::CodecMismatch {
                registered: CodecId::new(0x8001),
            },
            WireError::MalformedRequest,
            WireError::Overloaded,
            WireError::BrokenPromise,
            WireError::ReplyTooLarge,
            WireError::ReplyEncodeFailed,
        ];
        for error in errors {
            let message = WireMessage::Reply {
                call_id: 1,
                outcome: WireOutcome::Err(error),
            };
            assert_eq!(decode_message(&encode_message(&message)), Ok(message));
        }
    }

    #[test]
    fn round_trips_and_rejects_malformed_envelopes() {
        let bytes = encode_message(&request());
        assert_eq!(decode_message(&bytes), Ok(request()));
        assert_eq!(decode_message(&encode_message(&hello())), Ok(hello()));
        // The body is the rest of the frame, so only the fixed prefix can be
        // truncated.
        for cut in 0..request_envelope_len(0) {
            assert!(decode_message(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        assert_eq!(decode_message(&[]), Err(EnvelopeError::Empty));
        assert_eq!(
            decode_message(&[0x7F]),
            Err(EnvelopeError::UnknownKind(0x7F))
        );
        let mut unknown_status = encode_message(&WireMessage::Reply {
            call_id: 1,
            outcome: WireOutcome::Err(WireError::Overloaded),
        });
        unknown_status[9] = 200;
        assert_eq!(
            decode_message(&unknown_status),
            Err(EnvelopeError::UnknownStatus(200))
        );
        let mut oversized_detail = encode_message(&WireMessage::Reply {
            call_id: 1,
            outcome: WireOutcome::Err(WireError::SchemaMismatch {
                registered: SchemaVersion::new(1),
            }),
        });
        oversized_detail[12] = 1;
        assert_eq!(
            decode_message(&oversized_detail),
            Err(EnvelopeError::InvalidField("schema"))
        );
        let mut trailing = encode_message(&hello());
        trailing.push(0);
        assert_eq!(decode_message(&trailing), Err(EnvelopeError::TrailingBytes));
    }
}
