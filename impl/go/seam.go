package jed

import (
	"crypto/rand"
	"encoding/binary"
	"time"
)

// The entropy + clock seam (spec/design/entropy.md) — two host-injectable functions that feed the
// volatile UUID generators (uuidv4/uuidv7), each defaulting to the platform primitive:
//
//   - the RANDOM SOURCE — fills N bytes; default = the OS CSPRNG (crypto/rand), drawn PER VALUE (so
//     production UUIDs are unpredictable, not derived from a single seeded PRNG).
//   - the CLOCK SOURCE  — returns micros since the Unix epoch; default = the wall clock (time.Now).
//
// A host injects its own functions for reproducibility (e.g. a controllable clock, or the provided
// SeededRandomSource below). The conformance harness injects exactly those via the # seed: /
// # clock: directives, which is what makes the generators byte-identical across cores. The engine
// itself contains NO production PRNG — splitmix64 lives here only as the provided DETERMINISTIC
// source a caller may opt into; it is never the default.

// RandomSource fills its argument with len(buf) random bytes. A deterministic source (e.g.
// SeededRandomSource) advances its own captured state per call.
type RandomSource func(buf []byte)

// ClockSource returns micros since the Unix epoch (a host may inject an advancing/simulated clock).
type ClockSource func() int64

// Seam is the host seam carried on the Database handle (spec/design/api.md §10): the injected random
// + clock functions, each nil ⇒ the platform default. Only the volatile uuid generators touch it;
// every other expression ignores it.
type Seam struct {
	random RandomSource
	clock  ClockSource
}

// SetRandom injects a random source (the deterministic / reproducible path); ClearRandom falls back
// to the OS CSPRNG, drawn per value (production — unpredictable output).
func (s *Seam) SetRandom(f RandomSource) { s.random = f }
func (s *Seam) ClearRandom()             { s.random = nil }

// SetClock injects a clock source; ClearClock falls back to the wall clock (production).
func (s *Seam) SetClock(f ClockSource) { s.clock = f }
func (s *Seam) ClearClock()            { s.clock = nil }

// fill writes len(buf) random bytes: the injected source, else the OS CSPRNG (crypto/rand).
func (s *Seam) fill(buf []byte) error {
	if s.random != nil {
		s.random(buf)
		return nil
	}
	if _, err := rand.Read(buf); err != nil {
		return NewError(IoError, "OS entropy source unavailable")
	}
	return nil
}

// nowMicros returns the current time in micros since the Unix epoch: the injected clock, else the
// wall clock.
func (s *Seam) nowMicros() int64 {
	if s.clock != nil {
		return s.clock()
	}
	return time.Now().UnixMicro()
}

// splitmix64 constants (entropy.md §2; identical to the bench PRNG, re-authored as engine data).
const (
	smGamma uint64 = 0x9E3779B97F4A7C15
	smMix1  uint64 = 0xBF58476D1CE4E5B9
	smMix2  uint64 = 0x94D049BB133111EB
)

// SeededRandomSource is the provided DETERMINISTIC random source: a splitmix64 stream seeded with
// seed, serialized big-endian in 8-byte chunks (a final partial chunk takes the high bytes of one
// more draw — never hit by the 16-/8-byte uuid fills). This is what a host injects for
// reproducibility and what the conformance harness injects for the # seed: directive; it is
// byte-pinned in spec/encoding/prng.toml and asserted cross-core (entropy.md §2). Not the default.
func SeededRandomSource(seed uint64) RandomSource {
	state := seed
	return func(buf []byte) {
		for i := 0; i < len(buf); {
			state += smGamma
			x := state
			x = (x ^ (x >> 30)) * smMix1
			x = (x ^ (x >> 27)) * smMix2
			var chunk [8]byte
			binary.BigEndian.PutUint64(chunk[:], x^(x>>31))
			n := len(buf) - i
			if n > 8 {
				n = 8
			}
			copy(buf[i:i+n], chunk[:n])
			i += n
		}
	}
}

// FixedClock is the provided FIXED clock source: always returns micros. The # clock: directive
// injects this (entropy.md §6); a host wanting a frozen instant uses it too.
func FixedClock(micros int64) ClockSource { return func() int64 { return micros } }

// StmtRng is the per-statement mutable seam state: the uuidv7 monotonic counter and the
// once-resolved statement clock (entropy.md §5 — read once, reused, so a statement's time cannot
// vary row-to-row). The PRNG state itself lives in the injected RandomSource (handle-scoped).
type StmtRng struct {
	counter       uint32
	clock         int64
	clockResolved bool
}

func newStmtRng() *StmtRng { return &StmtRng{} }

// statementClockMicros returns the statement clock in micros since the Unix epoch, resolved once
// (entropy.md §5): the seam's clock source. Reused for every uuidv7 in the statement.
func (r *StmtRng) statementClockMicros(seam *Seam) int64 {
	if !r.clockResolved {
		r.clock = seam.nowMicros()
		r.clockResolved = true
	}
	return r.clock
}

// uuidV4 — 16 bytes from the seam's random source, version/variant overwritten (entropy.md §3).
func (r *StmtRng) uuidV4(seam *Seam) ([]byte, error) {
	b := make([]byte, 16)
	if err := seam.fill(b); err != nil {
		return nil, err
	}
	return buildUUIDv4(b), nil
}

// uuidV7 — the 48-bit ms of shiftedMicros (the statement clock, possibly interval-shifted by the
// caller), a per-statement monotonic counter in rand_a, and 62 random bits (8 bytes from the seam)
// in rand_b (entropy.md §3). An out-of-48-bit ms traps 22008.
func (r *StmtRng) uuidV7(seam *Seam, shiftedMicros int64) ([]byte, error) {
	unixMs := floorDiv(shiftedMicros, 1000)
	if unixMs < 0 || unixMs >= (int64(1)<<48) {
		return nil, NewError(DatetimeFieldOverflow, "uuidv7 timestamp out of range")
	}
	counter := uint16(r.counter & 0x0FFF)
	r.counter++
	var randB [8]byte
	if err := seam.fill(randB[:]); err != nil {
		return nil, err
	}
	return buildUUIDv7(uint64(unixMs), counter, randB), nil
}
