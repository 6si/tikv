// Copyright 2024 TiKV Project Authors. Licensed under Apache-2.0.

//! Shard-key support for MPP co-location optimization.
//!
//! When TiDB creates a table with `SHARD BY (col) SHARDS N`, it allocates N
//! physical table IDs and routes rows via CRC32/IEEE hashing. TiKV uses this
//! module to verify co-location when pushing joins to the coprocessor: if shard
//! slot S of table A and shard slot S of table B are both local, the join can
//! run without cross-node data movement.
//!
//! # Encoding contract (must match TiDB exactly)
//!
//! TiDB computes the shard slot in `pkg/table/tables/shard.go:locateShard()`:
//!
//! ```text
//! h := crc32.NewIEEE()
//! for each shard-key column:
//!     if value is NULL:
//!         h.Write([]byte{0x00})
//!     else:
//!         h.Write(value.ToHashKey())
//! slot = h.Sum32() % shardCnt
//! ```
//!
//! `Datum.ToHashKey()` calls `collate.GetCollator(d.Collation()).Key(d.ToString())`.
//! For the binary collator (default for integer/string columns with binary
//! collation), `Key(s)` returns `[]byte(s)` — i.e., the UTF-8 bytes of the
//! decimal string representation for integers, or the raw string bytes for
//! string types.
//!
//! **Examples:**
//! - Integer `42` → `ToString()` = `"42"` → bytes `[0x34, 0x32]`
//! - String `"hello"` → bytes `[0x68, 0x65, 0x6c, 0x6c, 0x6f]`
//! - NULL → bytes `[0x00]`
//! - Multi-column `(42, "ab")` → CRC32 fed `[0x34, 0x32]` then `[0x61, 0x62]`
//!
//! The caller is responsible for encoding column values to bytes using these
//! rules before calling [`shard_slot`]. TiKV's coprocessor must reproduce the
//! exact same byte sequence that TiDB would feed to CRC32 — otherwise the
//! computed shard slot will differ and co-located joins will silently produce
//! wrong results.

/// ShardKeyInfo describes the shard key for MPP co-location optimization.
/// Tables with matching shard keys can skip exchange during joins.
#[derive(Clone, Debug, PartialEq)]
pub struct ShardKeyInfo {
    /// Column names forming the shard key.
    pub columns: Vec<String>,
    /// Number of shards (must be >= 2 and <= 64 per TiDB DDL validation).
    pub shard_cnt: u32,
}

/// Compute which shard slot a row belongs to.
///
/// Uses CRC32/IEEE (the polynomial used by Go's `crc32.NewIEEE()` and the
/// `crc32fast` crate's default `Hasher`).
///
/// `key_bytes` is the concatenation of each shard-key column's encoded bytes,
/// in column order, following the encoding contract documented at module level.
/// `shard_cnt` is the total number of shards for the table.
///
/// Returns the shard slot index in `[0, shard_cnt)`.
///
/// # Panics
///
/// Panics in debug mode if `shard_cnt` is 0.
pub fn shard_slot(key_bytes: &[u8], shard_cnt: u32) -> u32 {
    debug_assert!(shard_cnt > 0, "shard_cnt must be positive");
    let mut h = crc32fast::Hasher::new();
    h.update(key_bytes);
    h.finalize() % shard_cnt
}

/// Convenience: compute shard slot by feeding multiple column byte slices
/// sequentially into a single CRC32 hasher, matching TiDB's multi-column
/// `locateShard` which calls `h.Write(data)` for each column in order.
///
/// This is equivalent to `shard_slot(&columns.concat(), shard_cnt)` but avoids
/// the allocation.
pub fn shard_slot_multi(columns: &[&[u8]], shard_cnt: u32) -> u32 {
    debug_assert!(shard_cnt > 0, "shard_cnt must be positive");
    let mut h = crc32fast::Hasher::new();
    for col in columns {
        h.update(col);
    }
    h.finalize() % shard_cnt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shard_slot_basic() {
        // shard_cnt=1 always returns 0.
        assert_eq!(shard_slot(b"anything", 1), 0);
        // Result is always in [0, shard_cnt).
        for cnt in [2, 4, 8, 16, 32, 64] {
            let slot = shard_slot(b"test_value", cnt);
            assert!(slot < cnt, "slot {} not in [0, {})", slot, cnt);
        }
    }

    #[test]
    fn test_shard_slot_distribution() {
        let shard_cnt: u32 = 8;
        let mut counts = vec![0u64; shard_cnt as usize];
        // Simulate integer shard keys 0..100_000 encoded as decimal strings
        // (matching TiDB's Datum.ToString() for integers).
        for i in 0..100_000u64 {
            let s = i.to_string();
            let slot = shard_slot(s.as_bytes(), shard_cnt);
            counts[slot as usize] += 1;
        }
        let expected = 100_000 / shard_cnt as u64;
        for (idx, &count) in counts.iter().enumerate() {
            let diff = count.abs_diff(expected);
            assert!(
                diff < expected / 5,
                "Shard {} distribution uneven: {} vs expected {} (diff {})",
                idx,
                count,
                expected,
                diff,
            );
        }
    }

    #[test]
    fn test_shard_slot_multi_matches_concat() {
        // Multi-column shard slot must match concatenated single-call.
        let col1 = b"42";
        let col2 = b"hello";
        let concat: Vec<u8> = [col1.as_slice(), col2.as_slice()].concat();
        let slot_single = shard_slot(&concat, 16);
        let slot_multi = shard_slot_multi(&[col1, col2], 16);
        assert_eq!(slot_single, slot_multi);
    }

    #[test]
    fn test_null_encoding() {
        // NULL columns are encoded as a single 0x00 byte on the TiDB side.
        let null_byte: &[u8] = &[0x00];
        let slot = shard_slot(null_byte, 4);
        assert!(slot < 4);
    }

    // Cross-check test vectors against TiDB's locateShard().
    //
    // TiDB encoding for int64 values:
    //   Datum.ToString() → string, then binCollator.Key(s) → []byte(s)
    //   e.g. int64(42) → "42" → [0x34, 0x32]
    //
    // Verify in Go playground (https://go.dev/play/):
    //   package main
    //   import ("fmt"; "hash/crc32")
    //   func main() {
    //       for _, tc := range []struct{ s string; cnt uint32 }{
    //           {"42", 4}, {"99", 4}, {"1", 4}, {"42", 16},
    //       } {
    //           h := crc32.NewIEEE()
    //           h.Write([]byte(tc.s))
    //           fmt.Printf("CRC32(%q) = 0x%08X, %%%d = %d\n",
    //               tc.s, h.Sum32(), tc.cnt, h.Sum32()%tc.cnt)
    //       }
    //   }
    #[test]
    fn test_crc32_ieee_cross_check() {
        // CRC32/IEEE of b"42" = 0x3224B088 = 841_265_288
        assert_eq!(crc32fast::hash(b"42"), 0x3224_B088);
        assert_eq!(shard_slot(b"42", 4), 0);  // 841_265_288 % 4 = 0
        assert_eq!(shard_slot(b"42", 16), 8); // 841_265_288 % 16 = 8

        // CRC32/IEEE of b"99" = 0x1058174D = 274_208_589
        assert_eq!(crc32fast::hash(b"99"), 0x1058_174D);
        assert_eq!(shard_slot(b"99", 4), 1); // 274_208_589 % 4 = 1

        // CRC32/IEEE of b"1" = 0x83DCEFB7 = 2_212_294_583
        assert_eq!(crc32fast::hash(b"1"), 0x83DC_EFB7);
        assert_eq!(shard_slot(b"1", 4), 3); // 2_212_294_583 % 4 = 3

        // CRC32/IEEE of b"hello" = 0x3610A686 = 907_060_870
        assert_eq!(crc32fast::hash(b"hello"), 0x3610_A686);
        assert_eq!(shard_slot(b"hello", 4), 2); // 907_060_870 % 4 = 2
    }

    #[test]
    fn test_multi_column_cross_check() {
        // Multi-column: CRC32/IEEE fed b"42" then b"hello" sequentially.
        // CRC32/IEEE("42"+"hello") = 0x78C0A902 = 2_025_892_098
        let mut h = crc32fast::Hasher::new();
        h.update(b"42");
        h.update(b"hello");
        assert_eq!(h.finalize(), 0x78C0_A902);
        assert_eq!(shard_slot_multi(&[b"42", b"hello"], 4), 2); // 2_025_892_098 % 4 = 2
    }

    #[test]
    fn test_null_cross_check() {
        // NULL → single 0x00 byte. CRC32/IEEE(0x00) = 0xD202EF8D = 3_523_407_757
        assert_eq!(crc32fast::hash(&[0x00]), 0xD202_EF8D);
        assert_eq!(shard_slot(&[0x00], 4), 1); // 3_523_407_757 % 4 = 1
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
