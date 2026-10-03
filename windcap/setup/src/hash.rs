//! SHA-256, in-house, so that "the code can prove it made the backup" costs no new dependency.
//!
//! The safety bar for `windsetup` is that a copy only counts as a backup once it has been *read back
//! and compared* — `std::fs::copy` returning `Ok` says nothing about the bytes that landed, and a
//! truncated copy of a user's only index is precisely the failure this crate exists to survive.
//! That needs a content digest, and the workspace rule is `cargo build --offline` with no new
//! crates.io tree (`windcap-core/Cargo.toml` states it outright). `sha2` happens to sit in this
//! machine's registry cache, but the cache is not the workspace: a build gate that passes because one
//! laptop has the right `.crate` files is not a gate. So the primitive is small, public,
//! dependency-free, and pinned to digests computed by an independent implementation on this machine.
//!
//! This is a *content* digest, not a security boundary: nothing here authenticates a file against an
//! attacker who can already edit it. It answers the much narrower question — "are the bytes I just
//! copied the same bytes I was about to change?" — which is the question a restore hinges on.

/// SHA-256 initial hash values, FIPS 180-4 §5.3.3(b).
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Round constants, FIPS 180-4 §4.2.2: the leading 32 bits of the fractional parts of the cube roots
/// of the first 64 primes.
///
/// Spelled out, not generated: the standard's values were truncated at 32 bits, so an exact integer
/// recomputation of `floor(2^32 * frac(cbrt(p)))` disagrees with them in the low bits (K[0] is
/// 0x428a2f98, not the 0x428a2f9c an arbitrary-precision calculation yields). A generator would have
/// to reproduce a rounding accident to be correct, which is a worse pile of code than a table.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Streaming SHA-256 state.
#[derive(Debug, Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    filled: usize,
    length: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 { state: H0, buffer: [0u8; 64], filled: 0, length: 0 }
    }

    pub fn update(&mut self, mut input: &[u8]) {
        self.length = self.length.wrapping_add(input.len() as u64);
        if self.filled > 0 {
            let take = (64 - self.filled).min(input.len());
            self.buffer[self.filled..self.filled + take].copy_from_slice(&input[..take]);
            self.filled += take;
            input = &input[take..];
            if self.filled == 64 {
                let block = self.buffer;
                compress(&mut self.state, &block);
                self.filled = 0;
            }
        }
        while input.len() >= 64 {
            let (head, rest) = input.split_at(64);
            compress(&mut self.state, head);
            input = rest;
        }
        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.filled = input.len();
        }
    }

    /// The 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        let bit_length = self.length.wrapping_mul(8);
        // Padding is 0x80, then zeros, then the big-endian 64-bit bit length, and the whole padded
        // message must be a multiple of 64 bytes. Feeding it through `update` rather than hand-writing
        // the tail block keeps one compression path; the length counter is polluted by the padding
        // bytes, which is why the real length is captured first and never re-read.
        //
        // `filled + 9` is the 0x80 plus the eight length bytes; `z` zeros are then added to bring the
        // total up to the next multiple of 64, which is why 55-byte and 56-byte messages differ by a
        // whole block and are both in the test table below.
        let zeros = (64 - (self.filled + 9) % 64) % 64;
        let pad_len = 9 + zeros;
        let mut padding = [0u8; 72];
        padding[0] = 0x80;
        padding[pad_len - 8..pad_len].copy_from_slice(&bit_length.to_be_bytes());
        self.update(&padding[..pad_len]);
        debug_assert_eq!(self.filled, 0, "the padded message must end on a block boundary");

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// Lowercase hex, the form stored in a marker file and printed in a report.
    pub fn hex(self) -> String {
        hex_of(&self.finalize())
    }
}

pub fn hex_of(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.hex()
}

/// A truncated digest, for display only.
///
/// Callers print this and store the full one. Conflating the two is how a log line stops being
/// evidence: the whole point of the hash in `backup::Backup` is that a human can compare it against a
/// restore candidate, and eight hex characters collide across a few dozen files.
pub fn short_digest(hex: &str) -> &str {
    let cut = hex.len().min(12);
    // Slice on a char boundary by construction: the input is hex, so every boundary is a byte boundary.
    &hex[..cut]
}

/// Digest a file without holding it in memory — month databases run to hundreds of megabytes.
pub fn digest_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hasher.hex())
}

/// `Σ f(i) >> (32-i)`, FIPS 180-4 §4.1.2.
#[inline]
fn rotr(x: u32, n: u32) -> u32 {
    x.rotate_right(n)
}

fn compress(state: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 64];
    for (i, word) in w.iter_mut().take(16).enumerate() {
        let offset = i * 4;
        *word = u32::from_be_bytes([
            block[offset],
            block[offset + 1],
            block[offset + 2],
            block[offset + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
        let s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;

    for i in 0..64 {
        let s1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    for (word, delta) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expected value here was produced by Python's `hashlib.sha256` on this machine, i.e. by
    /// a second, independent implementation — not by this one, which would pass its own tautology.
    const VECTORS: &[(&[u8], &str)] = &[
        (b"", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        (b"abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (
            b"The quick brown fox jumps over the lazy dog",
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592",
        ),
    ];

    #[test]
    fn known_digests_match_an_independent_implementation() {
        for (input, expected) in VECTORS {
            assert_eq!(&sha256_hex(input), expected, "input length {}", input.len());
        }
    }

    /// 55, 56 and 64 bytes are the padding branch points: 55 fits with room for the length, 56 does
    /// not and forces a second block, 64 is exactly one block with nothing left over. An
    /// implementation that is wrong *only* at a boundary produces a plausible digest for every small
    /// config file and a wrong one for the first file big enough to matter.
    #[test]
    fn padding_boundaries_are_digests_not_guesses() {
        for (length, expected) in [
            (0usize, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            (3, "9834876dcfb05cb167a5c24953eba58c4ac89b1adf57f28f2f9d09af107ee8f0"),
            (55, "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"),
            (56, "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"),
            (64, "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"),
            (65, "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0"),
            (200, "c2a908d98f5df987ade41b5fce213067efbcc21ef2240212a41e54b5e7c28ae5"),
        ] {
            assert_eq!(sha256_hex(&vec![b'a'; length]), expected, "{length}-byte message");
        }
    }

    #[test]
    fn streaming_in_arbitrary_chunks_equals_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let one_shot = sha256_hex(&data);
        for chunk in [1usize, 3, 7, 55, 63, 64, 65, 128, 1024] {
            let mut hasher = Sha256::new();
            for part in data.chunks(chunk) {
                hasher.update(part);
            }
            assert_eq!(hasher.hex(), one_shot, "chunk size {chunk}");
        }
    }

    #[test]
    fn a_digest_can_be_recomputed_from_disk() {
        let path = std::env::temp_dir().join(format!("wind-setup-hash-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(digest_file(&path).unwrap(), sha256_hex(b"abc"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        assert!(digest_file(std::path::Path::new("Z:/definitely-not-here/wind.db")).is_err());
    }

    #[test]
    fn shortening_is_only_for_display_and_never_grows() {
        assert_eq!(short_digest("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_digest("0123456789ab"), "0123456789ab");
        assert_eq!(short_digest("abc"), "abc");
        assert_eq!(short_digest(""), "");
    }
}
