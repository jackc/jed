package jed

// Cross-check: the Go interval parser/renderer reproduces the byte-exact vectors in
// spec/encoding/intervals.toml (CLAUDE.md §8) — identical to the Rust/TS cores. Reuses the tiny
// inline-table scanner helpers from timestamp_test.go (same package). Test-only.

import (
	"bytes"
	"encoding/hex"
	"os"
	"sort"
	"strconv"
	"strings"
	"testing"
)

type ivCase struct {
	section string // "parse" | "parse_error" | "render"
	fields  map[string]string
}

func readIntervalCases(t *testing.T, path string) []ivCase {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	var out []ivCase
	section := ""
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		switch {
		case line == "[[parse]]":
			section = "parse"
		case line == "[[parse_error]]":
			section = "parse_error"
		case line == "[[render]]":
			section = "render"
		case strings.HasPrefix(line, "{") && section != "":
			out = append(out, ivCase{section: section, fields: parseInlineFields(line)})
		}
	}
	return out
}

func TestIntervalVectors(t *testing.T) {
	t.Parallel()
	cases := readIntervalCases(t, specPath(t, "encoding/intervals.toml"))
	if len(cases) == 0 {
		t.Fatal("no interval vectors parsed")
	}
	for _, c := range cases {
		switch c.section {
		case "parse":
			in := unquote(c.fields["input"])
			months, _ := strconv.ParseInt(c.fields["months"], 10, 32)
			days, _ := strconv.ParseInt(c.fields["days"], 10, 32)
			micros, _ := strconv.ParseInt(c.fields["micros"], 10, 64)
			got, err := parseInterval(in)
			switch {
			case err != nil:
				t.Errorf("parse %q: unexpected error %v", in, err)
			case got.Months != int32(months) || got.Days != int32(days) || got.Micros != micros:
				t.Errorf("parse %q = {%d,%d,%d}, want {%d,%d,%d}", in,
					got.Months, got.Days, got.Micros, months, days, micros)
			}
		case "parse_error":
			in := unquote(c.fields["input"])
			want := unquote(c.fields["error"])
			_, err := parseInterval(in)
			ee, ok := err.(*EngineError)
			if !ok {
				t.Errorf("parse %q: expected error %s, got %v", in, want, err)
			} else if ee.Code() != want {
				t.Errorf("parse %q: error %s, want %s", in, ee.Code(), want)
			}
		case "render":
			months, _ := strconv.ParseInt(c.fields["months"], 10, 32)
			days, _ := strconv.ParseInt(c.fields["days"], 10, 32)
			micros, _ := strconv.ParseInt(c.fields["micros"], 10, 64)
			want := unquote(c.fields["text"])
			iv := Interval{Months: int32(months), Days: int32(days), Micros: micros}
			if got := renderInterval(iv); got != want {
				t.Errorf("render {%d,%d,%d} = %q, want %q", months, days, micros, got, want)
			}
		}
	}
}

func TestIntervalSpanCanonical(t *testing.T) {
	t.Parallel()
	oneMonth, _ := parseInterval("1 mon")
	thirtyDays, _ := parseInterval("30 days")
	hours, _ := parseInterval("720:00:00")
	if oneMonth.SpanCmp(thirtyDays) != 0 || oneMonth.SpanCmp(hours) != 0 {
		t.Errorf("'1 mon', '30 days', '720:00:00' should be span-equal")
	}
	if renderInterval(oneMonth) == renderInterval(thirtyDays) {
		t.Errorf("span-equal intervals should still render distinctly")
	}
	day, _ := parseInterval("1 day")
	twoDays, _ := parseInterval("2 days")
	negDay, _ := parseInterval("-1 day")
	if day.SpanCmp(twoDays) >= 0 || negDay.SpanCmp(day) >= 0 {
		t.Errorf("span ordering wrong")
	}
}

// TestIntervalEncodeKey verifies the order-preserving KEY body (interval-span-i128,
// encoding.md §2.10): the 16-byte i128 span (bias 2^127 + big-endian). Sorting by EncodeKey must
// equal span order; span-equal intervals share a key (the "equal but not identical" UNIQUE
// wrinkle, decimal's 1.5/1.50); byte-exact against the canonical vectors (interval.toml).
func TestIntervalEncodeKey(t *testing.T) {
	t.Parallel()
	iv := func(m, d int32, u int64) Interval { return Interval{Months: m, Days: d, Micros: u} }
	// Ascending by span — sorting by key must reproduce this order (sign boundary, zero, ±µs).
	ordered := []Interval{
		iv(-1200, 0, 0), iv(-1, 0, 0), iv(0, -1, 0), iv(0, 0, -1_000_000), iv(0, 0, -1),
		iv(0, 0, 0), iv(0, 0, 1), iv(0, 0, 1_000_000), iv(0, 1, 0), iv(1, 0, 0), iv(1200, 0, 0),
	}
	byKey := append([]Interval(nil), ordered...)
	sort.Slice(byKey, func(i, j int) bool {
		return bytes.Compare(byKey[i].EncodeKey(), byKey[j].EncodeKey()) < 0
	})
	for i := range ordered {
		if byKey[i].SpanCmp(ordered[i]) != 0 {
			t.Fatalf("encode_key order must equal span order at %d", i)
		}
	}
	// Span-equal intervals share a key (1 mon == 30 days == 720:00:00) — the UNIQUE wrinkle.
	if !bytes.Equal(iv(1, 0, 0).EncodeKey(), iv(0, 30, 0).EncodeKey()) {
		t.Errorf("1 mon / 30 days keys must coincide")
	}
	if !bytes.Equal(iv(1, 0, 0).EncodeKey(), iv(0, 0, 30*86_400_000_000).EncodeKey()) {
		t.Errorf("1 mon / 720:00:00 keys must coincide")
	}
	// Byte-exact canonical vectors (the §2.10 worked-bytes table).
	exact := []struct {
		iv   Interval
		want string
	}{
		{iv(0, 0, 0), "80000000000000000000000000000000"},
		{iv(0, 0, 1), "80000000000000000000000000000001"},
		{iv(0, 0, -1), "7fffffffffffffffffffffffffffffff"},
		{iv(0, 1, 0), "8000000000000000000000141dd76000"},
		{iv(1, 0, 0), "80000000000000000000025b7f3d4000"},
		{iv(0, -1, 0), "7fffffffffffffffffffffebe228a000"},
	}
	for _, e := range exact {
		if got := hex.EncodeToString(e.iv.EncodeKey()); got != e.want {
			t.Errorf("EncodeKey(%+v) = %s, want %s", e.iv, got, e.want)
		}
	}
}
