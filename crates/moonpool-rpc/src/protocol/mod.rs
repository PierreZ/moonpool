//! Wire protocol: framing, the envelope and method/schema identity.
//!
//! Three layers, each validated before the next is looked at:
//!
//! 1. **Frame** ([`frame`]): `u32 LE length | u64 LE XXH3-64(length bytes ‖
//!    payload) | payload`. The length is bounded by
//!    [`RpcConfig::max_frame_bytes`](crate::RpcConfig::max_frame_bytes)
//!    before the payload is buffered; the checksum is verified before the
//!    payload is parsed.
//! 2. **Envelope** ([`wire`]): a hand-written, fixed-width, little-endian
//!    layout owned by this crate and versioned by [`PROTOCOL_VERSION`]. It
//!    carries everything the transport needs to route, bound and reject a
//!    message — kind, reply route (`call_id`), transport incarnation,
//!    endpoint token, [`MethodId`], [`SchemaVersion`] and
//!    [`CodecId`](crate::CodecId) — and never depends on a payload codec.
//! 3. **Body**: opaque bytes produced by a [`Wire`](crate::Wire)
//!    implementation, decoded only after the endpoint's method, schema and
//!    codec all matched ([`codec`](crate::codec)).
//!
//! Connection rules: the first frame each way is [`WireMessage::Hello`]
//! carrying [`PROTOCOL_MAGIC`], the sender's supported version range
//! (its [`RpcConfig::protocol_versions`](crate::RpcConfig::protocol_versions),
//! within `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION`), its
//! [`Incarnation`](crate::Incarnation), reserved feature bits, its frame
//! limit and its canonical listening address. A peer
//! with no common version, anything but a Hello first, a wrong magic, a
//! checksum mismatch or an unparsable envelope is a protocol violation: the
//! connection is closed (never resynchronised), its calls fail through the
//! ordinary disconnect path, and
//! [`RpcStats::protocol_violations`](crate::RpcStats::protocol_violations)
//! (plus [`RpcStats::checksum_failures`](crate::RpcStats::checksum_failures)
//! for corruption, or
//! [`RpcStats::version_rejections`](crate::RpcStats::version_rejections) for
//! an unsupported peer) counts it.

mod cursor;
pub mod frame;
pub mod metadata;
mod schema;
pub mod wire;

pub use frame::{FrameDecoder, FrameError, HEADER_LEN, encode_frame};
pub use schema::{MethodId, RpcMethod, SchemaVersion};
pub use wire::{
    CREDENTIALS_VERSION, EnvelopeError, HELLO_ENVELOPE_LEN, LIVENESS_ENVELOPE_LEN,
    MIN_PROTOCOL_VERSION, PROTOCOL_MAGIC, PROTOCOL_VERSION, REJECTION_ENVELOPE_LEN,
    REQUEST_FLAG_ONE_WAY, REQUEST_FLAG_STREAM, STREAM_ACK_ENVELOPE_LEN, STREAM_END_ENVELOPE_LEN,
    WireError, WireMessage, WireOutcome, decode_message, decode_message_at, encode_message,
    negotiate, reply_envelope_len, request_envelope_len, request_metadata_len,
    stream_item_envelope_len, stream_item_frame_len, stream_request_envelope_len,
};
