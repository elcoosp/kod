//! Shared hash helpers.
//!
//! The workspace has more than one place that wants an FNV-1a-64
//! digest over length-prefixed byte slices — the transcript-coherence
//! checker and the tool-loop guard both fingerprint sequences where
//! the boundary between adjacent fields must be part of the digest.
//! A local copy in each is a drift hazard: two implementations of the
//! same wire encoding that silently disagree when one gets a fix.
//! This module is the single source of truth.

/// FNV-1a-64 prime.
pub const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a-64 offset basis.
pub const FNV1A_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Feed one length-prefixed byte slice into an FNV-1a-64 accumulator.
///
/// Length-prefixing makes the encoding injective: `("ab", "c")` and
/// `("a", "bc")` produce different digests. A NUL separator would not
/// be — NUL is a valid byte inside a JSON string, so a body that
/// contained one would collide with a differently-split field
/// sequence.
pub fn fnv1a_64_feed(h: &mut u64, bytes: &[u8]) {
    for b in (bytes.len() as u64).to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV1A_PRIME);
    }
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV1A_PRIME);
    }
}
