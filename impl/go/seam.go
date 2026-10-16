package jed

import (
	"crypto/rand"
	"encoding/binary"
	"sync"
	"time"
)

// The entropy + clock sources (spec/design/entropy.md, environment.md §4) — the two outside inputs
// that feed the volatile UUID generators (uuidv4/uuidv7) and the clock functions. An Environment
// names one of each (EnvironmentBuilder.SetRandomSource / SetClockSource), defaulting to the platform
// primitive:
//
//   - the RANDOM SOURCE — fills N bytes; default = the OS CSPRNG (crypto/rand), drawn PER VALUE (so
//     production UUIDs are unpredictable, not derived from a single seeded PRNG).
//   - the CLOCK SOURCE  — returns micros since the Unix epoch; default = the wall clock (time.Now).
//
// The environment fixes WHICH source is installed, not the source's internal state: a source is an
// object the host may keep a reference to and control (reseed a SeededRandom, set or advance a
// ManualClock, reset an AdvancingClock). The conformance harness does exactly that for the # seed: /
// # clock: / # clock_advance: directives, which is what makes the generators byte-identical across
// cores. One environment may serve many databases and sessions at once, so every provided source is
// goroutine-safe, and a host's own source must be too. The engine contains NO production PRNG —
// splitmix64 lives here only as the provided DETERMINISTIC source a caller may opt into.

// RandomSource fills buf with len(buf) random bytes. Must be safe for concurrent use.
type RandomSource interface {
	Fill(buf []byte) error
}

// ClockSource returns the current time in micros since the Unix epoch. Must be safe for concurrent
// use.
type ClockSource interface {
	NowMicros() int64
}

type osRandomSource struct{}

// OSRandomSource returns the default random source: the OS CSPRNG (crypto/rand), drawn per value.
func OSRandomSource() RandomSource { return osRandomSource{} }

func (osRandomSource) Fill(buf []byte) error {
	if _, err := rand.Read(buf); err != nil {
		return newError(IoError, "OS entropy source unavailable")
	}
	return nil
}

type wallClock struct{}

// WallClock returns the default clock source: the wall clock (time.Now).
func WallClock() ClockSource { return wallClock{} }

func (wallClock) NowMicros() int64 { return time.Now().UnixMicro() }

// splitmix64 constants (entropy.md §2; identical to the bench PRNG, re-authored as engine data).
const (
	smGamma uint64 = 0x9E3779B97F4A7C15
	smMix1  uint64 = 0xBF58476D1CE4E5B9
	smMix2  uint64 = 0x94D049BB133111EB
)

// SeededRandom is the provided DETERMINISTIC random source: a splitmix64 stream, serialized
// big-endian in 8-byte chunks (a final partial chunk takes the high bytes of one more draw — never
// hit by the 16-/8-byte uuid fills). Byte-pinned in spec/encoding/prng.toml and asserted cross-core
// (entropy.md §2). What a host installs for reproducibility and what the conformance harness reseeds
// for the # seed: directive. Not the default.
type SeededRandom struct {
	mu    sync.Mutex
	state uint64
}

// NewSeededRandom returns a seeded source whose stream starts at seed.
func NewSeededRandom(seed uint64) *SeededRandom { return &SeededRandom{state: seed} }

// Reseed restarts the stream exactly as a fresh NewSeededRandom(seed) would.
func (r *SeededRandom) Reseed(seed uint64) {
	r.mu.Lock()
	r.state = seed
	r.mu.Unlock()
}

// Fill draws len(buf) bytes from the stream; it never fails.
func (r *SeededRandom) Fill(buf []byte) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	for i := 0; i < len(buf); {
		r.state += smGamma
		x := r.state
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
	return nil
}

// ManualClock is the provided MANUAL clock source: it returns the instant it was last set to, and
// moves only when told (Set / Advance). The # clock: directive sets one (entropy.md §6); a host
// wanting a frozen or hand-stepped instant uses it too.
type ManualClock struct {
	mu     sync.Mutex
	micros int64
}

// NewManualClock returns a manual clock reading micros.
func NewManualClock(micros int64) *ManualClock { return &ManualClock{micros: micros} }

// Set moves the clock to micros.
func (c *ManualClock) Set(micros int64) {
	c.mu.Lock()
	c.micros = micros
	c.mu.Unlock()
}

// Advance moves the clock forward by step micros (backward for a negative step).
func (c *ManualClock) Advance(step int64) {
	c.mu.Lock()
	c.micros += step
	c.mu.Unlock()
}

// NowMicros returns the clock's current instant.
func (c *ManualClock) NowMicros() int64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.micros
}

// AdvancingClock is the provided ADVANCING clock source: it returns start, then start+step,
// start+2·step, … — one increment per read. The # clock_advance: directive installs this
// (entropy.md §6) to make clock_timestamp()'s per-call reads deterministic and distinguishable from
// the statement-stable now() cross-core; the draw order follows expression-evaluation order.
type AdvancingClock struct {
	mu   sync.Mutex
	cur  int64
	step int64
}

// NewAdvancingClock returns an advancing clock whose first read is start.
func NewAdvancingClock(start, step int64) *AdvancingClock {
	return &AdvancingClock{cur: start, step: step}
}

// Reset restarts the clock exactly as a fresh NewAdvancingClock(start, step) would.
func (c *AdvancingClock) Reset(start, step int64) {
	c.mu.Lock()
	c.cur = start
	c.step = step
	c.mu.Unlock()
}

// NowMicros returns the current reading and advances by one step.
func (c *AdvancingClock) NowMicros() int64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	v := c.cur
	c.cur += c.step
	return v
}

// StmtRng is the per-statement mutable seam state: the uuidv7 monotonic counter and the
// once-resolved statement clock (entropy.md §5 — read once, reused, so a statement's time cannot
// vary row-to-row). The PRNG state itself lives in the environment's RandomSource.
type stmtRng struct {
	counter       uint32
	clock         int64
	clockResolved bool
}

func newStmtRng() *stmtRng { return &stmtRng{} }

// statementClockMicros returns the statement clock in micros since the Unix epoch, resolved once
// (entropy.md §5): the environment's clock source. Reused for every uuidv7 / now() in the statement (STABLE).
func (r *stmtRng) statementClockMicros(env *Environment) int64 {
	if !r.clockResolved {
		r.clock = env.nowMicros()
		r.clockResolved = true
	}
	return r.clock
}

// clockNowMicros is a fresh read of the environment's clock in micros since the Unix epoch — used by
// clock_timestamp() (entropy.md §5), which reads on EVERY call (VOLATILE) and so does NOT touch the
// once-resolved statement clock above. It caches nothing.
func (r *stmtRng) clockNowMicros(env *Environment) int64 {
	return env.nowMicros()
}

// uuidV4 — 16 bytes from the environment's random source, version/variant overwritten (entropy.md §3).
func (r *stmtRng) uuidV4(env *Environment) ([]byte, error) {
	b := make([]byte, 16)
	if err := env.fill(b); err != nil {
		return nil, err
	}
	return buildUUIDv4(b), nil
}

// uuidV7 — the 48-bit ms of shiftedMicros (the statement clock, possibly interval-shifted by the
// caller), a per-statement monotonic counter in rand_a, and 62 random bits (8 bytes from the environment's random source)
// in rand_b (entropy.md §3). An out-of-48-bit ms traps 22008.
func (r *stmtRng) uuidV7(env *Environment, shiftedMicros int64) ([]byte, error) {
	unixMs := floorDiv(shiftedMicros, 1000)
	if unixMs < 0 || unixMs >= (int64(1)<<48) {
		return nil, newError(DatetimeFieldOverflow, "uuidv7 timestamp out of range")
	}
	counter := uint16(r.counter & 0x0FFF)
	r.counter++
	var randB [8]byte
	if err := env.fill(randB[:]); err != nil {
		return nil, err
	}
	return buildUUIDv7(uint64(unixMs), counter, randB), nil
}
