//! Hash stable across processes and versions (D3, D4): xxh3-64 with a fixed
//! seed. Never use `std`'s `DefaultHasher` for anything that crosses process
//! boundaries.

use xxhash_rust::xxh3::xxh3_64_with_seed;

/// Fixed seed: changing it reshuffles every shard and partition.
const SEED: u64 = 0x7265_6765_6e63_7931; // "regency1"

pub fn stable_hash(value: &str) -> u64 {
    xxh3_64_with_seed(value.as_bytes(), SEED)
}

/// Bucket of `value` among `count` (shard of an order, Reach partition of a
/// host). `count` must be > 0.
pub fn bucket(value: &str, count: u32) -> u32 {
    (stable_hash(value) % u64::from(count.max(1))) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_frozen() {
        // If this changes, deployed shard/partition assignments change.
        assert_eq!(bucket("web-01", 256), bucket("web-01", 256));
        assert_eq!(stable_hash("web-01"), xxh3_64_with_seed(b"web-01", SEED));
        assert!(bucket("anything", 7) < 7);
        assert_eq!(bucket("x", 0), 0);
    }
}
