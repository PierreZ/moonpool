//! The frame envelope: a hand-written, versioned, codec-independent layout.
//!
//! The transport routes, bounds and rejects messages using only these
//! fields; bodies are opaque bytes tagged with their [`CodecId`]. All
//! integers are little-endian and fixed-width.
//!
//! ```text
//! kind 0x01 HELLO    magic u32 | version u16 | incarnation u64
//! kind 0x02 REQUEST  call_id u64 | incarnation u64 | token.index u32 | token.generation u32
//!                    | method u64 | schema u64 | codec u16 | body (rest of the frame)
//! kind 0x03 REPLY    call_id u64 | status u8 | status 0:  codec u16 | body (rest)
//!                                            | status >0: detail u64 (nothing after)
//! ```
//!
//! Reply status codes: `0` ok, `1` endpoint not found, `2` stale incarnation,
//! `3` method mismatch (detail: registered method), `4` schema mismatch
//! (detail: registered schema), `5` codec mismatch (detail: registered
//! codec), `6` malformed request, `7` overloaded, `8` broken promise, `9`
//! reply too large. Detail is `0` when it carries nothing.
//!
//! Changing this layout, adding a kind or a status code requires a new
//! [`PROTOCOL_VERSION`]; the golden vectors in the tests pin version 1.

use thiserror::Error;

use super::cursor::{Reader, Writer};
use super::schema::{MethodId, SchemaId};
use crate::codec::CodecId;
use crate::endpoint::{EndpointToken, Incarnation};

/// Magic number opening every connection's first frame (`"MPRC"`).
pub const PROTOCOL_MAGIC: u32 = 0x4d50_5243;

/// The envelope layout version this build speaks.
///
/// A connection whose peer announces a different version is closed as a
/// protocol violation before any request is admitted. Accepting adjacent
/// versions during rolling upgrades is future work (#218).
pub const PROTOCOL_VERSION: u16 = 1;

const KIND_HELLO: u8 = 0x01;
const KIND_REQUEST: u8 = 0x02;
const KIND_REPLY: u8 = 0x03;

/// One frame payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage {
    /// First frame in each direction of every connection.
    Hello {
        /// Must equal [`PROTOCOL_MAGIC`].
        magic: u32,
        /// The sender's [`PROTOCOL_VERSION`].
        version: u16,
        /// The sender's transport incarnation.
        incarnation: Incarnation,
    },
    /// One request attempt for a dynamic endpoint.
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
        schema: SchemaId,
        /// The codec that produced `body`.
        codec: CodecId,
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
        registered: SchemaId,
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
    /// The handler's reply could not be encoded within the frame limit.
    ReplyTooLarge,
}

impl WireError {
    fn status_and_detail(self) -> (u8, u64) {
        match self {
            Self::EndpointNotFound => (1, 0),
            Self::StaleIncarnation => (2, 0),
            Self::MethodMismatch { registered } => (3, registered.get()),
            Self::SchemaMismatch { registered } => (4, registered.get()),
            Self::CodecMismatch { registered } => (5, u64::from(registered.get())),
            Self::MalformedRequest => (6, 0),
            Self::Overloaded => (7, 0),
            Self::BrokenPromise => (8, 0),
            Self::ReplyTooLarge => (9, 0),
        }
    }

    fn from_status(status: u8, detail: u64) -> Result<Self, EnvelopeError> {
        Ok(match status {
            1 => Self::EndpointNotFound,
            2 => Self::StaleIncarnation,
            3 => Self::MethodMismatch {
                registered: MethodId::new(detail),
            },
            4 => Self::SchemaMismatch {
                registered: SchemaId::new(detail),
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
            version,
            incarnation,
        } => {
            out.u8(KIND_HELLO)
                .u32(*magic)
                .u16(*version)
                .u64(incarnation.get());
        }
        WireMessage::Request {
            call_id,
            incarnation,
            token,
            method,
            schema,
            codec,
            body,
        } => {
            out.u8(KIND_REQUEST)
                .u64(*call_id)
                .u64(incarnation.get())
                .u32(token.index())
                .u32(token.generation())
                .u64(method.get())
                .u64(schema.get())
                .u16(codec.get())
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
    }
    out.0
}

/// Size of a request envelope carrying a `body_len`-byte body.
#[must_use]
pub const fn request_envelope_len(body_len: usize) -> usize {
    1 + 8 + 8 + 4 + 4 + 8 + 8 + 2 + body_len
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
                version: input.u16().ok_or_else(truncated)?,
                incarnation: Incarnation::from_raw(input.u64().ok_or_else(truncated)?),
            };
            if !input.is_empty() {
                return Err(EnvelopeError::TrailingBytes);
            }
            message
        }
        KIND_REQUEST => {
            let call_id = input.u64().ok_or_else(truncated)?;
            let incarnation = Incarnation::from_raw(input.u64().ok_or_else(truncated)?);
            let index = input.u32().ok_or_else(truncated)?;
            let generation = input.u32().ok_or_else(truncated)?;
            let method = MethodId::new(input.u64().ok_or_else(truncated)?);
            let schema = SchemaId::new(input.u64().ok_or_else(truncated)?);
            let codec = CodecId::new(input.u16().ok_or_else(truncated)?);
            WireMessage::Request {
                call_id,
                incarnation,
                token: EndpointToken::from_parts(index, generation),
                method,
                schema,
                codec,
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
        other => return Err(EnvelopeError::UnknownKind(other)),
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::{
        EnvelopeError, PROTOCOL_MAGIC, PROTOCOL_VERSION, WireError, WireMessage, WireOutcome,
        decode_message, encode_message, reply_envelope_len, request_envelope_len,
    };
    use crate::codec::CodecId;
    use crate::endpoint::{EndpointToken, Incarnation};
    use crate::protocol::{MethodId, SchemaId};

    fn request() -> WireMessage {
        WireMessage::Request {
            call_id: 5,
            incarnation: Incarnation::from_raw(0x0102),
            token: EndpointToken::from_parts(3, 1),
            method: MethodId::new(0x20),
            schema: SchemaId::new(0x10),
            codec: CodecId::PROST,
            body: vec![0xAA, 0xBB],
        }
    }

    /// Golden vectors: a change here is a wire-format change and needs a
    /// protocol version bump, not a fixture update.
    #[test]
    fn golden_encodings() {
        let hello = WireMessage::Hello {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            incarnation: Incarnation::from_raw(1),
        };
        assert_eq!(
            encode_message(&hello),
            [
                0x01, 0x43, 0x52, 0x50, 0x4d, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        assert_eq!(
            encode_message(&request()),
            [
                0x02, 5, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x01, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0,
                0x20, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0xAA, 0xBB
            ]
        );
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
    fn every_error_round_trips() {
        let errors = [
            WireError::EndpointNotFound,
            WireError::StaleIncarnation,
            WireError::MethodMismatch {
                registered: MethodId::new(u64::MAX),
            },
            WireError::SchemaMismatch {
                registered: SchemaId::new(7),
            },
            WireError::CodecMismatch {
                registered: CodecId::new(0x8001),
            },
            WireError::MalformedRequest,
            WireError::Overloaded,
            WireError::BrokenPromise,
            WireError::ReplyTooLarge,
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
        let mut trailing = encode_message(&WireMessage::Hello {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            incarnation: Incarnation::from_raw(1),
        });
        trailing.push(0);
        assert_eq!(decode_message(&trailing), Err(EnvelopeError::TrailingBytes));
    }
}
