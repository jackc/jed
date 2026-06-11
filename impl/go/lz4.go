package jed

// The pinned LZ4-block codec (spec/fileformat/lz4.md) — hand-rolled, deterministic, and
// byte-identical across every core (a library is inadmissible: encoders diverge — CLAUDE.md §14;
// spec/design/large-values.md §6). The encoder's free parameters are FIXED by lz4.md §2 (greedy
// match search, step 1, a 4096-entry single-candidate hash table, no backward extension); the
// output is pinned by spec/fileformat/lz4_vectors.toml and the compressed_table.jed golden. The
// decoder (lz4.md §3) is total and safe: every read is bounds-checked, the output never grows
// past the expected length, and malformed input is a structured data_corrupted (CLAUDE.md §13).

import "encoding/binary"

const (
	lz4MinMatch = 4
	lz4MaxOff   = 65535
	// lz4MFLimit: no match may start after len-12 (the block format's end constraint).
	lz4MFLimit = 12
	// lz4LastLiterals: no match may extend past len-5.
	lz4LastLiterals = 5
	lz4HashLog      = 12
	lz4HashMul      = 2654435761
)

func lz4Hash(v uint32) int { return int((v * lz4HashMul) >> (32 - lz4HashLog)) }

// lz4EmitLength appends extension bytes for a token nibble that hit 15: 255* then the
// remainder (lz4.md §1).
func lz4EmitLength(out []byte, n int) []byte {
	for n >= 255 {
		out = append(out, 255)
		n -= 255
	}
	return append(out, byte(n))
}

func lz4EmitSequence(out, literals []byte, offset, mlen int) []byte {
	lit := len(literals)
	ml := mlen - lz4MinMatch
	out = append(out, byte(min(lit, 15)<<4|min(ml, 15)))
	if lit >= 15 {
		out = lz4EmitLength(out, lit-15)
	}
	out = append(out, literals...)
	// The u16 offset is LITTLE-endian — the one deliberate exception to the big-endian house
	// rule, so the blob stays readable by any conformant LZ4 decoder (lz4.md §1).
	out = binary.LittleEndian.AppendUint16(out, uint16(offset))
	if ml >= 15 {
		out = lz4EmitLength(out, ml-15)
	}
	return out
}

func lz4EmitLastLiterals(out, literals []byte) []byte {
	lit := len(literals)
	out = append(out, byte(min(lit, 15)<<4))
	if lit >= 15 {
		out = lz4EmitLength(out, lit-15)
	}
	return append(out, literals...)
}

// lz4Compress is the pinned encoder (lz4.md §2): one input → one output, in every core.
func lz4Compress(src []byte) []byte {
	out := make([]byte, 0, len(src)/2+16)
	table := make([]int, 1<<lz4HashLog)
	for i := range table {
		table[i] = -1
	}
	anchor := 0
	p := 0
	limit := len(src) - lz4MFLimit // last legal match start (may be negative)
	for p <= limit {
		h := lz4Hash(binary.LittleEndian.Uint32(src[p:]))
		cand := table[h]
		table[h] = p // store AFTER reading the candidate
		if cand >= 0 && p-cand <= lz4MaxOff &&
			binary.LittleEndian.Uint32(src[cand:]) == binary.LittleEndian.Uint32(src[p:]) {
			maxend := len(src) - lz4LastLiterals
			mlen := lz4MinMatch
			for p+mlen < maxend && src[cand+mlen] == src[p+mlen] {
				mlen++
			}
			out = lz4EmitSequence(out, src[anchor:p], p-cand, mlen)
			p += mlen // positions inside the match are NOT hashed
			anchor = p
		} else {
			p++ // step is always 1 (no acceleration)
		}
	}
	return lz4EmitLastLiterals(out, src[anchor:])
}

// lz4Decompress decodes comp to exactly rawLen bytes or fails data_corrupted (lz4.md §3).
func lz4Decompress(comp []byte, rawLen int) ([]byte, error) {
	out := make([]byte, 0, rawLen)
	i := 0
	n := len(comp)
	for {
		if i >= n {
			return nil, NewError(DataCorrupted, "truncated compressed block")
		}
		token := comp[i]
		i++
		lit := int(token >> 4)
		if lit == 15 {
			for {
				if i >= n {
					return nil, NewError(DataCorrupted, "truncated compressed block")
				}
				b := comp[i]
				i++
				lit += int(b)
				if b != 255 {
					break
				}
			}
		}
		if i+lit > n {
			return nil, NewError(DataCorrupted, "truncated compressed block")
		}
		if len(out)+lit > rawLen {
			return nil, NewError(DataCorrupted, "decompressed length overflow")
		}
		out = append(out, comp[i:i+lit]...)
		i += lit
		if i == n {
			break // a literals-only tail ends the block
		}
		if i+2 > n {
			return nil, NewError(DataCorrupted, "truncated compressed block")
		}
		offset := int(binary.LittleEndian.Uint16(comp[i:]))
		i += 2
		if offset == 0 || offset > len(out) {
			return nil, NewError(DataCorrupted, "invalid match offset")
		}
		ml := int(token & 0x0F)
		if ml == 15 {
			for {
				if i >= n {
					return nil, NewError(DataCorrupted, "truncated compressed block")
				}
				b := comp[i]
				i++
				ml += int(b)
				if b != 255 {
					break
				}
			}
		}
		ml += lz4MinMatch
		if len(out)+ml > rawLen {
			return nil, NewError(DataCorrupted, "decompressed length overflow")
		}
		from := len(out) - offset
		for k := 0; k < ml; k++ {
			// Byte-by-byte ascending: an overlapping match replicates the run (lz4.md §3).
			out = append(out, out[from+k])
		}
	}
	if len(out) != rawLen {
		return nil, NewError(DataCorrupted, "decompressed length mismatch")
	}
	return out, nil
}
