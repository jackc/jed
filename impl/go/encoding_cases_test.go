package jed

// Line scanner for the encoding fixtures' case rows (spec/encoding/integers.toml).
// The cases are inline tables inside a multi-line `cases = [ ... ]` array under each
// `[[bare]]` / `[[nullable]]` / `[[descending]]` group. The tiny test TOML reader
// captures scalar keys but not these nested inline-table arrays, so this dedicated
// scanner walks the file once and pulls out (kind, type, value|null, bytes) tuples.
// Test-only (CLAUDE.md §5).

import (
	"os"
	"strconv"
	"strings"
	"testing"
)

type encCase struct {
	kind   string // "bare" | "nullable" | "descending"
	typ    string
	value  int64
	isNull bool
	bytes  string
}

func readEncodingCases(t *testing.T, path string) []encCase {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	var out []encCase
	kind := ""
	typ := ""
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		switch {
		case line == "[[bare]]":
			kind, typ = "bare", ""
		case line == "[[nullable]]":
			kind, typ = "nullable", ""
		case line == "[[descending]]":
			kind, typ = "descending", ""
		case strings.HasPrefix(line, "type ="):
			typ = unquote(strings.TrimSpace(stripComment(strings.TrimPrefix(line, "type ="))))
		case strings.HasPrefix(line, "{"):
			if c, ok := parseEncCaseLine(line, kind, typ); ok {
				out = append(out, c)
			}
		}
	}
	return out
}

// parseEncCaseLine parses one `{ value = N, bytes = "hex" },` or
// `{ null = true, bytes = "hex" },` inline-table line.
func parseEncCaseLine(line, kind, typ string) (encCase, bool) {
	if kind == "" || typ == "" {
		return encCase{}, false
	}
	inner := line
	if i := strings.Index(inner, "{"); i >= 0 {
		inner = inner[i+1:]
	}
	if i := strings.Index(inner, "}"); i >= 0 {
		inner = inner[:i]
	}
	c := encCase{kind: kind, typ: typ}
	for _, part := range strings.Split(inner, ",") {
		k, v, ok := strings.Cut(part, "=")
		if !ok {
			continue
		}
		k = strings.TrimSpace(k)
		v = strings.TrimSpace(v)
		switch k {
		case "value":
			n, err := strconv.ParseInt(v, 10, 64)
			if err != nil {
				return encCase{}, false
			}
			c.value = n
		case "null":
			c.isNull = v == "true"
		case "bytes":
			c.bytes = unquote(v)
		}
	}
	if c.bytes == "" {
		return encCase{}, false
	}
	return c, true
}
