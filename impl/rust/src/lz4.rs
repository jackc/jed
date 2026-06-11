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
    let mut out = Vec::with_capacity(raw_len);
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
        let from = out.len() - offset;
        for k in 0..ml {
            // Byte-by-byte ascending: an overlapping match replicates the run (lz4.md §3).
            let b = out[from + k];
            out.push(b);
        }
    }
    if out.len() != raw_len {
        return Err(corrupt("decompressed length mismatch"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
