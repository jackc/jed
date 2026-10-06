package jed

// Cross-check the query-memory logical size schedule against the shared vectors in
// spec/cost/memory_sizes.toml (spec/design/memory.md §3). Every core runs the same vectors, so the
// measurement — and therefore a 54P05 threshold — is cross-core identical.

import (
	"os"
	"strconv"
	"strings"
	"testing"
)

// memoryVector is one [[vector]] entry of memory_sizes.toml.
type memoryVector struct {
	sql     string
	setup   []string
	measure string
	bytes   int64
}

func TestMemorySizeVectorsMatchSpec(t *testing.T) {
	vectors := readMemoryVectors(t, specPath(t, "cost/memory_sizes.toml"))
	if len(vectors) == 0 {
		t.Fatal("no [[vector]] entries")
	}
	for _, v := range vectors {
		s := dbWith(t, v.setup...)
		out, err := queryOutcome(s, v.sql, nil)
		if err != nil {
			t.Fatalf("%s: %v", v.sql, err)
		}
		if len(out.Rows) != 1 {
			t.Fatalf("%s: want one row, got %d", v.sql, len(out.Rows))
		}
		var got int64
		switch v.measure {
		case "value":
			if len(out.Rows[0]) != 1 {
				t.Fatalf("%s: want one column, got %d", v.sql, len(out.Rows[0]))
			}
			got = memValueBytes(out.Rows[0][0])
		case "row":
			got = memRowBytes(out.Rows[0])
		default:
			t.Fatalf("%s: unknown measure %q", v.sql, v.measure)
		}
		if got != v.bytes {
			t.Errorf("%s: measured %d bytes, spec says %d", v.sql, got, v.bytes)
		}
	}
}

// readMemoryVectors parses memory_sizes.toml's [[vector]] tables. The shared tomlmini reader cannot
// split a `setup` array whose strings contain commas or unescape a basic string's `\"`, so this
// reads just the shape the file uses: `key = "basic string"`, `key = ["s", …]`, `key = integer`.
func readMemoryVectors(t *testing.T, path string) []memoryVector {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	var out []memoryVector
	var cur *memoryVector
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if line == "[[vector]]" {
			out = append(out, memoryVector{})
			cur = &out[len(out)-1]
			continue
		}
		key, val, ok := strings.Cut(line, "=")
		if !ok {
			continue
		}
		key, val = strings.TrimSpace(key), strings.TrimSpace(val)
		if key == "schema_version" {
			if val != "1" {
				t.Fatalf("schema_version %s, want 1", val)
			}
			continue
		}
		if cur == nil {
			continue
		}
		switch key {
		case "sql":
			cur.sql, _ = tomlBasicString(t, val)
		case "measure":
			cur.measure, _ = tomlBasicString(t, val)
		case "bytes":
			n, err := strconv.ParseInt(stripComment(val), 10, 64)
			if err != nil {
				t.Fatalf("bytes %q: %v", val, err)
			}
			cur.bytes = n
		case "setup":
			rest := strings.TrimSpace(strings.TrimPrefix(val, "["))
			for !strings.HasPrefix(rest, "]") {
				s, n := tomlBasicString(t, rest)
				cur.setup = append(cur.setup, s)
				rest = strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(rest[n:]), ","))
			}
		}
	}
	return out
}

// tomlBasicString decodes the TOML basic string at the start of s (only the `\"` and `\\` escapes
// the vectors use), returning it and the number of bytes consumed.
func tomlBasicString(t *testing.T, s string) (string, int) {
	t.Helper()
	if !strings.HasPrefix(s, `"`) {
		t.Fatalf("expected a basic string: %s", s)
	}
	var b strings.Builder
	for i := 1; i < len(s); i++ {
		switch c := s[i]; c {
		case '\\':
			i++
			if i >= len(s) || (s[i] != '"' && s[i] != '\\') {
				t.Fatalf("unsupported escape in %s", s)
			}
			b.WriteByte(s[i])
		case '"':
			return b.String(), i + 1
		default:
			b.WriteByte(c)
		}
	}
	t.Fatalf("unterminated string: %s", s)
	return "", 0
}
