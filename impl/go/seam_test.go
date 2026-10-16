package jed

import "testing"

// seededEnv returns an environment on the provided deterministic random source (seed) + a manual
// clock — the test / reproducible path (spec/design/entropy.md §6).
func seededEnv(seed uint64, clock int64) *Environment {
	b := NewEnvironmentBuilder()
	b.SetRandomSource(NewSeededRandom(seed))
	b.SetClockSource(NewManualClock(clock))
	return b.Build()
}

func TestSplitmix64PinnedVectors(t *testing.T) {
	t.Parallel()
	// The provided seeded source must fill bytes from the spec'd byte-exact stream (entropy.md §2).
	// seed 1 → 910a2dec89025cc1, beeb8da1658eec67, f893a2eefb32555e (big-endian).
	src := NewSeededRandom(1)
	buf := make([]byte, 24)
	if err := src.Fill(buf); err != nil {
		t.Fatal(err)
	}
	want := []byte{
		0x91, 0x0a, 0x2d, 0xec, 0x89, 0x02, 0x5c, 0xc1,
		0xbe, 0xeb, 0x8d, 0xa1, 0x65, 0x8e, 0xec, 0x67,
		0xf8, 0x93, 0xa2, 0xee, 0xfb, 0x32, 0x55, 0x5e,
	}
	if string(buf) != string(want) {
		t.Fatalf("seeded source bytes = %x, want %x", buf, want)
	}
	// Reseed restarts the stream exactly as a fresh source.
	src.Reseed(1)
	again := make([]byte, 24)
	if err := src.Fill(again); err != nil {
		t.Fatal(err)
	}
	if string(again) != string(want) {
		t.Fatalf("reseeded source bytes = %x, want %x", again, want)
	}
}

func TestUUIDv4DeterministicAndWellFormed(t *testing.T) {
	t.Parallel()
	env := seededEnv(1, 0)
	r := newStmtRng()
	b, err := r.uuidV4(env)
	if err != nil {
		t.Fatal(err)
	}
	want, _ := parseUUID("910a2dec-8902-4cc1-beeb-8da1658eec67")
	if string(b) != string(want) {
		t.Fatalf("uuidv4 = %x, want %x", b, want)
	}
	if v, ok := uuidExtractVersion(b); !ok || v != 4 {
		t.Fatalf("version = %d (%v), want 4", v, ok)
	}
}

func TestUUIDv7EmbedsClockAndIsMonotonic(t *testing.T) {
	t.Parallel()
	clock := int64(1_721_056_591_872_000)
	env := seededEnv(42, clock)
	r := newStmtRng()
	a, err := r.uuidV7(env, clock)
	if err != nil {
		t.Fatal(err)
	}
	b, err := r.uuidV7(env, clock)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := uuidExtractVersion(a); !ok || v != 7 {
		t.Fatalf("version = %d (%v), want 7", v, ok)
	}
	if mc, ok := uuidExtractTimestampMicros(a); !ok || mc != 1_721_056_591_872_000 {
		t.Fatalf("timestamp = %d (%v)", mc, ok)
	}
	if string(a) >= string(b) {
		t.Fatalf("uuidv7 must be monotonic within a statement-millisecond: %x !< %x", a, b)
	}
}

func TestUnseededPathUsesOSEntropyAndWallClock(t *testing.T) {
	t.Parallel()
	// The PRODUCTION path: the empty environment's default sources → crypto/rand per draw + the wall
	// clock. Assert only STRUCTURAL invariants so the outcome is deterministic.
	env := NewEnvironment()
	r := newStmtRng()
	v4, err := r.uuidV4(env)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := uuidExtractVersion(v4); !ok || v != 4 {
		t.Fatalf("version = %d (%v), want 4", v, ok)
	}
	v7, err := r.uuidV7(env, r.statementClockMicros(env))
	if err != nil {
		t.Fatal(err)
	}
	if mc, ok := uuidExtractTimestampMicros(v7); !ok || mc <= 1_577_836_800_000_000 {
		t.Fatalf("v7 timestamp = %d (%v), want a plausible wall-clock instant", mc, ok)
	}
}

func TestUUIDv7RejectsOutOfRange(t *testing.T) {
	t.Parallel()
	env := seededEnv(1, 0)
	r := newStmtRng()
	if _, err := r.uuidV7(env, -1000000); err == nil {
		t.Fatal("expected 22008 for a pre-epoch clock")
	}
}

func TestAdvancingClockStepsPerReadAndNowCaches(t *testing.T) {
	t.Parallel()
	// The advancing clock yields start, start+step, … one increment per read (entropy.md §6).
	clk := NewAdvancingClock(1000, 1)
	for i, want := range []int64{1000, 1001, 1002} {
		if got := clk.NowMicros(); got != want {
			t.Fatalf("advancing read %d = %d, want %d", i, got, want)
		}
	}
	// Reset restarts it exactly as a fresh clock.
	clk.Reset(1000, 1)
	if got := clk.NowMicros(); got != 1000 {
		t.Fatalf("reset read = %d, want 1000", got)
	}
	// now() (statementClockMicros) reads ONCE and caches: it pulls 1000 then stays 1000 even as
	// clock_timestamp() (clockNowMicros) keeps advancing the SAME source — the stable-vs-volatile
	// distinction, made deterministic.
	b := NewEnvironmentBuilder()
	b.SetClockSource(NewAdvancingClock(1000, 1))
	env := b.Build()
	r := newStmtRng()
	if got := r.statementClockMicros(env); got != 1000 {
		t.Fatalf("first statement clock = %d, want 1000", got)
	}
	if got := r.clockNowMicros(env); got != 1001 {
		t.Fatalf("first clock_timestamp = %d, want 1001", got)
	}
	if got := r.clockNowMicros(env); got != 1002 {
		t.Fatalf("second clock_timestamp = %d, want 1002", got)
	}
	if got := r.statementClockMicros(env); got != 1000 {
		t.Fatalf("statement clock after advances = %d, want cached 1000", got)
	}
}

func TestManualClockSetAndAdvance(t *testing.T) {
	t.Parallel()
	clk := NewManualClock(5)
	if got := clk.NowMicros(); got != 5 {
		t.Fatalf("manual clock = %d, want 5", got)
	}
	if got := clk.NowMicros(); got != 5 {
		t.Fatalf("manual clock must not move on read: %d", got)
	}
	clk.Advance(10)
	if got := clk.NowMicros(); got != 15 {
		t.Fatalf("advanced manual clock = %d, want 15", got)
	}
	clk.Set(-3)
	if got := clk.NowMicros(); got != -3 {
		t.Fatalf("set manual clock = %d, want -3", got)
	}
}
