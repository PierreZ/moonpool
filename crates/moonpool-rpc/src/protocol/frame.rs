//! Length-prefixed, checksummed frames and the incremental parser.
//!
//! # Frame layout
//!
//! ```text
//! +----------------+---------------------+---------------------------+
//! | length: u32 LE | checksum: u64 LE    | payload: `length` bytes   |
//! +----------------+---------------------+---------------------------+
//!   bytes 0..4       bytes 4..12           bytes 12..12+length
//! ```
//!
//! - `length` counts the payload only (the envelope, see
//!   [`wire`](super::wire)). It is never zero.
//! - `checksum` is XXH3-64 over the four `length` bytes **followed by** the
//!   payload: the header field that frames the stream is covered too, so a
//!   flipped length bit cannot silently re-frame the stream into different
//!   messages.
//!
//! This mirrors FDB `FlowTransport` (`scanPackets`): a packet length, an
//! XXH3-64 checksum, `PACKET_LIMIT` checked before waiting for the body, the
//! checksum verified before anything is delivered, and `checksum_failed`
//! tearing the connection down. Unlike FDB, the checksum also covers the
//! length and is always on; the TLS package (#218) may make it optional on
//! connections whose record layer already authenticates every byte.
//!
//! Until the checksum is verified, `length` is used for exactly two things:
//! rejecting a frame larger than the configured maximum (without buffering
//! it, so a corrupt or hostile length cannot allocate more than
//! `HEADER_LEN + max_frame_bytes` plus one read chunk) and knowing how many
//! bytes to wait for. Nothing is decoded before the checksum matches. On a
//! mismatch the decoder fails permanently: the connection is closed, never
//! resynchronised, and its in-flight calls fail as disconnected.

use thiserror::Error;

/// Bytes in a frame header: payload length plus checksum.
pub const HEADER_LEN: usize = 12;

/// A frame the parser or the encoder refused.
///
/// Every variant is a protocol violation on the connection that produced it:
/// the connection is closed, never resynchronised, because a byte stream
/// whose framing is lost cannot be trusted again.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrameError {
    /// The declared (or encoded) payload is larger than the configured limit.
    #[error("frame payload of {size} bytes exceeds the {limit}-byte limit")]
    TooLarge {
        /// The declared or encoded payload size.
        size: u64,
        /// The configured maximum payload size.
        limit: u32,
    },
    /// The header declared an empty payload.
    #[error("frame declared an empty payload")]
    Empty,
    /// The length and payload do not match the checksum: corruption.
    #[error("frame checksum mismatch (carried {expected:#018x}, computed {computed:#018x})")]
    Checksum {
        /// The checksum carried in the header.
        expected: u64,
        /// The checksum of the bytes actually received.
        computed: u64,
    },
}

/// Wrap `payload` in a frame header.
///
/// # Errors
///
/// [`FrameError::TooLarge`] when the payload exceeds `max_frame_bytes`, and
/// [`FrameError::Empty`] for an empty payload.
pub fn encode_frame(payload: &[u8], max_frame_bytes: u32) -> Result<Vec<u8>, FrameError> {
    let size = payload.len() as u64;
    if size > u64::from(max_frame_bytes) {
        return Err(FrameError::TooLarge {
            size,
            limit: max_frame_bytes,
        });
    }
    if payload.is_empty() {
        return Err(FrameError::Empty);
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge {
        size,
        limit: max_frame_bytes,
    })?;
    let length = length.to_le_bytes();
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&length);
    frame.extend_from_slice(&checksum(length, payload).to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// XXH3-64 over the length bytes followed by the payload.
fn checksum(length: [u8; 4], payload: &[u8]) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&length);
    hasher.update(payload);
    hasher.digest()
}

/// Incremental frame parser over arbitrarily split input.
///
/// Feed it whatever a read returned — a partial header, several frames, the
/// tail of one frame and the head of the next — and pull complete payloads
/// with [`next_frame`](Self::next_frame). Split points never change the
/// frames produced.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    /// Start of the unconsumed bytes in `buffer`.
    start: usize,
    max_frame_bytes: u32,
    failed: Option<FrameError>,
}

impl FrameDecoder {
    /// A parser accepting payloads of at most `max_frame_bytes`.
    #[must_use]
    pub fn new(max_frame_bytes: u32) -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            max_frame_bytes,
            failed: None,
        }
    }

    /// Append received bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start >= self.buffer.len() / 2 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// Bytes buffered but not yet returned as a frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffer.len() - self.start
    }

    /// Whether the decoder is between frames (no partial frame buffered).
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.buffered() == 0
    }

    /// The next complete payload, `Ok(None)` when more bytes are needed.
    ///
    /// # Errors
    ///
    /// A [`FrameError`] once framing is lost. The decoder stays failed: every
    /// later call returns the same class of error rather than guessing at a
    /// resynchronisation point.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let pending = &self.buffer[self.start..];
        let Some(length_bytes) = pending.first_chunk::<4>().copied() else {
            return Ok(None);
        };
        let length = u32::from_le_bytes(length_bytes);
        // Bounded before the checksum can be checked: the only use of an
        // unverified length is to refuse it or to wait for that many bytes.
        if length > self.max_frame_bytes {
            let error = FrameError::TooLarge {
                size: u64::from(length),
                limit: self.max_frame_bytes,
            };
            self.failed = Some(error.clone());
            return Err(error);
        }
        if length == 0 {
            self.failed = Some(FrameError::Empty);
            return Err(FrameError::Empty);
        }
        let length = length as usize;
        if pending.len() < HEADER_LEN + length {
            return Ok(None);
        }
        let mut expected = [0; 8];
        expected.copy_from_slice(&pending[4..HEADER_LEN]);
        let expected = u64::from_le_bytes(expected);
        let payload = &pending[HEADER_LEN..HEADER_LEN + length];
        let computed = checksum(length_bytes, payload);
        if computed != expected {
            let error = FrameError::Checksum { expected, computed };
            self.failed = Some(error.clone());
            return Err(error);
        }
        let payload = payload.to_vec();
        self.start += HEADER_LEN + length;
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        }
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameDecoder, FrameError, HEADER_LEN, checksum, encode_frame};

    fn frames() -> Vec<Vec<u8>> {
        vec![b"a".to_vec(), vec![7; 300], b"hello world".to_vec()]
    }

    fn stream() -> Vec<u8> {
        frames()
            .iter()
            .flat_map(|payload| encode_frame(payload, 1024).expect("encodes"))
            .collect()
    }

    fn drain(decoder: &mut FrameDecoder) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(frame) = decoder.next_frame().expect("valid stream") {
            out.push(frame);
        }
        out
    }

    #[test]
    fn every_single_split_point_yields_the_same_frames() {
        let bytes = stream();
        for split in 0..=bytes.len() {
            let mut decoder = FrameDecoder::new(1024);
            decoder.feed(&bytes[..split]);
            let mut out = drain(&mut decoder);
            decoder.feed(&bytes[split..]);
            out.extend(drain(&mut decoder));
            assert_eq!(out, frames(), "split at {split}");
            assert!(decoder.is_idle());
        }
    }

    #[test]
    fn byte_at_a_time_and_every_pair_of_splits() {
        let bytes = stream();
        let mut decoder = FrameDecoder::new(1024);
        let mut out = Vec::new();
        for byte in &bytes {
            decoder.feed(std::slice::from_ref(byte));
            out.extend(drain(&mut decoder));
        }
        assert_eq!(out, frames());

        // Two split points across the first two frames' boundary region.
        let limit = (HEADER_LEN * 2 + 1 + 20).min(bytes.len());
        for first in 0..limit {
            for second in first..limit {
                let mut decoder = FrameDecoder::new(1024);
                let mut out = Vec::new();
                for chunk in [&bytes[..first], &bytes[first..second], &bytes[second..]] {
                    decoder.feed(chunk);
                    out.extend(drain(&mut decoder));
                }
                assert_eq!(out, frames(), "splits at {first}/{second}");
            }
        }
    }

    #[test]
    fn several_frames_in_one_read() {
        let mut decoder = FrameDecoder::new(1024);
        decoder.feed(&stream());
        assert_eq!(drain(&mut decoder), frames());
    }

    #[test]
    fn oversized_length_is_rejected_before_buffering_the_payload() {
        let mut decoder = FrameDecoder::new(16);
        let mut header = Vec::new();
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        decoder.feed(&header);
        assert_eq!(
            decoder.next_frame(),
            Err(FrameError::TooLarge {
                size: u64::from(u32::MAX),
                limit: 16
            })
        );
        // Failed decoders stay failed.
        assert!(decoder.next_frame().is_err());
        assert!(decoder.buffered() <= HEADER_LEN);
    }

    #[test]
    fn empty_frames_are_violations() {
        let mut decoder = FrameDecoder::new(16);
        decoder.feed(&[0; HEADER_LEN]);
        assert_eq!(decoder.next_frame(), Err(FrameError::Empty));
    }

    /// Feed `bytes` and report the first outcome other than "need more".
    fn outcome(bytes: &[u8], max: u32) -> Result<Option<Vec<u8>>, FrameError> {
        let mut decoder = FrameDecoder::new(max);
        decoder.feed(bytes);
        decoder.next_frame()
    }

    #[test]
    fn every_single_bit_flip_in_payload_or_checksum_is_detected() {
        let frame = encode_frame(b"a payload with some length", 64).expect("encodes");
        for byte in 4..frame.len() {
            for bit in 0..8 {
                let mut corrupt = frame.clone();
                corrupt[byte] ^= 1 << bit;
                assert!(
                    matches!(outcome(&corrupt, 64), Err(FrameError::Checksum { .. })),
                    "flip at byte {byte} bit {bit} must fail the checksum"
                );
            }
        }
    }

    #[test]
    fn length_bit_flips_never_deliver_a_frame() {
        // Two frames back to back, so a shortened length has bytes to frame.
        let mut stream = encode_frame(b"first frame payload", 64).expect("encodes");
        stream.extend(encode_frame(b"second", 64).expect("encodes"));
        for byte in 0..4 {
            for bit in 0..8 {
                let mut corrupt = stream.clone();
                corrupt[byte] ^= 1 << bit;
                let mut decoder = FrameDecoder::new(64);
                decoder.feed(&corrupt);
                // Either an oversize/empty/checksum violation, or waiting for
                // bytes that never come: never a delivered payload.
                assert!(
                    !matches!(decoder.next_frame(), Ok(Some(_))),
                    "flip at length byte {byte} bit {bit} delivered a frame"
                );
            }
        }
    }

    #[test]
    fn truncated_checksum_is_never_a_frame() {
        let frame = encode_frame(b"abc", 16).expect("encodes");
        for cut in 0..frame.len() {
            let mut decoder = FrameDecoder::new(16);
            decoder.feed(&frame[..cut]);
            assert_eq!(decoder.next_frame(), Ok(None), "cut at {cut}");
            assert!(
                !decoder.is_idle() || cut == 0,
                "partial frame stays buffered"
            );
        }
    }

    #[test]
    fn encoder_enforces_the_same_limit() {
        assert_eq!(
            encode_frame(&[0; 17], 16),
            Err(FrameError::TooLarge {
                size: 17,
                limit: 16
            })
        );
        assert_eq!(encode_frame(&[], 16), Err(FrameError::Empty));
        assert_eq!(encode_frame(&[0; 16], 16).map(|f| f.len()), Ok(28));
    }

    #[test]
    fn header_is_little_endian_length_then_checksum_over_length_and_payload() {
        let frame = encode_frame(b"abc", 16).expect("encodes");
        assert_eq!(&frame[..4], &3u32.to_le_bytes());
        let mut covered = 3u32.to_le_bytes().to_vec();
        covered.extend_from_slice(b"abc");
        assert_eq!(
            &frame[4..12],
            &xxhash_rust::xxh3::xxh3_64(&covered).to_le_bytes()
        );
        assert_eq!(
            checksum(3u32.to_le_bytes(), b"abc"),
            xxhash_rust::xxh3::xxh3_64(&covered)
        );
        assert_eq!(&frame[12..], b"abc");
    }
}
