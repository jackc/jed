package jed

import "testing"

func mustUUID(t *testing.T, s string) []byte {
	t.Helper()
	b, reason := ParseUUID(s)
	if reason != "" {
		t.Fatalf("ParseUUID(%q): %s", s, reason)
	}
	return b
}

func TestUUIDExtractVersion(t *testing.T) {
	// PG 18 oracle (spec/design/functions.md §12).
	cases := []struct {
		s    string
		want int64
		ok   bool
	}{
		{"5b2cc7f0-9a3e-4e7b-8c1d-2f3a4b5c6d7e", 4, true},
		{"0190b6f7-8000-7000-8000-000000000000", 7, true},
		{"c232ab00-9414-11ec-b3c8-9e6bdeced846", 1, true},
		{"1ec9414c-232a-6b00-b3c8-9e6bdeced846", 6, true},
		// nil (variant 0), non-RFC (variant 0), Microsoft GUID (variant 11) → NULL.
		{"00000000-0000-0000-0000-000000000000", 0, false},
		{"5b2cc7f0-9a3e-4e7b-0c1d-2f3a4b5c6d7e", 0, false},
		{"5b2cc7f0-9a3e-4e7b-cc1d-2f3a4b5c6d7e", 0, false},
	}
	for _, c := range cases {
		got, ok := uuidExtractVersion(mustUUID(t, c.s))
		if ok != c.ok || (ok && got != c.want) {
			t.Errorf("uuidExtractVersion(%s) = (%d,%v), want (%d,%v)", c.s, got, ok, c.want, c.ok)
		}
	}
}

func TestUUIDExtractTimestamp(t *testing.T) {
	// micros oracle-verified against PG 18; v1/v7 only (v6/v4/non-RFC → NULL).
	cases := []struct {
		s    string
		want int64
		ok   bool
	}{
		{"0190b6f7-8000-7000-8000-000000000000", 1_721_056_591_872_000, true},
		{"c232ab00-9414-11ec-b3c8-9e6bdeced846", 1_645_557_742_000_000, true},
		{"c232ab07-9414-11ec-b3c8-9e6bdeced846", 1_645_557_742_000_000, true}, // sub-µs truncated
		{"1ec9414c-232a-6b00-b3c8-9e6bdeced846", 0, false},                    // v6 → NULL (PG 18)
		{"5b2cc7f0-9a3e-4e7b-8c1d-2f3a4b5c6d7e", 0, false},                    // v4 → NULL
		{"00000000-0000-0000-0000-000000000000", 0, false},                    // nil → NULL
	}
	for _, c := range cases {
		got, ok := uuidExtractTimestampMicros(mustUUID(t, c.s))
		if ok != c.ok || (ok && got != c.want) {
			t.Errorf("uuidExtractTimestampMicros(%s) = (%d,%v), want (%d,%v)", c.s, got, ok, c.want, c.ok)
		}
	}
}
