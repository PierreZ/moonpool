/// A raw ping frame contains the sequence and its complement.
const FRAME_LEN: usize = 16;

pub(crate) type Frame = [u8; FRAME_LEN];

/// Encode a sequence number with a small integrity check for injected bit flips.
pub(crate) fn encode_frame(sequence: u64) -> Frame {
    let mut frame = [0; FRAME_LEN];
    frame[..8].copy_from_slice(&sequence.to_be_bytes());
    frame[8..].copy_from_slice(&(!sequence).to_be_bytes());
    frame
}

/// Decode a frame, rejecting corruption before it can become a false pong.
pub(crate) fn decode_frame(frame: &Frame) -> Option<u64> {
    let mut sequence = [0; 8];
    sequence.copy_from_slice(&frame[..8]);
    let sequence = u64::from_be_bytes(sequence);

    let mut complement = [0; 8];
    complement.copy_from_slice(&frame[8..]);
    let complement = u64::from_be_bytes(complement);
    (complement == !sequence).then_some(sequence)
}
