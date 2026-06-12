// Package bench is the shared plumbing for the jed benchmark harness binaries
// (spec/design/benchmarks.md). Corpus parsing, the splitmix64 param stream, the FNV-1a
// answer checksum, fingerprint checks, and the engine-agnostic run loop live here; each
// cmd/bench-* binary contributes only its driver.
package bench

// Prng is the shared splitmix64 generator (spec/design/benchmarks.md §4). Every harness
// in every language implements exactly this so all engines answer the identical query
// sequence; the pinned vectors in the design doc are asserted in prng_test.go.
type Prng struct {
	z uint64
}

// NewPrng seeds a stream.
func NewPrng(seed uint64) *Prng { return &Prng{z: seed} }

// Next returns the next raw 64-bit output.
func (p *Prng) Next() uint64 {
	p.z += 0x9E3779B97F4A7C15
	x := p.z
	x = (x ^ (x >> 30)) * 0xBF58476D1CE4E5B9
	x = (x ^ (x >> 27)) * 0x94D049BB133111EB
	return x ^ (x >> 31)
}

// IntUniform draws an integer in [lo, hi] (inclusive). Modulo bias is accepted — it is
// deterministic and identical across all harnesses, which is all the contract needs.
func (p *Prng) IntUniform(lo, hi int64) int64 {
	span := uint64(hi-lo) + 1
	return lo + int64(p.Next()%span)
}

// Text draws a lowercase ASCII string with length in [minLen, maxLen]: one bounded draw
// for the length, then one per character ('a' + next() % 26).
func (p *Prng) Text(minLen, maxLen int64) string {
	n := p.IntUniform(minLen, maxLen)
	b := make([]byte, n)
	for i := range b {
		b[i] = byte('a' + p.Next()%26)
	}
	return string(b)
}
