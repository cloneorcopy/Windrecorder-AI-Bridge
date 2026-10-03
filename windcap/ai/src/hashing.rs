//! A hash for cache keys, which is not the same thing as a hash for signatures.
//!
//! `wind-base` exposes `base64` and nothing else, and `Cargo.lock` carries no digest crate that this
//! crate may reach for without adding a dependency to the offline build. That would be a poor trade
//! for what is asked here: the value identifies *which set of window titles* a cached tag list was
//! derived from, so that a re-run over an unchanged month costs zero API calls.
//!
//! So the requirement is exactly: deterministic across runs and platforms, order-insensitive after
//! normalisation, and unlikely to collide by accident. FNV-1a 64-bit satisfies all three. It is not
//! collision resistant, and nothing here needs it to be — a collision means one redundant request and
//! one refreshed cache entry, not a security failure. If this value ever migrates to naming a file
//! the user trusts (a download, a backup, an integrity check), replace it with something from the
//! system CNG API and not with a hand-rolled SHA.

/// `offset64` and the FNV prime, per the reference implementation.
const OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01B3;

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The hex form that goes into a cache file and onto a terminal.
pub fn hex64(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a64(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference FNV-1a 64 values, computed with an independent implementation, so a changed constant
    /// is caught here and not in a user's cache.
    #[test]
    fn matches_the_reference_vectors() {
        assert_eq!(fnv1a64(b""), OFFSET);
        assert_eq!(hex64(b"a"), "af63dc4c8601ec8c");
        assert_eq!(hex64(b"foobar"), "85944171f73967e8");
        assert_eq!(hex64(b"hello world"), "779a65e7023cd2e7");
    }

    #[test]
    fn hashes_are_stable_and_distributed() {
        assert_eq!(hex64(b"2026-09 Chrome|Excel"), hex64(b"2026-09 Chrome|Excel"));
        assert_ne!(hex64(b"2026-09 Chrome|Excel"), hex64(b"2026-09 Chrome|Excels"));
        // A month boundary must not be confusable with the next one.
        assert_ne!(hex64(b"2026-09"), hex64(b"2026-10"));
    }
}
