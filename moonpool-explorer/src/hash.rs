//! Stable hashing and string utilities used by assertion and bucket tables.

/// FNV-1a hash of a message string to a stable u32.
pub fn msg_hash(msg: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in msg.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Truncate `s` into a fixed-size NUL-padded buffer.
///
/// Reserves the last byte for a NUL terminator, so at most `N - 1` bytes of
/// `s` are copied. Remaining bytes (including the trailing one) are zero.
pub(crate) fn truncate_to_buf<const N: usize>(s: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    let n = s.len().min(N - 1);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    buf
}
