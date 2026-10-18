//! GIN posting runs (spec/design/gin.md §4.5, `format_version` 47).
//!
//! A GIN entry tree stores each term's posting list as **runs**: one record key holds the term, the
//! run's last (largest) storage key, then the run's earlier storage keys front-coded, then the coded
//! length. The record keys of one term sort by their last storage key, so the run that holds — or
//! would hold — a storage key `k` is the first record at or after `term ‖ k`, and every term's runs
//! are the prefix range `[term, successor(term))` as single entries were. The run layout is part of
//! the byte contract: a from-scratch build packs each posting list greedily, and maintenance edits
//! one run at a time by the rules below, so the same statements produce the same records in every
//! core.

use crate::error::{EngineError, Result};
use crate::pmap::KeyBound;
use crate::storage::TableStore;

/// The longest run key maintenance builds by adding a storage key to a run (gin.md §4.5). A run of
/// one storage key may be longer — a long term or storage key alone.
pub(crate) const GIN_RUN_MAX_BYTES: usize = 64;

fn corrupt() -> EngineError {
    EngineError::data_corrupted("a GIN posting run is malformed")
}

fn push_varint(out: &mut Vec<u8>, mut v: usize) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Result<usize> {
    let mut v = 0usize;
    for shift in (0..).step_by(7).take(3) {
        let byte = *bytes.get(*at).ok_or_else(corrupt)?;
        *at += 1;
        v |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(corrupt())
}

/// The byte-successor of a prefix: the smallest byte string greater than every extension of `p`
/// (`None` for an all-`0xFF` prefix — an unbounded high end).
fn prefix_successor(p: &[u8]) -> Option<Vec<u8>> {
    let mut s = p.to_vec();
    while let Some(last) = s.pop() {
        if last != 0xFF {
            s.push(last + 1);
            return Some(s);
        }
    }
    None
}

fn shared_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// The run key of `term` over the ascending, distinct storage keys `keys` (non-empty):
/// `term ‖ last ‖ body ‖ u16 BE len(body)`, where the body front-codes every key but the last, in
/// ascending order, each as `varint(shared) ‖ varint(suffix length) ‖ suffix` against the previous
/// key (the first against the last).
pub(crate) fn encode_run(term: &[u8], keys: &[Vec<u8>]) -> Vec<u8> {
    let (last, rest) = keys
        .split_last()
        .expect("a posting run holds a storage key");
    let mut body = Vec::new();
    let mut prev: &[u8] = last;
    for key in rest {
        let shared = shared_prefix(prev, key);
        push_varint(&mut body, shared);
        push_varint(&mut body, key.len() - shared);
        body.extend_from_slice(&key[shared..]);
        prev = key;
    }
    let mut out = Vec::with_capacity(term.len() + last.len() + body.len() + 2);
    out.extend_from_slice(term);
    out.extend_from_slice(last);
    out.extend_from_slice(&body);
    out.extend_from_slice(
        &u16::try_from(body.len())
            .expect("a run body fits u16 (runs grow only to GIN_RUN_MAX_BYTES)")
            .to_be_bytes(),
    );
    out
}

/// The ascending storage keys of a run key whose term is `term_len` bytes long. A malformed run is
/// `XX001`.
pub(crate) fn decode_run(term_len: usize, run: &[u8]) -> Result<Vec<Vec<u8>>> {
    let n = run.len();
    if n < term_len + 3 {
        return Err(corrupt());
    }
    let body_len = usize::from(u16::from_be_bytes([run[n - 2], run[n - 1]]));
    let body_end = n - 2;
    let last_end = body_end.checked_sub(body_len).ok_or_else(corrupt)?;
    if last_end <= term_len {
        return Err(corrupt());
    }
    let last = &run[term_len..last_end];
    let body = &run[last_end..body_end];
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let shared = read_varint(body, &mut at)?;
        let suffix = read_varint(body, &mut at)?;
        let prev: &[u8] = keys.last().map_or(last, Vec::as_slice);
        if shared > prev.len() || at + suffix > body.len() || shared + suffix == 0 {
            return Err(corrupt());
        }
        let mut key = prev[..shared].to_vec();
        key.extend_from_slice(&body[at..at + suffix]);
        at += suffix;
        if keys.last().is_some_and(|p| p.as_slice() >= key.as_slice()) {
            return Err(corrupt());
        }
        keys.push(key);
    }
    if keys.last().is_some_and(|p| p.as_slice() >= last) {
        return Err(corrupt());
    }
    keys.push(last.to_vec());
    Ok(keys)
}

/// The runs of `term` over ascending, distinct `keys`, packed greedily: a run takes each next key
/// while its key stays within [`GIN_RUN_MAX_BYTES`] (a from-scratch build, gin.md §4.5).
pub(crate) fn pack_runs(term: &[u8], keys: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut runs = Vec::new();
    let mut start = 0;
    while start < keys.len() {
        let mut end = start + 1;
        while end < keys.len() && encode_run(term, &keys[start..=end]).len() <= GIN_RUN_MAX_BYTES {
            end += 1;
        }
        runs.push(encode_run(term, &keys[start..end]));
        start = end;
    }
    runs
}

/// The runs of a run's key list after an edit (gin.md §4.5): one run when it fits, else the first
/// `ceil(n/2)` keys and the rest, each split again the same way until it fits or holds one key.
fn split_runs(term: &[u8], keys: &[Vec<u8>], out: &mut Vec<Vec<u8>>) {
    let run = encode_run(term, keys);
    if run.len() <= GIN_RUN_MAX_BYTES || keys.len() == 1 {
        out.push(run);
        return;
    }
    let mid = keys.len().div_ceil(2);
    split_runs(term, &keys[..mid], out);
    split_runs(term, &keys[mid..], out);
}

/// The posting range of `term`: every run key that starts with it.
fn term_bound(term: &[u8]) -> KeyBound {
    KeyBound {
        lo: Some(term.to_vec()),
        lo_inc: true,
        hi: prefix_successor(term),
        hi_inc: false,
    }
}

/// The first run key of `term` at or after `term ‖ key` — the run holding `key` if any holds it.
pub(crate) fn run_at_or_after(
    store: &TableStore,
    term: &[u8],
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    let mut bound = term_bound(term);
    bound.lo = Some([term, key].concat());
    let mut found = None;
    store.scan_range(&bound, &mut |k, _| {
        found = Some(k.to_vec());
        Ok(false)
    })?;
    Ok(found)
}

/// The last run key of `term` before `term ‖ key`.
fn run_before(store: &TableStore, term: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut bound = term_bound(term);
    bound.hi = Some([term, key].concat());
    let mut found = None;
    store.scan_range_rev(&bound, &mut |k, _| {
        found = Some(k.to_vec());
        Ok(false)
    })?;
    Ok(found)
}

fn replace_run(store: &mut TableStore, old: &[u8], runs: Vec<Vec<u8>>) -> Result<()> {
    store.remove(old)?;
    for run in runs {
        store.insert(run, Vec::new())?;
    }
    Ok(())
}

/// Add the posting `(term, key)` (gin.md §4.5). The run at or after `term ‖ key` takes the key (it
/// holds a larger last key), split in halves when it outgrows [`GIN_RUN_MAX_BYTES`]. A key above every
/// run joins the term's last run when that run stays within the limit, else starts a new run.
/// `false` when the posting is already present.
pub(crate) fn gin_insert(store: &mut TableStore, term: &[u8], key: &[u8]) -> Result<bool> {
    if let Some(run) = run_at_or_after(store, term, key)? {
        let mut keys = decode_run(term.len(), &run)?;
        let at = match keys.binary_search_by(|k| k.as_slice().cmp(key)) {
            Ok(_) => return Ok(false),
            Err(at) => at,
        };
        keys.insert(at, key.to_vec());
        let mut runs = Vec::new();
        split_runs(term, &keys, &mut runs);
        replace_run(store, &run, runs)?;
        return Ok(true);
    }
    if let Some(run) = run_before(store, term, key)? {
        let mut keys = decode_run(term.len(), &run)?;
        keys.push(key.to_vec());
        let grown = encode_run(term, &keys);
        if grown.len() <= GIN_RUN_MAX_BYTES {
            replace_run(store, &run, vec![grown])?;
            return Ok(true);
        }
    }
    store.insert(encode_run(term, &[key.to_vec()]), Vec::new())
}

/// Remove the posting `(term, key)` (gin.md §4.5): its run loses the key, and an emptied run is
/// removed. `false` when the posting is absent.
pub(crate) fn gin_remove(store: &mut TableStore, term: &[u8], key: &[u8]) -> Result<bool> {
    let Some(run) = run_at_or_after(store, term, key)? else {
        return Ok(false);
    };
    let mut keys = decode_run(term.len(), &run)?;
    let Ok(at) = keys.binary_search_by(|k| k.as_slice().cmp(key)) else {
        return Ok(false);
    };
    keys.remove(at);
    let mut runs = Vec::new();
    if !keys.is_empty() {
        split_runs(term, &keys, &mut runs);
    }
    replace_run(store, &run, runs)?;
    Ok(true)
}
