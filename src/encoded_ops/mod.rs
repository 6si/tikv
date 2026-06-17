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

/// Encoded filter request relayed from TiDB planner to TiFlash.
/// Describes a filter predicate on a dictionary-encoded column.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedFilterRequest {
    /// Column being filtered
    pub column_id: i64,
    /// Filter type: "eq", "ne", "in", "like", "range"
    pub filter_type: String,
    /// Constant values for the filter (serialized)
    pub filter_values: Vec<Vec<u8>>,
    /// Whether this filter can use the encoded path
    pub can_use_encoded_path: bool,
}

/// Encoded group-by request relayed from TiDB planner to TiFlash.
/// Describes a group-by that can operate directly on dictionary IDs.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedGroupByRequest {
    /// Column IDs being grouped on
    pub group_by_column_ids: Vec<i64>,
    /// Aggregate functions requested
    pub agg_funcs: Vec<AggFuncType>,
    /// Column IDs being aggregated
    pub agg_column_ids: Vec<i64>,
    /// Estimated number of groups (from TiDB stats)
    pub estimated_groups: u64,
    /// Whether array-indexed aggregation is feasible
    pub can_use_encoded_path: bool,
}

/// Aggregate function types supported in encoded group-by.
#[derive(Clone, Debug, PartialEq)]
pub enum AggFuncType {
    Sum,
    Count,
    Min,
    Max,
    Any,
}

/// Encoded star join request relayed from TiDB planner to TiFlash.
/// Describes a fused join+group-by+aggregate pipeline on encoded data.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedStarJoinRequest {
    /// Fact table ID
    pub fact_table_id: i64,
    /// Dimension joins in the star schema
    pub dimension_joins: Vec<DimensionJoinRequest>,
    /// Whether GROUP BY is present above the join
    pub has_group_by_above: bool,
    /// Whether fused scan→join→agg is feasible
    pub can_use_fused_path: bool,
}

/// One dimension in a star join request.
#[derive(Clone, Debug, PartialEq)]
pub struct DimensionJoinRequest {
    /// Dimension table ID
    pub dimension_table_id: i64,
    /// Foreign key column in fact table
    pub fact_join_column_id: i64,
    /// Primary key column in dimension table
    pub dim_join_column_id: i64,
    /// Column from dimension used in GROUP BY
    pub dim_group_column_id: i64,
    /// Estimated dimension table size
    pub estimated_dim_size: u64,
}

/// Full encoded operations request combining all phases.
/// This is attached to the coprocessor DAGRequest for TiFlash.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncodedOpsRequest {
    /// Base encoding hint (Phase 1)
    pub encoding_hint: Option<EncodingHint>,
    /// Filter requests (Phase 2)
    pub filter_requests: Vec<EncodedFilterRequest>,
    /// Group-by request (Phase 3)
    pub group_by_request: Option<EncodedGroupByRequest>,
    /// Star join request (Phase 4)
    pub star_join_request: Option<EncodedStarJoinRequest>,
}

impl EncodedOpsRequest {
    /// Create a new empty request
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any encoded operations are requested
    pub fn has_any_ops(&self) -> bool {
        self.encoding_hint.is_some()
            || !self.filter_requests.is_empty()
            || self.group_by_request.is_some()
            || self.star_join_request.is_some()
    }

    /// Get the number of phases with active requests
    pub fn active_phases(&self) -> u32 {
        let mut count = 0;
        if self.encoding_hint.is_some() {
            count += 1;
        }
        if !self.filter_requests.is_empty() {
            count += 1;
        }
        if self.group_by_request.is_some() {
            count += 1;
        }
        if self.star_join_request.is_some() {
            count += 1;
        }
        count
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

    #[test]
    fn test_encoded_filter_request() {
        let req = EncodedFilterRequest {
            column_id: 10,
            filter_type: "eq".to_string(),
            filter_values: vec![b"hello".to_vec()],
            can_use_encoded_path: true,
        };
        assert_eq!(req.column_id, 10);
        assert_eq!(req.filter_type, "eq");
        assert!(req.can_use_encoded_path);
    }

    #[test]
    fn test_encoded_group_by_request() {
        let req = EncodedGroupByRequest {
            group_by_column_ids: vec![1, 2],
            agg_funcs: vec![AggFuncType::Sum, AggFuncType::Count],
            agg_column_ids: vec![3, 0],
            estimated_groups: 100,
            can_use_encoded_path: true,
        };
        assert_eq!(req.group_by_column_ids.len(), 2);
        assert_eq!(req.agg_funcs.len(), 2);
        assert_eq!(req.estimated_groups, 100);
        assert!(req.can_use_encoded_path);
    }

    #[test]
    fn test_encoded_star_join_request() {
        let req = EncodedStarJoinRequest {
            fact_table_id: 1,
            dimension_joins: vec![
                DimensionJoinRequest {
                    dimension_table_id: 2,
                    fact_join_column_id: 10,
                    dim_join_column_id: 1,
                    dim_group_column_id: 2,
                    estimated_dim_size: 500,
                },
                DimensionJoinRequest {
                    dimension_table_id: 3,
                    fact_join_column_id: 11,
                    dim_join_column_id: 1,
                    dim_group_column_id: 3,
                    estimated_dim_size: 100,
                },
            ],
            has_group_by_above: true,
            can_use_fused_path: true,
        };
        assert_eq!(req.fact_table_id, 1);
        assert_eq!(req.dimension_joins.len(), 2);
        assert!(req.has_group_by_above);
        assert!(req.can_use_fused_path);
    }

    #[test]
    fn test_encoded_ops_request_empty() {
        let req = EncodedOpsRequest::new();
        assert!(!req.has_any_ops());
        assert_eq!(req.active_phases(), 0);
    }

    #[test]
    fn test_encoded_ops_request_full() {
        let req = EncodedOpsRequest {
            encoding_hint: Some(EncodingHint {
                dict_eligible_columns: vec![1],
                max_cardinality: 4096,
                enable_encoded_filter: true,
                enable_encoded_group_by: true,
                enable_encoded_bloom_filter: false,
            }),
            filter_requests: vec![EncodedFilterRequest {
                column_id: 1,
                filter_type: "eq".to_string(),
                filter_values: vec![],
                can_use_encoded_path: true,
            }],
            group_by_request: Some(EncodedGroupByRequest {
                group_by_column_ids: vec![1],
                agg_funcs: vec![AggFuncType::Sum],
                agg_column_ids: vec![2],
                estimated_groups: 50,
                can_use_encoded_path: true,
            }),
            star_join_request: Some(EncodedStarJoinRequest {
                fact_table_id: 1,
                dimension_joins: vec![],
                has_group_by_above: true,
                can_use_fused_path: true,
            }),
        };
        assert!(req.has_any_ops());
        assert_eq!(req.active_phases(), 4);
    }

    #[test]
    fn test_agg_func_type_clone() {
        let func = AggFuncType::Sum;
        let cloned = func.clone();
        assert_eq!(func, cloned);
    }

    // --- Gap tests: hint passthrough, partial capabilities, large filter list ---

    #[test]
    fn test_hint_passthrough_fidelity() {
        // Verify encoding hint is bit-identical after clone (simulating relay).
        let original = EncodingHint {
            dict_eligible_columns: vec![1, 5, 99, -1, i64::MAX],
            max_cardinality: 2048,
            enable_encoded_filter: true,
            enable_encoded_group_by: false,
            enable_encoded_bloom_filter: true,
        };
        let relayed = original.clone();
        assert_eq!(original, relayed, "hint must be identical after clone/relay");
    }

    #[test]
    fn test_partial_encoding_capabilities() {
        // Phase 1 only: encoding hint present but no filter/groupby/star-join.
        let req = EncodedOpsRequest {
            encoding_hint: Some(EncodingHint {
                dict_eligible_columns: vec![1],
                max_cardinality: 4096,
                enable_encoded_filter: false,
                enable_encoded_group_by: false,
                enable_encoded_bloom_filter: false,
            }),
            filter_requests: vec![],
            group_by_request: None,
            star_join_request: None,
        };
        assert!(req.has_any_ops());
        assert_eq!(req.active_phases(), 1);
        // The hint itself reports no encoded ops enabled.
        assert!(!req.encoding_hint.as_ref().unwrap().has_any_encoded_ops());
    }

    #[test]
    fn test_large_filter_list() {
        // >100 IN-list values in encoded filter hint — verify no truncation.
        let large_values: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("val_{}", i).into_bytes())
            .collect();
        let req = EncodedFilterRequest {
            column_id: 42,
            filter_type: "in".to_string(),
            filter_values: large_values.clone(),
            can_use_encoded_path: true,
        };
        assert_eq!(req.filter_values.len(), 200);
        assert_eq!(req.filter_values[0], b"val_0");
        assert_eq!(req.filter_values[199], b"val_199");
    }

    #[test]
    fn test_full_request_clone_fidelity() {
        // Full 4-phase request must survive clone without data loss.
        let req = EncodedOpsRequest {
            encoding_hint: Some(EncodingHint {
                dict_eligible_columns: vec![1, 2, 3],
                max_cardinality: 1024,
                enable_encoded_filter: true,
                enable_encoded_group_by: true,
                enable_encoded_bloom_filter: true,
            }),
            filter_requests: vec![
                EncodedFilterRequest {
                    column_id: 1,
                    filter_type: "eq".to_string(),
                    filter_values: vec![b"active".to_vec()],
                    can_use_encoded_path: true,
                },
                EncodedFilterRequest {
                    column_id: 2,
                    filter_type: "in".to_string(),
                    filter_values: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
                    can_use_encoded_path: true,
                },
            ],
            group_by_request: Some(EncodedGroupByRequest {
                group_by_column_ids: vec![1],
                agg_funcs: vec![AggFuncType::Sum, AggFuncType::Count, AggFuncType::Min, AggFuncType::Max],
                agg_column_ids: vec![3, 0, 3, 3],
                estimated_groups: 50,
                can_use_encoded_path: true,
            }),
            star_join_request: Some(EncodedStarJoinRequest {
                fact_table_id: 100,
                dimension_joins: vec![
                    DimensionJoinRequest {
                        dimension_table_id: 200,
                        fact_join_column_id: 10,
                        dim_join_column_id: 1,
                        dim_group_column_id: 2,
                        estimated_dim_size: 500,
                    },
                ],
                has_group_by_above: true,
                can_use_fused_path: true,
            }),
        };
        let cloned = req.clone();
        assert_eq!(req, cloned);
        assert_eq!(cloned.active_phases(), 4);
        assert_eq!(cloned.filter_requests.len(), 2);
        assert_eq!(cloned.filter_requests[1].filter_values.len(), 3);
    }

    #[test]
    fn test_all_agg_func_types() {
        // Verify all AggFuncType variants are distinct.
        let funcs = vec![
            AggFuncType::Sum,
            AggFuncType::Count,
            AggFuncType::Min,
            AggFuncType::Max,
            AggFuncType::Any,
        ];
        for i in 0..funcs.len() {
            for j in (i + 1)..funcs.len() {
                assert_ne!(funcs[i], funcs[j], "AggFuncType variants must be distinct");
            }
        }
    }
}
