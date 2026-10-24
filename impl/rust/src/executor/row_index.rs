//! A membership index over rows its caller already owns (eager DISTINCT and GROUP BY). It stores
//! only each row's position, so a dedup set or group table holds one copy of a row instead of
//! two, and a probe reads the candidate values in place. Never iterated: output order comes from
//! the caller's insertion-ordered rows (no hashmap-order leak — CLAUDE.md §8/§10).
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};

use crate::value::Value;

enum Bucket {
    One(usize),
    Many(Vec<usize>),
}

#[derive(Default)]
pub(crate) struct RowIndex {
    hasher: RandomState,
    buckets: HashMap<u64, Bucket>,
}

impl RowIndex {
    /// The hash of a key's values, under `Value`'s own `Hash` (consistent with its `Eq`).
    pub(crate) fn hash<'a>(&self, key: impl ExactSizeIterator<Item = &'a Value>) -> u64 {
        let mut h = self.hasher.build_hasher();
        key.len().hash(&mut h);
        for v in key {
            v.hash(&mut h);
        }
        h.finish()
    }

    /// The position of the indexed row with hash `hash` for which `equal` holds.
    pub(crate) fn find(&self, hash: u64, mut equal: impl FnMut(usize) -> bool) -> Option<usize> {
        match self.buckets.get(&hash)? {
            Bucket::One(at) => equal(*at).then_some(*at),
            Bucket::Many(ats) => ats.iter().copied().find(|&at| equal(at)),
        }
    }

    /// Index the row at position `at` under `hash`; the caller has checked it is absent.
    pub(crate) fn insert(&mut self, hash: u64, at: usize) {
        match self.buckets.get_mut(&hash) {
            None => {
                self.buckets.insert(hash, Bucket::One(at));
            }
            Some(Bucket::One(first)) => {
                let first = *first;
                self.buckets.insert(hash, Bucket::Many(vec![first, at]));
            }
            Some(Bucket::Many(ats)) => ats.push(at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct rows sharing one hash stay distinct, and each probe finds its own row.
    #[test]
    fn colliding_hashes_keep_rows_apart() {
        let rows = [
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
        ];
        let mut index = RowIndex::default();
        for at in 0..rows.len() {
            assert_eq!(index.find(7, |i| rows[i] == rows[at]), None);
            index.insert(7, at);
        }
        for (at, row) in rows.iter().enumerate() {
            assert_eq!(index.find(7, |i| rows[i] == *row), Some(at));
        }
        assert_eq!(index.find(7, |i| rows[i] == vec![Value::Int(4)]), None);
        assert_eq!(index.find(8, |_| true), None);
    }

    // The hash follows `Value`'s equality: equal keys hash alike.
    #[test]
    fn equal_keys_hash_alike() {
        let index = RowIndex::default();
        let a = [Value::Text("x".into()), Value::Null];
        let b = [Value::Text("x".into()), Value::Null];
        assert_eq!(index.hash(a.iter()), index.hash(b.iter()));
    }
}
