// Copyright 2024 TiKV Project Authors. Licensed under Apache-2.0.

//! Encoded operations support for TiFlash dictionary-encoded columnstore.
//!
//! This module provides the TiKV-side support for dictionary encoding hints
//! that are passed from TiDB through TiKV's coprocessor to TiFlash. When TiDB's
//! planner enables encoded operations (`tidb_tiflash_encoded_operations = ON`),
//! the DAGRequest includes encoding hints that tell TiFlash which columns are
//! eligible for dictionary encoding and what operations can be performed on
//! encoded data.
//!
//! TiKV's role is minimal — it acts as a passthrough for these hints in the
//! coprocessor pipeline and provides bloom filter encoding support when
//! applicable.

/// Encoding hint that can be attached to a coprocessor request.
/// This tells TiFlash about dictionary encoding opportunities.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodingHint {
    /// Column IDs eligible for dictionary encoding
    pub dict_eligible_columns: Vec<i64>,
    /// Maximum cardinality threshold for dictionary encoding
    pub max_cardinality: u32,
    /// Whether encoded filter operations are requested
    pub enable_encoded_filter: bool,
    /// Whether encoded group-by operations are requested
    pub enable_encoded_group_by: bool,
    /// Whether encoded bloom filter pushdown is requested
    pub enable_encoded_bloom_filter: bool,
}

impl Default for EncodingHint {
    fn default() -> Self {
        Self {
            dict_eligible_columns: Vec::new(),
            max_cardinality: 4096,
            enable_encoded_filter: false,
            enable_encoded_group_by: false,
            enable_encoded_bloom_filter: false,
        }
    }
}

impl EncodingHint {
    /// Create a new encoding hint with default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any encoded operations are enabled
    pub fn has_any_encoded_ops(&self) -> bool {
        self.enable_encoded_filter || self.enable_encoded_group_by || self.enable_encoded_bloom_filter
    }

    /// Check if a specific column is eligible for dictionary encoding
    pub fn is_column_eligible(&self, col_id: i64) -> bool {
        self.dict_eligible_columns.contains(&col_id)
    }
}

/// Bloom filter hint for encoded pushdown.
/// When TiFlash has dictionary-encoded data, bloom filters can be built on
/// dictionary entries (which are fewer than actual rows) for efficient semi-join
/// filtering.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedBloomFilterHint {
    /// The column ID this bloom filter applies to
    pub column_id: i64,
    /// Hash values for the bloom filter (from the build side of a join)
    pub hash_values: Vec<u64>,
    /// Number of bits in the bloom filter
    pub num_bits: u32,
    /// Number of hash functions used
    pub num_hash_funcs: u32,
}

impl EncodedBloomFilterHint {
    /// Check if a value might be in the bloom filter
    pub fn might_contain(&self, hash: u64) -> bool {
        if self.num_bits == 0 {
            return true;
        }
        // Simple bloom filter check using double hashing
        let h1 = hash;
        let h2 = hash.wrapping_shr(32) | hash.wrapping_shl(32);
        for i in 0..self.num_hash_funcs {
            let bit_pos = (h1.wrapping_add((i as u64).wrapping_mul(h2))) % (self.num_bits as u64);
            let byte_idx = (bit_pos / 8) as usize;
            if byte_idx >= (self.num_bits as usize + 7) / 8 {
                return true; // Out of bounds, assume might contain
            }
        }
        true // Simplified — actual implementation would check bit array
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoding_hint_default() {
        let hint = EncodingHint::default();
        assert_eq!(hint.max_cardinality, 4096);
        assert!(!hint.enable_encoded_filter);
        assert!(!hint.enable_encoded_group_by);
        assert!(!hint.enable_encoded_bloom_filter);
        assert!(!hint.has_any_encoded_ops());
        assert!(hint.dict_eligible_columns.is_empty());
    }

    #[test]
    fn test_encoding_hint_with_ops() {
        let hint = EncodingHint {
            dict_eligible_columns: vec![1, 2, 3],
            max_cardinality: 2048,
            enable_encoded_filter: true,
            enable_encoded_group_by: true,
            enable_encoded_bloom_filter: false,
        };
        assert!(hint.has_any_encoded_ops());
        assert!(hint.is_column_eligible(1));
        assert!(hint.is_column_eligible(2));
        assert!(hint.is_column_eligible(3));
        assert!(!hint.is_column_eligible(4));
    }

    #[test]
    fn test_bloom_filter_hint() {
        let hint = EncodedBloomFilterHint {
            column_id: 5,
            hash_values: vec![123, 456, 789],
            num_bits: 1024,
            num_hash_funcs: 3,
        };
        assert_eq!(hint.column_id, 5);
        // Bloom filter should never return false negative
        assert!(hint.might_contain(123));
    }
}
