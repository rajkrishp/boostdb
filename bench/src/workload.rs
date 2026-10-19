//! Deterministic keys, values and random choices, so every run does exactly the same work.

pub const KEY_LEN: usize = 16;
pub const VALUE_LEN: usize = 100;
/// Key indices at or above this are never written, so looking one up is a miss.
pub const MISS_BASE: u64 = 1 << 62;

/// The splitmix64 finalizer. It is a bijection on `u64`, so distinct indices always give
/// distinct keys, and hits and misses can never collide.
pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The key for index `i`: 16 lowercase hex digits of a scrambled `i`. Inserting indices in
/// order therefore inserts keys in random order, and since the digits are fixed-width, key
/// order is numeric order, which lets scans pick ranges by arithmetic.
pub fn key(i: u64) -> [u8; KEY_LEN] {
    hex(mix64(i))
}

pub fn hex(x: u64) -> [u8; KEY_LEN] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0; KEY_LEN];
    for (n, byte) in out.iter_mut().enumerate() {
        *byte = DIGITS[(x >> (60 - 4 * n) & 0xf) as usize];
    }
    out
}

/// The value written to key `i` by write number `version`, `len` pseudo-random bytes.
pub fn value(i: u64, version: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(mix64(i) ^ version.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Whether the delete-heavy workload deletes key `i`: a fixed pseudo-random half.
pub fn deleted(i: u64) -> bool {
    mix64(i ^ 0x5eed_de1e_7e00_0000) & 1 == 0
}

/// A splitmix64 generator: tiny, fast, and plenty random for choosing keys.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix64(self.0)
    }

    /// A number in `[0, n)`.
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_fixed_width_hex_in_numeric_order() {
        assert_eq!(&hex(0x0123_4567_89ab_cdef), b"0123456789abcdef");
        let mut rng = Rng::new(1);
        for _ in 0..1000 {
            let (a, b) = (rng.next_u64(), rng.next_u64());
            assert_eq!(a.cmp(&b), hex(a).cmp(&hex(b)));
        }
    }

    #[test]
    fn values_are_deterministic_and_distinct() {
        assert_eq!(value(7, 0, VALUE_LEN), value(7, 0, VALUE_LEN));
        assert_eq!(value(7, 0, VALUE_LEN).len(), VALUE_LEN);
        assert_ne!(value(7, 0, VALUE_LEN), value(7, 1, VALUE_LEN));
        assert_ne!(value(7, 0, VALUE_LEN), value(8, 0, VALUE_LEN));
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = Rng::new(2);
        assert!((0..1000).all(|_| rng.below(10) < 10));
    }
}
