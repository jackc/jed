package jed

import "testing"

// seededSeam returns a seam on the provided deterministic random source (seed) + a fixed clock —
// the test / reproducible path (spec/design/entropy.md §6).
func seededSeam(seed uint64, clock int64) Seam {
	var s Seam
	s.SetRandom(SeededRandomSource(seed))
	s.SetClock(FixedClock(clock))
	return s
}

func TestSplitmix64PinnedVectors(t *testing.T) {
	// The provided seeded source must fill bytes from the spec'd byte-exact stream (entropy.md §2).
	// seed 1 → 910a2dec89025cc1, beeb8da1658eec67, f893a2eefb32555e (big-endian).
	src := SeededRandomSource(1)
	buf := make([]byte, 24)
	src(buf)
	want := []byte{
		0x91, 0x0a, 0x2d, 0xec, 0x89, 0x02, 0x5c, 0xc1,
		0xbe, 0xeb, 0x8d, 0xa1, 0x65, 0x8e, 0xec, 0x67,
		0xf8, 0x93, 0xa2, 0xee, 0xfb, 0x32, 0x55, 0x5e,
	}
	if string(buf) != string(want) {
		t.Fatalf("seeded source bytes = %x, want %x", buf, want)
	}
}

func TestUUIDv4DeterministicAndWellFormed(t *testing.T) {
	seam := seededSeam(1, 0)
	r := newStmtRng()
	b, err := r.uuidV4(&seam)
	if err != nil {
		t.Fatal(err)
	}
	want, _ := ParseUUID("910a2dec-8902-4cc1-beeb-8da1658eec67")
	if string(b) != string(want) {
		t.Fatalf("uuidv4 = %x, want %x", b, want)
	}
	if v, ok := uuidExtractVersion(b); !ok || v != 4 {
		t.Fatalf("version = %d (%v), want 4", v, ok)
	}
}

func TestUUIDv7EmbedsClockAndIsMonotonic(t *testing.T) {
	clock := int64(1_721_056_591_872_000)
	seam := seededSeam(42, clock)
	r := newStmtRng()
	a, err := r.uuidV7(&seam, clock)
	if err != nil {
		t.Fatal(err)
	}
	b, err := r.uuidV7(&seam, clock)
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
	// The PRODUCTION path: a default seam (no injected source) → crypto/rand per draw + the wall
	// clock. Assert only STRUCTURAL invariants so the outcome is deterministic.
	var seam Seam
	r := newStmtRng()
	v4, err := r.uuidV4(&seam)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := uuidExtractVersion(v4); !ok || v != 4 {
		t.Fatalf("version = %d (%v), want 4", v, ok)
	}
	v7, err := r.uuidV7(&seam, r.statementClockMicros(&seam))
	if err != nil {
		t.Fatal(err)
	}
	if mc, ok := uuidExtractTimestampMicros(v7); !ok || mc <= 1_577_836_800_000_000 {
		t.Fatalf("v7 timestamp = %d (%v), want a plausible wall-clock instant", mc, ok)
	}
}

func TestUUIDv7RejectsOutOfRange(t *testing.T) {
	seam := seededSeam(1, 0)
	r := newStmtRng()
	if _, err := r.uuidV7(&seam, -1000000); err == nil {
		t.Fatal("expected 22008 for a pre-epoch clock")
	}
}

func TestAdvancingClockStepsPerReadAndNowCaches(t *testing.T) {
	// The advancing clock yields start, start+step, … one increment per read (entropy.md §6).
	clk := AdvancingClock(1000, 1)
	for i, want := range []int64{1000, 1001, 1002} {
		if got := clk(); got != want {
			t.Fatalf("advancing read %d = %d, want %d", i, got, want)
		}
	}
	// now() (statementClockMicros) reads ONCE and caches: it pulls 1000 then stays 1000 even as
	// clock_timestamp() (clockNowMicros) keeps advancing the SAME source — the stable-vs-volatile
	// distinction, made deterministic.
	var seam Seam
	seam.SetClock(AdvancingClock(1000, 1))
	r := newStmtRng()
	if got := r.statementClockMicros(&seam); got != 1000 {
		t.Fatalf("first statement clock = %d, want 1000", got)
	}
	if got := r.clockNowMicros(&seam); got != 1001 {
		t.Fatalf("first clock_timestamp = %d, want 1001", got)
	}
	if got := r.clockNowMicros(&seam); got != 1002 {
		t.Fatalf("second clock_timestamp = %d, want 1002", got)
	}
	if got := r.statementClockMicros(&seam); got != 1000 {
		t.Fatalf("statement clock after advances = %d, want cached 1000", got)
	}
}
