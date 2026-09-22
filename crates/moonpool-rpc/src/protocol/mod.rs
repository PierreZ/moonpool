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
//! (`MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION`), its
//! [`Incarnation`](crate::Incarnation) and reserved feature bits. A peer
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
mod schema;
pub mod wire;

pub(crate) use cursor::{Reader, Writer};
pub use frame::{FrameDecoder, FrameError, HEADER_LEN, encode_frame};
pub use schema::{MethodId, RpcMethod, SchemaVersion};
pub use wire::{
    EnvelopeError, MIN_PROTOCOL_VERSION, PROTOCOL_MAGIC, PROTOCOL_VERSION, WireError, WireMessage,
    WireOutcome, decode_message, encode_message, negotiate, reply_envelope_len,
    request_envelope_len,
};
