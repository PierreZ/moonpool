//! Malformed input, seeded and dependency-free: frames, envelopes (Hello,
//! requests, replies, stream items, ACKs, cancels, rejections), references,
//! credential sections and version ranges built from random bytes and from
//! mutated golden fixtures. Every decoder must refuse or accept without
//! panicking, and never allocate more than its input and configured limits
//! justify: a length field is only ever used to refuse or to wait.
//!
//! A counting global allocator records the largest single allocation the
//! test process makes; the bounds below are far under what trusting a
//! length field (up to 4 GiB) would ask for. nextest runs every test in its
//! own process, so each bound is the test's own.

#![cfg(feature = "prost")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use moonpool_rpc::protocol::metadata::bearer;
use moonpool_rpc::protocol::{
    FrameDecoder, HEADER_LEN, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, decode_message_at,
    encode_frame, encode_message, negotiate,
};
use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion, ServiceRef};

/// The system allocator, remembering the largest request it served.
struct Counting;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded unchanged to the system allocator; the
// only addition is an atomic maximum of the requested sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's contract is System's contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc`/`realloc` above, i.e. from System.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's contract is System's contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        // SAFETY: the caller's contract is System's contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn largest_allocation() -> usize {
    LARGEST.load(Ordering::Relaxed)
}

/// xorshift64*: a fixed seed gives the same inputs on every run.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound.max(1)).unwrap_or(u64::MAX);
        usize::try_from(self.next() % bound).unwrap_or(0)
    }

    fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.byte()).collect()
    }
}

/// One random edit of `input`: bit flip, byte overwrite, truncation,
/// insertion, duplication, or an extreme little-endian `u16`/`u32`/`u64`
/// written over a length or count field.
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    let edits = 1 + rng.below(3);
    for _ in 0..edits {
        let at = rng.below(out.len().max(1));
        match rng.below(7) {
            0 if !out.is_empty() => out[at] ^= 1 << rng.below(8),
            1 if !out.is_empty() => out[at] = rng.byte(),
            2 => out.truncate(at),
            3 => {
                let len = 1 + rng.below(16);
                let extra = rng.bytes(len);
                out.splice(at..at, extra);
            }
            4 if !out.is_empty() => {
                let end = (at + 1 + rng.below(16)).min(out.len());
                let copy = out[at..end].to_vec();
                out.splice(at..at, copy);
            }
            _ => {
                let extremes: [u64; 6] = [0, 1, 0x7f, 0xffff, 0xffff_ffff, u64::MAX];
                let value = extremes[rng.below(extremes.len())].to_le_bytes();
                let width = [2, 4, 8][rng.below(3)];
                for (offset, byte) in value.iter().take(width).enumerate() {
                    if let Some(slot) = out.get_mut(at + offset) {
                        *slot = *byte;
                    }
                }
            }
        }
    }
    out
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .filter_map(|index| u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok())
        .collect()
}

/// Every `name hex` line of a fixture file.
fn fixtures(file: &str) -> Vec<(String, Vec<u8>)> {
    file.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let (name, bytes) = line.split_once(' ')?;
            Some((name.to_string(), hex(bytes.trim())))
        })
        .collect()
}

const WIRE_V1: &str = include_str!("fixtures/wire-v1.txt");
const WIRE_V2: &str = include_str!("fixtures/wire-v2.txt");
const REFERENCES_V1: &str = include_str!("fixtures/references-v1.txt");

/// A frame limit well under what an untrusted length could ask for.
const FRAME_LIMIT: u32 = 64 * 1024;

/// Random bytes, valid frames, and valid frames with mutated headers
/// (lengths up to `u32::MAX`), fed in random slices: the decoder returns
/// only frames that were really sent, stays failed once framing is lost,
/// buffers at most one bounded frame, and never allocates for a claimed
/// length it has not received.
#[test]
fn random_frames_never_panic_or_overallocate() {
    let mut rng = Rng::new(0x5eed_0001);
    let payloads: Vec<Vec<u8>> = fixtures(WIRE_V1).into_iter().map(|(_, b)| b).collect();
    for _ in 0..3000 {
        let mut stream = Vec::new();
        for _ in 0..=rng.below(4) {
            let payload = payloads[rng.below(payloads.len())].clone();
            let frame = encode_frame(&payload, FRAME_LIMIT).expect("fixture fits");
            match rng.below(4) {
                0 => {
                    let len = rng.below(64);
                    stream.extend(rng.bytes(len));
                }
                1 => stream.extend(mutate(&mut rng, &frame)),
                _ => stream.extend(&frame),
            }
        }
        let mut decoder = FrameDecoder::new(FRAME_LIMIT);
        let mut offset = 0;
        let mut received = Vec::new();
        let mut failed = false;
        while offset < stream.len() && !failed {
            let end = (offset + 1 + rng.below(40)).min(stream.len());
            decoder.feed(&stream[offset..end]);
            offset = end;
            loop {
                match decoder.next_frame() {
                    Ok(Some(frame)) => received.push(frame),
                    Ok(None) => break,
                    Err(_) => {
                        failed = true;
                        assert!(
                            decoder.next_frame().is_err(),
                            "a failed decoder stays failed"
                        );
                        break;
                    }
                }
            }
            if !failed {
                assert!(
                    decoder.buffered() <= HEADER_LEN + FRAME_LIMIT as usize,
                    "at most one bounded frame is buffered"
                );
            }
        }
        // A checksum over the whole frame: whatever came out was sent.
        for frame in &received {
            assert!(
                payloads.contains(frame),
                "only frames that were sent come out"
            );
        }
    }
    assert!(
        largest_allocation() < 256 * 1024,
        "largest allocation {} bytes",
        largest_allocation()
    );
}

/// Every golden envelope (Hello, requests with and without credentials,
/// replies, rejections, stream items, ends, ACKs, cancels, pings), mutated
/// thousands of ways, decoded at every supported version: it decodes or is
/// refused, never panics, and anything that decodes re-encodes to a
/// message that decodes to itself.
#[test]
fn mutated_envelopes_decode_or_refuse_without_panicking() {
    let mut rng = Rng::new(0x5eed_0002);
    let mut decoded = 0u64;
    let mut refused = 0u64;
    let mut envelopes = fixtures(WIRE_V1);
    envelopes.extend(fixtures(WIRE_V2));
    assert!(envelopes.len() > 20, "the fixtures were read");
    for (_, envelope) in &envelopes {
        for _ in 0..400 {
            let input = if rng.below(8) == 0 {
                let len = rng.below(96);
                rng.bytes(len)
            } else {
                mutate(&mut rng, envelope)
            };
            for version in MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION {
                match decode_message_at(&input, version) {
                    Ok(message) => {
                        decoded += 1;
                        let again = encode_message(&message);
                        assert_eq!(
                            decode_message_at(&again, version).as_ref(),
                            Ok(&message),
                            "a decoded message round-trips"
                        );
                    }
                    Err(_) => refused += 1,
                }
            }
        }
    }
    assert!(
        decoded > 0 && refused > 0,
        "{decoded} decoded, {refused} refused"
    );
    assert!(
        largest_allocation() < 64 * 1024,
        "largest allocation {} bytes",
        largest_allocation()
    );
}

struct Echo;
impl RpcMethod for Echo {
    type Request = ();
    type Reply = ();
    const METHOD: MethodId = MethodId::new(0x6563_686f);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

/// Stored and forwarded references are untrusted input too: mutated
/// golden references (and random bytes) decode strictly or are refused,
/// and a reference that decodes re-encodes to itself.
#[test]
fn mutated_references_decode_or_refuse_without_panicking() {
    let mut rng = Rng::new(0x5eed_0003);
    let references = fixtures(REFERENCES_V1);
    assert!(!references.is_empty(), "the fixtures were read");
    let mut decoded = 0u64;
    for (_, reference) in &references {
        for _ in 0..2000 {
            let input = if rng.below(8) == 0 {
                let len = rng.below(80);
                rng.bytes(len)
            } else {
                mutate(&mut rng, reference)
            };
            if let Ok(service) = ServiceRef::<Echo>::from_bytes(&input) {
                decoded += 1;
                let again = ServiceRef::<Echo>::from_bytes(&service.to_bytes());
                assert!(
                    again.is_ok_and(|again| again.endpoint() == service.endpoint()),
                    "a decoded reference round-trips"
                );
            }
        }
    }
    assert!(decoded > 0, "some mutations stay valid references");
    assert!(
        largest_allocation() < 64 * 1024,
        "largest allocation {} bytes",
        largest_allocation()
    );
}

/// Credential sections: random and mutated sections are bounded before
/// they are read, and a bearer credential found is always inside them.
#[test]
fn credential_sections_are_bounded_and_never_panic() {
    let mut rng = Rng::new(0x5eed_0004);
    let valid = moonpool_rpc::protocol::metadata::encode_bearer(b"token-bytes").expect("fits");
    for _ in 0..20_000 {
        let section = if rng.below(2) == 0 {
            let len = rng.below(300);
            rng.bytes(len)
        } else {
            mutate(&mut rng, &valid)
        };
        let limit = rng.below(256);
        match bearer(&section, limit) {
            Ok(Some(credential)) => {
                assert!(section.len() <= limit && !credential.is_empty());
                assert!(credential.len() + 3 <= section.len());
            }
            Ok(None) => assert!(section.len() <= limit),
            Err(_) => {}
        }
    }
}

/// Version ranges from a peer's Hello: any pair of ranges negotiates to a
/// version inside both, or to nothing.
#[test]
fn any_version_ranges_negotiate_inside_both_or_not_at_all() {
    let mut rng = Rng::new(0x5eed_0005);
    let draw = |rng: &mut Rng| -> u16 {
        match rng.below(4) {
            0 => 0,
            1 => u16::MAX,
            _ => u16::try_from(rng.below(6)).unwrap_or(0),
        }
    };
    for _ in 0..20_000 {
        let local = (draw(&mut rng), draw(&mut rng));
        let peer = (draw(&mut rng), draw(&mut rng));
        if let Some(version) = negotiate(local, peer) {
            assert!(
                local.0 <= version && version <= local.1,
                "{local:?} {version}"
            );
            assert!(peer.0 <= version && version <= peer.1, "{peer:?} {version}");
        }
    }
}
