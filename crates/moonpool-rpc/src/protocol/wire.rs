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
//!                    | interface u32 | interface_version u16
//!                    | method u32 | schema u16 | codec u16 | flags u8
//!                    | [window u64, only with the stream flag]
//!                    | metadata_len u16 | metadata | body (rest of the frame)
//! kind 0x03 REPLY    call_id u64 | status u8 | status 0:  codec u16 | body (rest)
//!                                            | status >0: detail u64 (nothing after)
//! kind 0x04 PING     nonce u64
//! kind 0x05 PONG     nonce u64
//! kind 0x06 STREAM_ITEM   call_id u64 | sequence u64 | codec u16 | body (rest)
//! kind 0x07 STREAM_END    call_id u64 | items u64 | status u8 | detail u64
//! kind 0x08 STREAM_ACK    call_id u64 | consumed u64
//! kind 0x09 STREAM_CANCEL call_id u64
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
//! - REQUEST: `interface` / `interface_version` name the endpoint group's
//!   interface the reference was adjusted from; zero for a single-method
//!   endpoint. The server compares them with the registration before the
//!   method. `metadata` is a length-prefixed section: version 1 carries it
//!   and ignores it; version 2 reads request credentials from it
//!   ([`metadata`](super::metadata)), which the server's security check
//!   verifies. It is never passed to a handler. `flags` bit 0 marks a one-way request: the receiver never
//!   sends a reply or a rejection for it. Bit 1 opens a **reply stream**:
//!   the request then carries the caller's credit `window` (accounted bytes
//!   it buffers unconsumed) right after the flags, and its `call_id` names
//!   the stream on this connection. Both bits together are invalid; every
//!   other bit is reserved and must be zero.
//! - `STREAM_ITEM` (server to caller): item `sequence` (0, 1, 2, ... per
//!   stream) of the stream `call_id`. Its accounted size, the unit of
//!   stream credit, is its whole frame: [`stream_item_frame_len`] of the
//!   body length.
//! - `STREAM_END` (server to caller): the stream's single terminal frame,
//!   after its last item. `items` is how many items were sent; `status` `0`
//!   (with detail `0`) is a normal end, any other value a reply status code
//!   (below) with its detail.
//! - `STREAM_ACK` (caller to server): `consumed` is the cumulative accounted
//!   size of the items the caller's application has taken, always on an
//!   item boundary. Cumulative, so only the latest one matters.
//! - `STREAM_CANCEL` (caller to server): the caller abandoned the stream.
//! - PING / PONG: connection liveness. A PING is answered with a PONG
//!   carrying the same nonce; any received byte counts as liveness.
//!
//! Reply status codes: `0` ok, `1` endpoint not found, `2` stale incarnation,
//! `3` method mismatch (detail: registered method), `4` schema mismatch
//! (detail: registered schema), `5` codec mismatch (detail: registered
//! codec), `6` malformed request, `7` overloaded, `8` broken promise, `9`
//! reply too large, `10` reply encoding failed, `11` method not found in
//! the group, `12` interface mismatch (detail: registered interface `<< 16`
//! | registered interface version), `13` stream failed by its producer
//! (detail: the application's code), `14` stream protocol violation (a
//! regressing, excess or misaligned acknowledgement), `15` streaming
//! mismatch (a stream request for a unary method or the reverse; detail:
//! `1` when the endpoint streams). Version 2 adds `16` unauthenticated
//! (detail: the [`CredentialError`] code), `17` permission denied and `18`
//! shutting down (the server's runtime is draining). Detail is `0` when it
//! carries nothing.
//!
//! # Versions
//!
//! | Version | Adds |
//! |---|---|
//! | 1 | everything above except what version 2 adds: handshake, requests, replies, liveness, reply streams (stream frames, flag and statuses 13–15 were added to version 1 in place by #216, before any release) |
//! | 2 | the metadata section carries credentials ([`metadata`](super::metadata)) and the server verifies them; statuses 16–18 |
//!
//! Every frame layout is identical in both versions: version 2 changes
//! what the metadata section means and adds status codes. The rule that
//! keeps an older peer safe: **nothing introduced after the negotiated
//! version is ever sent**. A rejection a version 1 peer cannot decode is
//! sent as the version 1 status that proves the same about execution
//! ([`WireError::for_version`]: a denial becomes "endpoint not found", as
//! `FoundationDB`'s unauthorized-endpoint notice marks the endpoint failed
//! for its caller; shutting down becomes "overloaded"), and a version 1
//! session ignores credentials, as version 1 specifies. A server that
//! verifies credentials speaks only version 2, so an older client is
//! refused at the handshake rather than let through without one. The
//! decoder is version-aware too ([`decode_message_at`]): a status newer
//! than the session's version is a protocol violation, never guessed at.
//!
//! The golden vectors of both versions are in `tests/fixtures/wire-v1.txt`
//! and `tests/fixtures/wire-v2.txt`. Changing a layout, a kind or a status
//! code requires a new [`PROTOCOL_VERSION`].

use thiserror::Error;

use super::cursor::{Reader, Writer};
use super::schema::{MethodId, SchemaVersion};
use crate::codec::CodecId;
use crate::endpoint::{EndpointToken, Incarnation};
use crate::interface::InterfaceId;
use crate::security::CredentialError;

/// Magic number opening every connection's first frame (`"MPRC"`).
pub const PROTOCOL_MAGIC: u32 = 0x4d50_5243;

/// The newest envelope version this build speaks.
pub const PROTOCOL_VERSION: u16 = 2;

/// The oldest envelope version this build speaks: the supported window
/// for rolling upgrades is `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION`.
///
/// A peer whose announced range does not overlap the runtime's
/// [`RpcConfig::protocol_versions`](crate::RpcConfig::protocol_versions)
/// (within this window) is refused before any request is admitted.
pub const MIN_PROTOCOL_VERSION: u16 = 1;

/// The first version whose sessions carry verified credentials.
pub const CREDENTIALS_VERSION: u16 = 2;

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
const KIND_STREAM_ITEM: u8 = 0x06;
const KIND_STREAM_END: u8 = 0x07;
const KIND_STREAM_ACK: u8 = 0x08;
const KIND_STREAM_CANCEL: u8 = 0x09;

/// `flags` bit of a one-way request: no reply or rejection is ever sent.
pub const REQUEST_FLAG_ONE_WAY: u8 = 0x01;
/// `flags` bit of a request that opens a reply stream; the envelope then
/// carries the caller's credit window.
pub const REQUEST_FLAG_STREAM: u8 = 0x02;
const REQUEST_FLAGS_KNOWN: u8 = REQUEST_FLAG_ONE_WAY | REQUEST_FLAG_STREAM;

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
        /// The interface of the group the reference names (zero: a
        /// single-method endpoint).
        interface: InterfaceId,
        /// That interface's version (zero with interface zero).
        interface_version: SchemaVersion,
        /// The method the caller invokes.
        method: MethodId,
        /// The contract version the caller encoded with.
        schema: SchemaVersion,
        /// The codec that produced `body`.
        codec: CodecId,
        /// Request flags ([`REQUEST_FLAG_ONE_WAY`], [`REQUEST_FLAG_STREAM`]);
        /// unknown bits are a protocol violation.
        flags: u8,
        /// With [`REQUEST_FLAG_STREAM`]: the caller's credit window, the
        /// accounted bytes it buffers unconsumed. Not on the wire (and zero)
        /// otherwise.
        stream_window: u64,
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
    /// One item of a reply stream.
    StreamItem {
        /// The stream: the `call_id` of the request that opened it.
        call_id: u64,
        /// The item's position in the stream, from zero.
        sequence: u64,
        /// The codec that produced `body`.
        codec: CodecId,
        /// The encoded item.
        body: Vec<u8>,
    },
    /// The single terminal frame of a reply stream.
    StreamEnd {
        /// The stream.
        call_id: u64,
        /// How many items the producer sent.
        items: u64,
        /// `None` for a normal end, else why the stream ended.
        error: Option<WireError>,
    },
    /// Cumulative consumption acknowledgement of a reply stream.
    StreamAck {
        /// The stream.
        call_id: u64,
        /// Accounted bytes of items the caller's application consumed.
        consumed: u64,
    },
    /// The caller abandoned a reply stream.
    StreamCancel {
        /// The stream.
        call_id: u64,
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
    /// The endpoint is a group that does not serve the method.
    MethodNotFound,
    /// The endpoint serves another interface than the reference names.
    InterfaceMismatch {
        /// The interface and version the endpoint was registered with
        /// (zero: a single-method endpoint).
        registered: (InterfaceId, SchemaVersion),
    },
    /// The producer ended the stream with an application error code.
    StreamFailed {
        /// The application's code.
        code: u64,
    },
    /// The other side broke the stream protocol (a regressing, excess or
    /// misaligned acknowledgement); the stream was ended.
    StreamProtocol,
    /// A stream request for a unary method, or a unary request for a
    /// streaming method.
    StreamingMismatch {
        /// Whether the endpoint's method streams its replies.
        endpoint_streams: bool,
    },
    /// The caller is not (validly) authenticated for the endpoint
    /// (version 2).
    Unauthenticated {
        /// Why its credential was not accepted.
        reason: CredentialError,
    },
    /// The caller is authenticated but not allowed to call the endpoint
    /// (version 2).
    PermissionDenied,
    /// The server's runtime is shutting down and admits nothing new
    /// (version 2).
    ShuttingDown,
}

impl WireError {
    /// The first protocol version that defines this status.
    #[must_use]
    pub const fn since(self) -> u16 {
        match self {
            Self::Unauthenticated { .. } | Self::PermissionDenied | Self::ShuttingDown => 2,
            _ => 1,
        }
    }

    /// This rejection as a session at `version` can carry it: unchanged
    /// when the version defines it, else the closest status of that
    /// version that proves the same about execution (every one of these is
    /// a refusal before admission). A denial becomes
    /// [`EndpointNotFound`](Self::EndpointNotFound) (the endpoint is out
    /// of reach for that caller, as `FoundationDB`'s unauthorized-endpoint
    /// notice tells its failure monitor), shutting down becomes
    /// [`Overloaded`](Self::Overloaded).
    #[must_use]
    pub const fn for_version(self, version: u16) -> Self {
        if self.since() <= version {
            return self;
        }
        match self {
            Self::ShuttingDown => Self::Overloaded,
            _ => Self::EndpointNotFound,
        }
    }

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
            Self::MethodNotFound => (11, 0),
            Self::InterfaceMismatch {
                registered: (interface, version),
            } => (
                12,
                (u64::from(interface.get()) << 16) | u64::from(version.get()),
            ),
            Self::StreamFailed { code } => (13, code),
            Self::StreamProtocol => (14, 0),
            Self::StreamingMismatch { endpoint_streams } => (15, u64::from(endpoint_streams)),
            Self::Unauthenticated { reason } => (16, u64::from(reason.code())),
            Self::PermissionDenied => (17, 0),
            Self::ShuttingDown => (18, 0),
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
            11 => Self::MethodNotFound,
            12 => Self::InterfaceMismatch {
                registered: (
                    InterfaceId::new(
                        u32::try_from(detail >> 16)
                            .map_err(|_| EnvelopeError::InvalidField("interface"))?,
                    ),
                    SchemaVersion::new(u16::try_from(detail & 0xffff).unwrap_or(0)),
                ),
            },
            13 => Self::StreamFailed { code: detail },
            14 => Self::StreamProtocol,
            15 => Self::StreamingMismatch {
                endpoint_streams: match detail {
                    0 => false,
                    1 => true,
                    _ => return Err(EnvelopeError::InvalidField("streaming")),
                },
            },
            16 => Self::Unauthenticated {
                reason: u8::try_from(detail)
                    .ok()
                    .and_then(CredentialError::from_code)
                    .ok_or(EnvelopeError::InvalidField("credential error"))?,
            },
            17 => Self::PermissionDenied,
            18 => Self::ShuttingDown,
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
            interface,
            interface_version,
            method,
            schema,
            codec,
            flags,
            stream_window,
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
                .u32(interface.get())
                .u16(interface_version.get())
                .u32(method.get())
                .u16(schema.get())
                .u16(codec.get())
                .u8(*flags);
            if flags & REQUEST_FLAG_STREAM != 0 {
                out.u64(*stream_window);
            }
            out.u16(u16::try_from(metadata.len()).unwrap_or(u16::MAX))
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
        stream => encode_stream(&mut out, stream),
    }
    out.0
}

/// Encode a reply stream frame.
fn encode_stream(out: &mut Writer, message: &WireMessage) {
    match message {
        WireMessage::StreamItem {
            call_id,
            sequence,
            codec,
            body,
        } => {
            out.u8(KIND_STREAM_ITEM)
                .u64(*call_id)
                .u64(*sequence)
                .u16(codec.get())
                .bytes(body);
        }
        WireMessage::StreamEnd {
            call_id,
            items,
            error,
        } => {
            let (status, detail) = error.map_or((0, 0), WireError::status_and_detail);
            out.u8(KIND_STREAM_END)
                .u64(*call_id)
                .u64(*items)
                .u8(status)
                .u64(detail);
        }
        WireMessage::StreamAck { call_id, consumed } => {
            out.u8(KIND_STREAM_ACK).u64(*call_id).u64(*consumed);
        }
        WireMessage::StreamCancel { call_id } => {
            out.u8(KIND_STREAM_CANCEL).u64(*call_id);
        }
        // Encoded by `encode_message`.
        WireMessage::Hello { .. }
        | WireMessage::Request { .. }
        | WireMessage::Reply { .. }
        | WireMessage::Ping { .. }
        | WireMessage::Pong { .. } => {}
    }
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
    1 + 8 + 16 + 8 + 4 + 4 + 2 + 4 + 2 + 2 + 1 + 2 + body_len
}

/// Size of an ok-reply envelope carrying a `body_len`-byte body.
#[must_use]
pub const fn reply_envelope_len(body_len: usize) -> usize {
    1 + 8 + 1 + 2 + body_len
}

/// Size of a stream-opening request envelope carrying a `body_len`-byte
/// body (the request plus its credit window).
#[must_use]
pub const fn stream_request_envelope_len(body_len: usize) -> usize {
    request_envelope_len(body_len) + 8
}

/// Size of a `STREAM_ITEM` envelope carrying a `body_len`-byte body.
#[must_use]
pub const fn stream_item_envelope_len(body_len: usize) -> usize {
    1 + 8 + 8 + 2 + body_len
}

/// The accounted size of one stream item, in bytes: its whole frame
/// (frame header plus `STREAM_ITEM` envelope) for a `body_len`-byte
/// encoded body. The unit of stream credit on both sides. Saturating, so
/// an absurd length can only be refused, never wrap.
#[must_use]
pub const fn stream_item_frame_len(body_len: usize) -> u64 {
    let fixed = (super::frame::HEADER_LEN + stream_item_envelope_len(0)) as u64;
    fixed.saturating_add(body_len as u64)
}

/// Size of a `STREAM_END` envelope.
pub const STREAM_END_ENVELOPE_LEN: usize = 1 + 8 + 8 + 1 + 8;

/// Size of a `STREAM_ACK` envelope (the largest caller-to-server stream
/// signal).
pub const STREAM_ACK_ENVELOPE_LEN: usize = 1 + 8 + 8;

/// The length of a request envelope's metadata (credential) section,
/// read without decoding the rest; `None` for anything but a well-formed
/// request prefix.
#[must_use]
pub fn request_metadata_len(payload: &[u8]) -> Option<usize> {
    let mut input = Reader::new(payload);
    if input.u8()? != KIND_REQUEST {
        return None;
    }
    // call id, incarnation, token, interface, interface version, method,
    // schema, codec.
    input.slice(8 + 16 + 8 + 4 + 4 + 2 + 4 + 2 + 2)?;
    let flags = input.u8()?;
    if flags & REQUEST_FLAG_STREAM != 0 {
        input.u64()?;
    }
    input.u16().map(usize::from)
}

/// Decode one frame payload of the newest version ([`PROTOCOL_VERSION`]).
///
/// # Errors
///
/// An [`EnvelopeError`] for anything that is not exactly one well-formed
/// message.
pub fn decode_message(payload: &[u8]) -> Result<WireMessage, EnvelopeError> {
    decode_message_at(payload, PROTOCOL_VERSION)
}

/// Decode one frame payload received on a session running `version`.
///
/// # Errors
///
/// An [`EnvelopeError`] for anything that is not exactly one well-formed
/// message of that version: a status introduced by a later version is
/// [`EnvelopeError::UnknownStatus`], as an older build would see it.
pub fn decode_message_at(payload: &[u8], version: u16) -> Result<WireMessage, EnvelopeError> {
    let message = decode_any(payload)?;
    let error = match &message {
        WireMessage::Reply {
            outcome: WireOutcome::Err(error),
            ..
        }
        | WireMessage::StreamEnd {
            error: Some(error), ..
        } => Some(*error),
        _ => None,
    };
    if let Some(error) = error
        && error.since() > version
    {
        return Err(EnvelopeError::UnknownStatus(error.status_and_detail().0));
    }
    Ok(message)
}

fn decode_any(payload: &[u8]) -> Result<WireMessage, EnvelopeError> {
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
            let interface = InterfaceId::new(input.u32().ok_or_else(truncated)?);
            let interface_version = SchemaVersion::new(input.u16().ok_or_else(truncated)?);
            let method = MethodId::new(input.u32().ok_or_else(truncated)?);
            let schema = SchemaVersion::new(input.u16().ok_or_else(truncated)?);
            let codec = CodecId::new(input.u16().ok_or_else(truncated)?);
            let flags = input.u8().ok_or_else(truncated)?;
            let both = REQUEST_FLAG_ONE_WAY | REQUEST_FLAG_STREAM;
            if flags & !REQUEST_FLAGS_KNOWN != 0 || flags & both == both {
                return Err(EnvelopeError::InvalidField("flags"));
            }
            let stream_window = if flags & REQUEST_FLAG_STREAM == 0 {
                0
            } else {
                input.u64().ok_or_else(truncated)?
            };
            let metadata_len = input.u16().ok_or_else(truncated)?;
            let metadata = input
                .slice(usize::from(metadata_len))
                .ok_or_else(truncated)?
                .to_vec();
            WireMessage::Request {
                call_id,
                incarnation,
                token: EndpointToken::from_parts(index, generation),
                interface,
                interface_version,
                method,
                schema,
                codec,
                flags,
                stream_window,
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
        KIND_STREAM_ITEM..=KIND_STREAM_CANCEL => decode_stream(kind, input)?,
        other => return Err(EnvelopeError::UnknownKind(other)),
    };
    Ok(message)
}

/// Decode a reply stream frame after its kind byte.
fn decode_stream(kind: u8, mut input: Reader<'_>) -> Result<WireMessage, EnvelopeError> {
    let truncated = || EnvelopeError::Truncated;
    let message = match kind {
        KIND_STREAM_ITEM => WireMessage::StreamItem {
            call_id: input.u64().ok_or_else(truncated)?,
            sequence: input.u64().ok_or_else(truncated)?,
            codec: CodecId::new(input.u16().ok_or_else(truncated)?),
            body: input.rest().to_vec(),
        },
        KIND_STREAM_END => {
            let call_id = input.u64().ok_or_else(truncated)?;
            let items = input.u64().ok_or_else(truncated)?;
            let status = input.u8().ok_or_else(truncated)?;
            let detail = input.u64().ok_or_else(truncated)?;
            let error = match (status, detail) {
                (0, 0) => None,
                (0, _) => return Err(EnvelopeError::InvalidField("stream end detail")),
                (status, detail) => Some(WireError::from_status(status, detail)?),
            };
            ended(&input)?;
            WireMessage::StreamEnd {
                call_id,
                items,
                error,
            }
        }
        KIND_STREAM_ACK => {
            let message = WireMessage::StreamAck {
                call_id: input.u64().ok_or_else(truncated)?,
                consumed: input.u64().ok_or_else(truncated)?,
            };
            ended(&input)?;
            message
        }
        KIND_STREAM_CANCEL => {
            let message = WireMessage::StreamCancel {
                call_id: input.u64().ok_or_else(truncated)?,
            };
            ended(&input)?;
            message
        }
        other => return Err(EnvelopeError::UnknownKind(other)),
    };
    Ok(message)
}

/// A fixed layout must end where it ends.
fn ended(input: &Reader<'_>) -> Result<(), EnvelopeError> {
    if input.is_empty() {
        Ok(())
    } else {
        Err(EnvelopeError::TrailingBytes)
    }
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
        WireError, WireMessage, WireOutcome, decode_message, decode_message_at, encode_message,
        negotiate, reply_envelope_len, request_envelope_len,
    };
    use crate::codec::CodecId;
    use crate::endpoint::{EndpointToken, Incarnation};
    use crate::interface::InterfaceId;
    use crate::protocol::{MethodId, SchemaVersion};
    use crate::security::CredentialError;

    fn request() -> WireMessage {
        WireMessage::Request {
            call_id: 5,
            incarnation: Incarnation::from_raw(0x0102),
            token: EndpointToken::from_parts(3, 1),
            interface: InterfaceId::new(0x4b56),
            interface_version: SchemaVersion::new(2),
            method: MethodId::new(0x20),
            schema: SchemaVersion::new(0x10),
            codec: CodecId::PROST,
            flags: 0,
            stream_window: 0,
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
        // Versions 1..=2: the layout is version 1's, only the range grew.
        let mut expected = vec![0x01, 0x43, 0x52, 0x50, 0x4d, 1, 0, 2, 0, 1];
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
        expected.extend([0x56, 0x4b, 0, 0]);
        expected.extend([2, 0]);
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
                registered: CodecId::new(2),
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

    /// Golden vectors of the stream frames and the stream-opening request.
    #[test]
    fn stream_golden_encodings_and_round_trips() {
        let mut open = request();
        if let WireMessage::Request {
            flags,
            stream_window,
            ..
        } = &mut open
        {
            *flags = super::REQUEST_FLAG_STREAM;
            *stream_window = 0x0102;
        }
        let bytes = encode_message(&open);
        assert_eq!(bytes.len(), super::stream_request_envelope_len(2));
        let at = request_envelope_len(0) - 3;
        assert_eq!(bytes[at], super::REQUEST_FLAG_STREAM);
        assert_eq!(&bytes[at + 1..at + 9], &[2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_message(&bytes), Ok(open));
        let mut both = bytes.clone();
        both[at] = super::REQUEST_FLAG_STREAM | super::REQUEST_FLAG_ONE_WAY;
        assert_eq!(
            decode_message(&both),
            Err(EnvelopeError::InvalidField("flags"))
        );

        let item = WireMessage::StreamItem {
            call_id: 5,
            sequence: 2,
            codec: CodecId::PROST,
            body: vec![9, 8],
        };
        let item_bytes = encode_message(&item);
        assert_eq!(
            item_bytes,
            [
                0x06, 5, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 9, 8
            ]
        );
        assert_eq!(item_bytes.len(), super::stream_item_envelope_len(2));
        assert_eq!(
            super::stream_item_frame_len(2),
            (crate::protocol::HEADER_LEN + item_bytes.len()) as u64
        );
        assert_eq!(super::stream_item_frame_len(usize::MAX), u64::MAX);
        let end = WireMessage::StreamEnd {
            call_id: 5,
            items: 3,
            error: None,
        };
        assert_eq!(
            encode_message(&end),
            [
                0x07, 5, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        assert_eq!(encode_message(&end).len(), super::STREAM_END_ENVELOPE_LEN);
        let failed = WireMessage::StreamEnd {
            call_id: 5,
            items: 0,
            error: Some(WireError::StreamFailed { code: 7 }),
        };
        let ack = WireMessage::StreamAck {
            call_id: 5,
            consumed: 31,
        };
        assert_eq!(
            encode_message(&ack),
            [0x08, 5, 0, 0, 0, 0, 0, 0, 0, 31, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(encode_message(&ack).len(), super::STREAM_ACK_ENVELOPE_LEN);
        let cancel = WireMessage::StreamCancel { call_id: 5 };
        assert_eq!(encode_message(&cancel), [0x09, 5, 0, 0, 0, 0, 0, 0, 0]);
        for message in [item, end, failed, ack, cancel] {
            assert_eq!(decode_message(&encode_message(&message)), Ok(message));
        }
        // A normal end carries no detail; fixed layouts end where they end.
        let mut detail = encode_message(&WireMessage::StreamEnd {
            call_id: 1,
            items: 0,
            error: None,
        });
        detail[18] = 1;
        assert_eq!(
            decode_message(&detail),
            Err(EnvelopeError::InvalidField("stream end detail"))
        );
        let mut trailing = encode_message(&WireMessage::StreamCancel { call_id: 1 });
        trailing.push(0);
        assert_eq!(decode_message(&trailing), Err(EnvelopeError::TrailingBytes));
        assert_eq!(
            decode_message(&[0x08, 1, 0, 0, 0, 0, 0, 0, 0, 1]),
            Err(EnvelopeError::Truncated)
        );
    }

    #[test]
    fn the_metadata_length_is_read_without_decoding() {
        let mut with_metadata = request();
        if let WireMessage::Request { metadata, .. } = &mut with_metadata {
            *metadata = vec![1, 2, 3];
        }
        assert_eq!(
            super::request_metadata_len(&encode_message(&with_metadata)),
            Some(3)
        );
        assert_eq!(
            super::request_metadata_len(&encode_message(&request())),
            Some(0)
        );
        if let WireMessage::Request {
            flags,
            stream_window,
            ..
        } = &mut with_metadata
        {
            *flags = super::REQUEST_FLAG_STREAM;
            *stream_window = 9;
        }
        assert_eq!(
            super::request_metadata_len(&encode_message(&with_metadata)),
            Some(3)
        );
        assert_eq!(
            super::request_metadata_len(&encode_message(&WireMessage::Ping { nonce: 1 })),
            None
        );
        assert_eq!(super::request_metadata_len(&[0x02, 1]), None);
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
            WireError::MethodNotFound,
            WireError::InterfaceMismatch {
                registered: (InterfaceId::new(u32::MAX), SchemaVersion::new(u16::MAX)),
            },
            WireError::InterfaceMismatch {
                registered: (InterfaceId::new(0), SchemaVersion::new(0)),
            },
            WireError::StreamFailed { code: u64::MAX },
            WireError::StreamProtocol,
            WireError::StreamingMismatch {
                endpoint_streams: true,
            },
            WireError::StreamingMismatch {
                endpoint_streams: false,
            },
            WireError::Unauthenticated {
                reason: CredentialError::Expired,
            },
            WireError::PermissionDenied,
            WireError::ShuttingDown,
        ];
        for error in errors {
            let message = WireMessage::Reply {
                call_id: 1,
                outcome: WireOutcome::Err(error),
            };
            assert_eq!(decode_message(&encode_message(&message)), Ok(message));
        }
    }

    /// Version 2 statuses: fixed codes, refused by a version 1 decoder,
    /// downgraded (never sent) to a version 1 peer with the same execution
    /// meaning.
    #[test]
    fn version_two_statuses_are_never_seen_by_a_version_one_session() {
        let denied = WireMessage::Reply {
            call_id: 2,
            outcome: WireOutcome::Err(WireError::Unauthenticated {
                reason: CredentialError::UnknownKey,
            }),
        };
        let bytes = encode_message(&denied);
        assert_eq!(
            bytes,
            [0x03, 2, 0, 0, 0, 0, 0, 0, 0, 16, 5, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(decode_message_at(&bytes, 2), Ok(denied));
        assert_eq!(
            decode_message_at(&bytes, 1),
            Err(EnvelopeError::UnknownStatus(16))
        );
        for (error, status) in [
            (WireError::PermissionDenied, 17),
            (WireError::ShuttingDown, 18),
        ] {
            let reply = encode_message(&WireMessage::Reply {
                call_id: 0,
                outcome: WireOutcome::Err(error),
            });
            assert_eq!(reply[9], status);
            assert_eq!(
                decode_message_at(&reply, 1),
                Err(EnvelopeError::UnknownStatus(status))
            );
            assert_eq!(error.since(), 2);
            assert_eq!(error.for_version(2), error);
            assert_eq!(error.for_version(1).since(), 1);
        }
        assert_eq!(
            WireError::PermissionDenied.for_version(1),
            WireError::EndpointNotFound
        );
        assert_eq!(
            WireError::ShuttingDown.for_version(1),
            WireError::Overloaded
        );
        assert_eq!(
            WireError::Overloaded.for_version(1),
            WireError::Overloaded,
            "a version 1 status is never rewritten"
        );
        // A stream end carrying a version 2 status is refused the same way.
        let end = encode_message(&WireMessage::StreamEnd {
            call_id: 1,
            items: 0,
            error: Some(WireError::ShuttingDown),
        });
        assert_eq!(
            decode_message_at(&end, 1),
            Err(EnvelopeError::UnknownStatus(18))
        );
        // An unknown credential code is invalid, never mapped to another.
        let mut unknown = bytes.clone();
        unknown[10] = 200;
        assert_eq!(
            decode_message(&unknown),
            Err(EnvelopeError::InvalidField("credential error"))
        );
        for error in CredentialError::all() {
            assert_eq!(CredentialError::from_code(error.code()), Some(*error));
        }
        assert_eq!(CredentialError::from_code(0), None);
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
