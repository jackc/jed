//! The pinned LZ4-block codec (spec/fileformat/lz4.md) — hand-rolled, deterministic, and
//! byte-identical across every core (a library is inadmissible: encoders diverge — CLAUDE.md §14;
//! spec/design/large-values.md §6). The encoder's free parameters are FIXED by lz4.md §2 (greedy
//! match search, step 1, a 4096-entry single-candidate hash table, no backward extension); the
//! output is pinned by `spec/fileformat/lz4_vectors.toml` and the `compressed_table.jed` golden.
//! The decoder (lz4.md §3) is total and safe: every read is bounds-checked, the output never grows
//! past the expected length, and malformed input is a structured `data_corrupted` (CLAUDE.md §13).

use crate::error::{EngineError, Result, SqlState};

const MIN_MATCH: usize = 4;
const MAX_OFFSET: usize = 65_535;
/// No match may start after `len - MFLIMIT` (the block format's end constraint).
const MFLIMIT: usize = 12;
/// No match may extend past `len - LAST_LITERALS`.
const LAST_LITERALS: usize = 5;
const HASH_LOG: u32 = 12;
const HASH_MUL: u32 = 2_654_435_761;

fn le32(src: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(src[p..p + 4].try_into().unwrap())
}

fn hash(v: u32) -> usize {
    (v.wrapping_mul(HASH_MUL) >> (32 - HASH_LOG)) as usize
}

/// Length-extension bytes for a token nibble that hit 15: `255`* then the remainder (lz4.md §1).
fn emit_length(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

fn emit_sequence(out: &mut Vec<u8>, literals: &[u8], offset: usize, mlen: usize) {
    let lit = literals.len();
    let ml = mlen - MIN_MATCH;
    out.push(((lit.min(15) as u8) << 4) | ml.min(15) as u8);
    if lit >= 15 {
        emit_length(out, lit - 15);
    }
    out.extend_from_slice(literals);
    // The u16 offset is LITTLE-endian — the one deliberate exception to the big-endian house
    // rule, so the blob stays readable by any conformant LZ4 decoder (lz4.md §1).
    out.extend_from_slice(&(offset as u16).to_le_bytes());
    if ml >= 15 {
        emit_length(out, ml - 15);
    }
}

fn emit_last_literals(out: &mut Vec<u8>, literals: &[u8]) {
    let lit = literals.len();
    out.push((lit.min(15) as u8) << 4);
    if lit >= 15 {
        emit_length(out, lit - 15);
    }
    out.extend_from_slice(literals);
}

/// The pinned encoder (lz4.md §2): one input → one output, in every core.
pub fn compress(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut table = vec![-1i64; 1 << HASH_LOG];
    let mut anchor = 0usize;
    let mut p = 0usize;
    // `limit` = the last legal match start (may be "negative": empty range when src is short).
    let limit = src.len() as i64 - MFLIMIT as i64;
    while (p as i64) <= limit {
        let h = hash(le32(src, p));
        let cand = table[h];
        table[h] = p as i64; // store AFTER reading the candidate
        if cand >= 0 && p - cand as usize <= MAX_OFFSET && le32(src, cand as usize) == le32(src, p)
        {
            let cand = cand as usize;
            let maxend = src.len() - LAST_LITERALS;
            let mut mlen = MIN_MATCH;
            while p + mlen < maxend && src[cand + mlen] == src[p + mlen] {
                mlen += 1;
            }
            emit_sequence(&mut out, &src[anchor..p], p - cand, mlen);
            p += mlen; // positions inside the match are NOT hashed
            anchor = p;
        } else {
            p += 1; // step is always 1 (no acceleration)
        }
    }
    emit_last_literals(&mut out, &src[anchor..]);
    out
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}

/// The decoder (lz4.md §3): decode `comp` to exactly `raw_len` bytes or fail `data_corrupted`.
pub fn decompress(comp: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    // No block expands more than 255x (a match-length extension byte adds at most 255 bytes), so
    // this is raw_len for any valid block and keeps a corrupt raw_len from sizing the buffer.
    let mut out = Vec::with_capacity(raw_len.min(255 * comp.len() + 16));
    let mut i = 0usize;
    let n = comp.len();
    loop {
        if i >= n {
            return Err(corrupt("truncated compressed block"));
        }
        let token = comp[i];
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                if i >= n {
                    return Err(corrupt("truncated compressed block"));
                }
                let b = comp[i];
                i += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        if i + lit > n {
            return Err(corrupt("truncated compressed block"));
        }
        if out.len() + lit > raw_len {
            return Err(corrupt("decompressed length overflow"));
        }
        out.extend_from_slice(&comp[i..i + lit]);
        i += lit;
        if i == n {
            break; // a literals-only tail ends the block
        }
        if i + 2 > n {
            return Err(corrupt("truncated compressed block"));
        }
        let offset = u16::from_le_bytes([comp[i], comp[i + 1]]) as usize;
        i += 2;
        if offset == 0 || offset > out.len() {
            return Err(corrupt("invalid match offset"));
        }
        let mut ml = (token & 0x0F) as usize;
        if ml == 15 {
            loop {
                if i >= n {
                    return Err(corrupt("truncated compressed block"));
                }
                let b = comp[i];
                i += 1;
                ml += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        ml += MIN_MATCH;
        if out.len() + ml > raw_len {
            return Err(corrupt("decompressed length overflow"));
        }
        // An overlapping match (offset < length) replicates the run byte by byte ascending (lz4.md
        // §3), which is the bytes from `from` repeated with period `offset`. Copying everything
        // already written from `from` keeps that distance a whole number of periods, so each block
        // copy continues the run exactly; a non-overlapping match is one copy.
        let from = out.len() - offset;
        let mut remaining = ml;
        while remaining > 0 {
            let chunk = remaining.min(out.len() - from);
            out.extend_from_within(from..from + chunk);
            remaining -= chunk;
        }
    }
    if out.len() != raw_len {
        return Err(corrupt("decompressed length mismatch"));
    }
    Ok(out)
}

/// Whether `comp` is a valid block that decodes ([`decompress`]) to exactly `expected` — checked
/// against `expected` in place, without building the output. A literal run must equal the bytes it
/// covers, and a match of length `ml` at offset `d` must leave `expected[p..p + ml]` equal to
/// `expected[p - d..p - d + ml]`: the decoder's overlapping copy repeats the run with period `d`,
/// which is exactly that slice equality. By induction over the sequences, a block passing every
/// check decodes to `expected`, and any other block fails one (or a bound) — the exact test the
/// compression memo verifies a hit with ([`crate::compress_memo`]).
pub fn decodes_to(comp: &[u8], expected: &[u8]) -> bool {
    let n = comp.len();
    let raw_len = expected.len();
    let mut i = 0usize;
    let mut pos = 0usize;
    // A length nibble of 15 continues in `255`* + a final byte (lz4.md §1); `None` when truncated.
    let extend = |i: &mut usize, mut len: usize| -> Option<usize> {
        loop {
            let b = *comp.get(*i)?;
            *i += 1;
            len += b as usize;
            if b != 255 {
                return Some(len);
            }
        }
    };
    loop {
        let Some(&token) = comp.get(i) else {
            return false;
        };
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            match extend(&mut i, lit) {
                Some(l) => lit = l,
                None => return false,
            }
        }
        if i + lit > n || pos + lit > raw_len || comp[i..i + lit] != expected[pos..pos + lit] {
            return false;
        }
        i += lit;
        pos += lit;
        if i == n {
            return pos == raw_len; // a literals-only tail ends the block
        }
        if i + 2 > n {
            return false;
        }
        let offset = u16::from_le_bytes([comp[i], comp[i + 1]]) as usize;
        i += 2;
        if offset == 0 || offset > pos {
            return false;
        }
        let mut ml = (token & 0x0F) as usize;
        if ml == 15 {
            match extend(&mut i, ml) {
                Some(l) => ml = l,
                None => return false,
            }
        }
        ml += MIN_MATCH;
        if pos + ml > raw_len
            || expected[pos..pos + ml] != expected[pos - offset..pos - offset + ml]
        {
            return false;
        }
        pos += ml;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_to_matches_decompress() {
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            vec![b'a'; 32],
            b"abc".repeat(400),
            vec![b'y'; 70_000],
            b"the quick brown fox jumps over the lazy dog ".repeat(60),
            (0..5000u32)
                .flat_map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes())
                .collect(),
        ];
        for src in &cases {
            let comp = compress(src);
            assert!(decodes_to(&comp, src));
            // Any other expected output is refused: one byte changed, one byte more or less.
            if !src.is_empty() {
                let mut flipped = src.clone();
                let mid = flipped.len() / 2;
                flipped[mid] ^= 1;
                assert!(!decodes_to(&comp, &flipped));
                assert!(!decodes_to(&comp, &src[..src.len() - 1]));
            }
            let mut longer = src.clone();
            longer.push(b'z');
            assert!(!decodes_to(&comp, &longer));
            // Truncated or corrupt blocks are refused, never a panic.
            for cut in 0..comp.len() {
                assert!(
                    !decodes_to(&comp[..cut], src) || decompress(&comp[..cut], src.len()).is_ok()
                );
            }
        }
    }

    #[test]
    fn empty_input_is_the_lone_zero_token() {
        assert_eq!(compress(&[]), vec![0x00]);
        assert_eq!(decompress(&[0x00], 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trips() {
        let cases: Vec<Vec<u8>> = vec![
            b"a".to_vec(),
            b"abcdefghijkl".to_vec(),
            vec![b'a'; 13],
            vec![b'a'; 32],
            b"abc".repeat(40),
            vec![b'y'; 1000],
            b"the quick brown fox jumps over the lazy dog ".repeat(6),
        ];
        for src in cases {
            let comp = compress(&src);
            assert_eq!(decompress(&comp, src.len()).unwrap(), src);
        }
    }

    #[test]
    fn malformed_blocks_are_data_corrupted() {
        // Truncated: a token promising literals that aren't there.
        assert!(decompress(&[0x50], 5).is_err());
        // Zero offset.
        assert!(decompress(&[0x14, b'a', 0x00, 0x00, 0x00], 10).is_err());
        // Offset beyond the decoded prefix.
        assert!(decompress(&[0x14, b'a', 0x05, 0x00, 0x00], 10).is_err());
        // Output would exceed the expected raw length.
        assert!(decompress(&[0x1F, b'a', 0x01, 0x00, 0xFF, 0xFF, 0x00], 4).is_err());
        // Length mismatch: decodes clean but short.
        assert!(decompress(&[0x10, b'a'], 2).is_err());
    }
}
