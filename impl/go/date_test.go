package jed

// Cross-check: the Go date parser/renderer reproduces the byte-exact vectors in
// spec/encoding/dates.toml (CLAUDE.md §8) — identical to the Rust/TS cores. Reuses the
// timestamp vector scanner (readTimestampCases / unquote / parseInlineFields). Test-only.

import (
	"strconv"
	"testing"
)

func TestDateVectors(t *testing.T) {
	cases := readTimestampCases(t, specPath(t, "encoding/dates.toml"))
	if len(cases) == 0 {
		t.Fatal("no date vectors parsed")
	}
	for _, c := range cases {
		if c.typ != "date" {
			t.Fatalf("unexpected vector type %q", c.typ)
		}
		switch c.section {
		case "parse":
			in := unquote(c.fields["input"])
			want, _ := strconv.ParseInt(c.fields["days"], 10, 64)
			got, err := ParseDate(in)
			switch {
			case err != nil:
				t.Errorf("parse %q: unexpected error %v", in, err)
			case int64(got) != want:
				t.Errorf("parse %q = %d, want %d", in, got, want)
			}
		case "parse_error":
			in := unquote(c.fields["input"])
			want := unquote(c.fields["error"])
			_, err := ParseDate(in)
			ee, ok := err.(*EngineError)
			if !ok {
				t.Errorf("parse %q: expected error %s, got %v", in, want, err)
			} else if ee.Code() != want {
				t.Errorf("parse %q: error %s, want %s", in, ee.Code(), want)
			}
		case "render":
			d, _ := strconv.ParseInt(c.fields["days"], 10, 64)
			want := unquote(c.fields["text"])
			if got := RenderDate(int32(d)); got != want {
				t.Errorf("render %d = %q, want %q", d, got, want)
			}
		}
	}
}
