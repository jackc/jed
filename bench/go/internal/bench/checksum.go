package bench

import "strconv"

// Checksum accumulates the cross-engine answer hash (spec/design/benchmarks.md §6):
// FNV-1a 64 over canonically rendered result values, 0x1F after each value, 0x1E after
// each row. Engine adapters fold every measured-iteration row into one of these; equal
// sums across all binaries prove equal answers.
type Checksum struct {
	h uint64
}

const (
	fnvOffset = 0xcbf29ce484222325
	fnvPrime  = 0x100000001b3
)

// NewChecksum starts a fresh accumulator.
func NewChecksum() *Checksum { return &Checksum{h: fnvOffset} }

func (c *Checksum) bytes(s string) {
	h := c.h
	for i := 0; i < len(s); i++ {
		h = (h ^ uint64(s[i])) * fnvPrime
	}
	c.h = h
}

func (c *Checksum) sep(b byte) { c.h = (c.h ^ uint64(b)) * fnvPrime }

// Null folds a NULL value.
func (c *Checksum) Null() { c.bytes("NULL"); c.sep(0x1F) }

// Int folds an integer value (canonical decimal rendering).
func (c *Checksum) Int(n int64) { c.bytes(strconv.FormatInt(n, 10)); c.sep(0x1F) }

// Text folds a text value (raw bytes).
func (c *Checksum) Text(s string) { c.bytes(s); c.sep(0x1F) }

// EndRow marks the end of one result row.
func (c *Checksum) EndRow() { c.sep(0x1E) }

// Hex returns the 16-lowercase-hex-char digest.
func (c *Checksum) Hex() string {
	const digits = "0123456789abcdef"
	var out [16]byte
	for i := 0; i < 16; i++ {
		out[i] = digits[(c.h>>uint(60-4*i))&0xF]
	}
	return string(out[:])
}
