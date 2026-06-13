package jed

// Cross-check: the Go interval parser/renderer reproduces the byte-exact vectors in
// spec/encoding/intervals.toml (CLAUDE.md §8) — identical to the Rust/TS cores. Reuses the tiny
// inline-table scanner helpers from timestamp_test.go (same package). Test-only.

import (
	"os"
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
			got, err := ParseInterval(in)
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
			_, err := ParseInterval(in)
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
			if got := RenderInterval(iv); got != want {
				t.Errorf("render {%d,%d,%d} = %q, want %q", months, days, micros, got, want)
			}
		}
	}
}

func TestIntervalSpanCanonical(t *testing.T) {
	oneMonth, _ := ParseInterval("1 mon")
	thirtyDays, _ := ParseInterval("30 days")
	hours, _ := ParseInterval("720:00:00")
	if oneMonth.SpanCmp(thirtyDays) != 0 || oneMonth.SpanCmp(hours) != 0 {
		t.Errorf("'1 mon', '30 days', '720:00:00' should be span-equal")
	}
	if RenderInterval(oneMonth) == RenderInterval(thirtyDays) {
		t.Errorf("span-equal intervals should still render distinctly")
	}
	day, _ := ParseInterval("1 day")
	twoDays, _ := ParseInterval("2 days")
	negDay, _ := ParseInterval("-1 day")
	if day.SpanCmp(twoDays) >= 0 || negDay.SpanCmp(day) >= 0 {
		t.Errorf("span ordering wrong")
	}
}
