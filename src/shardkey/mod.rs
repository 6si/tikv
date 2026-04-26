use xxhash_rust::xxh3::xxh3_64;

/// ShardKeyInfo describes the shard key for MPP co-location optimization.
/// Tables with matching shard keys can skip exchange during joins.
#[derive(Clone, Debug, PartialEq)]
pub struct ShardKeyInfo {
    /// Column names forming the shard key
    pub columns: Vec<String>,
    /// Number of shards (must be >= 1)
    pub shard_cnt: usize,
}

/// Compute which shard a row belongs to based on column values.
/// Uses xxHash3 (64-bit) for consistency with TiDB's cespare/xxhash/v2.
///
/// `values` should be the concatenated column values encoded as bytes.
/// `shard_cnt` is the total number of shards for the table.
pub fn compute_shard(values: &[u8], shard_cnt: usize) -> usize {
    debug_assert!(shard_cnt > 0, "shard_cnt must be positive");
    let hash = xxh3_64(values);
    (hash as usize) % shard_cnt
}

/// Compute the hash boundary for a shard's region.
/// Returns (start_hash, end_hash) where end_hash is exclusive.
/// Used for pre-splitting regions at table creation time.
pub fn calculate_hash_boundary(shard_id: usize, shard_cnt: usize) -> (u64, u64) {
    debug_assert!(shard_id < shard_cnt, "shard_id must be less than shard_cnt");
    let range = u64::MAX / shard_cnt as u64;
    let start = (shard_id as u64) * range;
    let end = if shard_id + 1 == shard_cnt {
        u64::MAX
    } else {
        (shard_id as u64 + 1) * range
    };
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_shard_basic() {
        let values = b"test_value";
        assert_eq!(compute_shard(values, 1), 0);
        assert!((compute_shard(values, 4) as usize) < 4);
        assert!((compute_shard(values, 16) as usize) < 16);
    }

    #[test]
    fn test_compute_shard_distribution() {
        let shard_cnt = 8;
        let mut counts = vec![0u64; shard_cnt];
        for i in 0..100_000 {
            let shard = compute_shard(&i.to_le_bytes(), shard_cnt);
            counts[shard] += 1;
        }
        // Each shard should get roughly 1/8 of the data (within 5%)
        let expected = 100_000 / shard_cnt;
        for &count in &counts {
            let diff = if count > expected { count - expected } else { expected - count };
            assert!(
                diff < expected / 5,
                "Shard distribution uneven: {} vs expected {}",
                count,
                expected
            );
        }
    }

    #[test]
    fn test_hash_boundary_coverage() {
        let shard_cnt = 4;
        let mut boundaries = Vec::new();
        for i in 0..shard_cnt {
            let (start, end) = calculate_hash_boundary(i, shard_cnt);
            boundaries.push((start, end));
            assert!(start < end, "Shard {} has invalid boundary", i);
            if i > 0 {
                assert_eq!(start, boundaries[i - 1].1, "Gap or overlap between shards");
            }
        }
        assert_eq!(boundaries[0].0, 0, "First shard should start at 0");
        assert_eq!(boundaries[shard_cnt - 1].1, u64::MAX, "Last shard should end at u64::MAX");
    }

    #[test]
    fn test_shard_key_info() {
        let info = ShardKeyInfo {
            columns: vec!["company_id".to_string()],
            shard_cnt: 4,
        };
        assert_eq!(info.columns.len(), 1);
        assert_eq!(info.shard_cnt, 4);
    }
}
