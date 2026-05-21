//! FNV-1a 64-bit fold for the hash-chain digest.
//!
//! Not cryptographic; this is deterministic content addressing only. FNV-1a
//! was chosen for its bit-stable constants (unlike `DefaultHasher`, which is
//! explicitly not stable across Rust versions) and minimal implementation
//! surface. `fold(prev, &[])` is deliberately distinct from `prev` so that
//! `Append Block []` is a real state transition.

/// FNV-1a 64-bit offset basis.
pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Initial digest value used as `H_0` in the chain.
pub const INITIAL_DIGEST: u64 = 0x0000;

/// Fold `prev` and `block` into a new digest.
#[must_use]
pub fn fold(prev: u64, block: &[u8]) -> u64 {
    let mut h = FNV_OFFSET ^ prev.wrapping_mul(FNV_PRIME);
    for &b in &prev.to_be_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    for &b in block {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        assert_eq!(fold(0, b"x"), fold(0, b"x"));
    }

    #[test]
    fn empty_block_mutates() {
        assert_ne!(fold(INITIAL_DIGEST, b""), INITIAL_DIGEST);
    }

    #[test]
    fn distinct_blocks_diff() {
        assert_ne!(fold(0, b"a"), fold(0, b"b"));
    }

    #[test]
    fn distinct_prev_diff() {
        assert_ne!(fold(0, b"a"), fold(1, b"a"));
    }
}
