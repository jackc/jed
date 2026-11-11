//! The writing transaction's **compression memo** — each large value it writes is LZ4-compressed
//! once rather than once per disposition plan.
//!
//! A written record is planned three times ([`crate::format`]'s `plan_dispositions`): for its
//! `value_compress` charge at the statement's write site, for its B-tree weight when the store
//! takes it, and for its encoding when the commit writes the leaf. Every plan attempts the same
//! compressions (large-values.md §13), so without a memo a 1 MiB value is compressed three times,
//! most of a large INSERT's CPU.
//!
//! The memo remembers each attempt's LZ4 block, adopted or not, keyed by a hash of the content
//! payload `P(v)` the attempt compressed. The block depends on those bytes alone (the encoder is
//! pinned, lz4.md §2), and a hit is **verified exactly** — the remembered block must decode to the
//! payload being planned ([`crate::lz4::decodes_to`]) — so a plan stays a pure function of the
//! row's logical values (large-values.md §15) and results, plans, costs, and bytes are unchanged
//! whatever the memo holds. A hash collision is only a failed verification: the attempt then
//! compresses, as without the memo, and replaces the entry.
//!
//! Ownership follows the writer: an open writable transaction owns one
//! (`ActiveTx::compress_memo`), which the executor passes to the write-site plan explicitly and
//! lends to the stores it writes as a [`Weak`] handle, so the B-tree weight and the commit encoder
//! find it there. A store's handle never keeps the memo alive: it dies with the transaction — on
//! ROLLBACK or a failed commit when the transaction drops, and on COMMIT once its pages are written
//! (a shared core's publish persists main after `commit_tx`, so the engine holds the memo until
//! then). A published snapshot is left only dead handles.
//!
//! Kept blocks are bounded by [`MEMO_MAX_BYTES`]: past it, attempts compress without being
//! remembered. The memo is a cache, not query memory — it is never charged to the query-memory
//! account, and filling or skipping it changes nothing observable.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The most block bytes one transaction's memo keeps (plus [`ENTRY_OVERHEAD`] per entry). An
/// incompressible 1 MiB value keeps a block of about 1 MiB, so this covers 64 such rows written
/// in one transaction; a compressible value's block is a small fraction of its payload.
pub(crate) const MEMO_MAX_BYTES: usize = 64 << 20;

/// The bookkeeping an entry is counted at beyond its block (map slot + `Arc` header).
const ENTRY_OVERHEAD: usize = 64;

/// See the module docs.
#[derive(Default)]
pub(crate) struct CompressMemo {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Payload hash → the block an attempt on that payload produced.
    entries: HashMap<u64, Arc<Vec<u8>>>,
    /// Bytes counted against [`MEMO_MAX_BYTES`].
    bytes: usize,
}

impl CompressMemo {
    /// The LZ4 block of `payload` ([`crate::lz4::compress`]): the remembered one when an earlier
    /// plan of this transaction compressed the same bytes, else freshly compressed (and remembered
    /// while the memo has room).
    pub(crate) fn compress(&self, payload: &[u8]) -> Arc<Vec<u8>> {
        let h = payload_hash(payload);
        let hit = self.lock().entries.get(&h).cloned();
        if let Some(block) = hit
            && crate::lz4::decodes_to(&block, payload)
        {
            return block;
        }
        let block = Arc::new(crate::lz4::compress(payload));
        let mut inner = self.lock();
        let add = block.len() + ENTRY_OVERHEAD;
        let old = inner
            .entries
            .get(&h)
            .map_or(0, |b| b.len() + ENTRY_OVERHEAD);
        if inner.bytes - old + add <= MEMO_MAX_BYTES {
            inner.bytes = inner.bytes - old + add;
            inner.entries.insert(h, block.clone());
        }
        block
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // The memo is only a cache of pure results; a panic mid-update leaves it consistent.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The bytes the memo counts against its cap (tests).
    #[cfg(test)]
    pub(crate) fn kept_bytes(&self) -> usize {
        self.lock().bytes
    }
}

/// A fast 64-bit hash of `b` — four independent multiply-rotate lanes over 32-byte strides, then a
/// splitmix64 finish. It only picks the entry to verify; exactness comes from the verification.
fn payload_hash(b: &[u8]) -> u64 {
    const K: [u64; 4] = [
        0x9E37_79B9_7F4A_7C15,
        0xC2B2_AE3D_27D4_EB4F,
        0x1656_67B1_9E37_79F9,
        0x27D4_EB2F_1656_67C5,
    ];
    let word = |c: &[u8]| u64::from_le_bytes(c.try_into().expect("8 bytes"));
    let mut h = K;
    let mut strides = b.chunks_exact(32);
    for s in &mut strides {
        for l in 0..4 {
            h[l] = (h[l] ^ word(&s[l * 8..l * 8 + 8]))
                .wrapping_mul(K[l])
                .rotate_left(29);
        }
    }
    let rest = strides.remainder();
    let mut tail = [0u8; 32];
    tail[..rest.len()].copy_from_slice(rest);
    for l in 0..4 {
        h[l] = (h[l] ^ word(&tail[l * 8..l * 8 + 8]))
            .wrapping_mul(K[l])
            .rotate_left(29);
    }
    let mut x = (b.len() as u64)
        ^ h[0]
        ^ h[1].rotate_left(16)
        ^ h[2].rotate_left(32)
        ^ h[3].rotate_left(48);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hit_returns_the_same_block_a_fresh_compression_would() {
        let memo = CompressMemo::default();
        let a = b"abcdefgh".repeat(4096);
        let first = memo.compress(&a);
        assert_eq!(*first, crate::lz4::compress(&a));
        let again = memo.compress(&a);
        assert!(Arc::ptr_eq(&first, &again), "the second attempt is a hit");
        assert_eq!(memo.kept_bytes(), first.len() + ENTRY_OVERHEAD);
    }

    #[test]
    fn a_colliding_entry_fails_verification_and_is_replaced() {
        let memo = CompressMemo::default();
        let a = b"abcdefgh".repeat(4096);
        let b = b"zyxwvuts".repeat(4096);
        // Plant a's block under b's hash: the lookup finds it, verification refuses it.
        let planted = Arc::new(crate::lz4::compress(&a));
        {
            let mut inner = memo.lock();
            inner.bytes = planted.len() + ENTRY_OVERHEAD;
            inner.entries.insert(payload_hash(&b), planted.clone());
        }
        let got = memo.compress(&b);
        assert_eq!(*got, crate::lz4::compress(&b));
        assert!(!Arc::ptr_eq(&got, &planted));
        assert_eq!(memo.kept_bytes(), got.len() + ENTRY_OVERHEAD);
        assert!(Arc::ptr_eq(&memo.compress(&b), &got));
    }

    #[test]
    fn past_the_cap_attempts_compress_without_being_kept() {
        let memo = CompressMemo::default();
        memo.lock().bytes = MEMO_MAX_BYTES - 10;
        let a = b"abcdefgh".repeat(4096);
        let first = memo.compress(&a);
        let again = memo.compress(&a);
        assert_eq!(first, again);
        assert!(!Arc::ptr_eq(&first, &again), "nothing was kept");
        assert_eq!(memo.kept_bytes(), MEMO_MAX_BYTES - 10);
    }

    /// The memo belongs to the writing transaction: a bare engine's commit and rollback leave
    /// nothing holding it, and the published stores only dead handles (the shared core's publish:
    /// `shared::compress_memo_tests`).
    #[test]
    fn the_memo_lives_only_as_long_as_its_writing_transaction() {
        use crate::executor::Engine;
        let large = "INSERT INTO t SELECT g, repeat(CAST(g AS text) || 'ab', 50000) \
                     FROM generate_series(1, 3) g";
        let mut db = Engine::new();
        crate::execute(&mut db, "CREATE TABLE t (id bigint PRIMARY KEY, body text)").unwrap();
        crate::execute(&mut db, "BEGIN").unwrap();
        crate::execute(&mut db, large).unwrap();
        assert!(
            db.compress_memo()
                .expect("a writable block's memo")
                .kept_bytes()
                > 0
        );
        let lent = db
            .session
            .tx
            .as_ref()
            .unwrap()
            .working
            .store("t")
            .compress_memo();
        let lent = Arc::downgrade(&lent.expect("the written store holds a handle"));
        crate::execute(&mut db, "COMMIT").unwrap();
        assert!(lent.upgrade().is_none(), "COMMIT drops the memo");
        assert!(db.commit_memo.is_none());
        assert!(db.committed.store("t").compress_memo().is_none());

        crate::execute(&mut db, "BEGIN").unwrap();
        crate::execute(&mut db, "DELETE FROM t").unwrap();
        crate::execute(&mut db, large).unwrap();
        let lent = db
            .session
            .tx
            .as_ref()
            .unwrap()
            .working
            .store("t")
            .compress_memo();
        let lent = Arc::downgrade(&lent.unwrap());
        crate::execute(&mut db, "ROLLBACK").unwrap();
        assert!(lent.upgrade().is_none(), "ROLLBACK drops the memo");
        assert!(db.compress_memo().is_none());
    }

    #[test]
    fn the_hash_sees_every_byte_and_the_length() {
        let base = vec![7u8; 1000];
        let h = payload_hash(&base);
        for i in [0, 31, 32, 500, 999] {
            let mut v = base.clone();
            v[i] ^= 1;
            assert_ne!(payload_hash(&v), h, "byte {i}");
        }
        assert_ne!(payload_hash(&base[..999]), h);
        assert_ne!(payload_hash(&[]), payload_hash(&[0]));
    }
}
