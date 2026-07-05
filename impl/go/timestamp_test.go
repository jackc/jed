package jed

// Cross-check: the Go timestamp parser/renderer reproduces the byte-exact vectors in
// spec/encoding/timestamps.toml (CLAUDE.md §8) — identical to the Rust/TS cores. The tiny
// test TOML reader does not handle the nested `cases = [ ... ]` arrays, so this dedicated
// scanner walks the file (like encoding_cases_test.go). Test-only.

import (
	"os"
	"strconv"
	"strings"
	"testing"
)

type tsCase struct {
	section string // "parse" | "parse_error" | "render"
	typ     string
	fields  map[string]string // raw values (strings keep their quotes)
}

func readTimestampCases(t *testing.T, path string) []tsCase {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	var out []tsCase
	section, typ := "", ""
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		switch {
		case line == "[[parse]]":
			section, typ = "parse", ""
		case line == "[[parse_error]]":
			section, typ = "parse_error", ""
		case line == "[[render]]":
			section, typ = "render", ""
		case strings.HasPrefix(line, "type ="):
			typ = unquote(strings.TrimSpace(stripComment(strings.TrimPrefix(line, "type ="))))
		case strings.HasPrefix(line, "{"):
			if section != "" && typ != "" {
				out = append(out, tsCase{section: section, typ: typ, fields: parseInlineFields(line)})
			}
		}
	}
	return out
}

// parseInlineFields pulls the `key = value` pairs from one `{ ... },` inline-table line.
// The vector values never contain a comma, so a plain split is sufficient (test data).
func parseInlineFields(line string) map[string]string {
	inner := line
	if i := strings.Index(inner, "{"); i >= 0 {
		inner = inner[i+1:]
	}
	if i := strings.Index(inner, "}"); i >= 0 {
		inner = inner[:i]
	}
	out := map[string]string{}
	for _, part := range strings.Split(inner, ",") {
		k, v, ok := strings.Cut(part, "=")
		if ok {
			out[strings.TrimSpace(k)] = strings.TrimSpace(v)
		}
	}
	return out
}

func tsParse(typ, in string) (int64, error) {
	if typ == "timestamp" {
		return parseTimestamp(in)
	}
	return parseTimestamptz(in)
}

func tsRender(typ string, m int64) string {
	if typ == "timestamp" {
		return renderTimestamp(m)
	}
	return renderTimestamptz(m)
}

func TestTimestampVectors(t *testing.T) {
	t.Parallel()
	cases := readTimestampCases(t, specPath(t, "encoding/timestamps.toml"))
	if len(cases) == 0 {
		t.Fatal("no timestamp vectors parsed")
	}
	for _, c := range cases {
		switch c.section {
		case "parse":
			in := unquote(c.fields["input"])
			want, _ := strconv.ParseInt(c.fields["micros"], 10, 64)
			got, err := tsParse(c.typ, in)
			switch {
			case err != nil:
				t.Errorf("%s parse %q: unexpected error %v", c.typ, in, err)
			case got != want:
				t.Errorf("%s parse %q = %d, want %d", c.typ, in, got, want)
			}
		case "parse_error":
			in := unquote(c.fields["input"])
			want := unquote(c.fields["error"])
			_, err := tsParse(c.typ, in)
			ee, ok := err.(*EngineError)
			if !ok {
				t.Errorf("%s parse %q: expected error %s, got %v", c.typ, in, want, err)
			} else if ee.Code() != want {
				t.Errorf("%s parse %q: error %s, want %s", c.typ, in, ee.Code(), want)
			}
		case "render":
			m, _ := strconv.ParseInt(c.fields["micros"], 10, 64)
			want := unquote(c.fields["text"])
			if got := tsRender(c.typ, m); got != want {
				t.Errorf("%s render %d = %q, want %q", c.typ, m, got, want)
			}
		}
	}
}
