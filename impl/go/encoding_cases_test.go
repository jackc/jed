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
	kind      string // "bare" | "nullable" | "descending"
	typ       string
	value     int64
	strValue  string // a quoted value (uuid's canonical string); empty for integer cases
	boolValue bool   // an unquoted true/false value (boolean's bool-byte cases)
	isNull    bool
	bytes     string
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

// nullableUUIDBytes builds the nullable key slot for a uuid case: 0x01 for NULL, else
// 0x00 + the 16 raw bytes (encoding.md §2.2/§2.7).
func nullableUUIDBytes(c encCase) []byte {
	if c.isNull {
		return []byte{0x01}
	}
	b, _ := ParseUUID(c.strValue)
	return append([]byte{0x00}, b...)
}

// nullableBoolBytes builds the nullable key slot for a boolean case: 0x01 for NULL, else
// 0x00 + the 1-byte bool-byte (encoding.md §2.2/§2.9).
func nullableBoolBytes(c encCase) []byte {
	if c.isNull {
		return []byte{0x01}
	}
	return append([]byte{0x00}, EncodeBool(c.boolValue)...)
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
			// A quoted value is a uuid's canonical string; an unquoted true/false is a
			// boolean's bool-byte value; any other unquoted value is an integer.
			switch {
			case strings.HasPrefix(v, "\""):
				c.strValue = unquote(v)
			case v == "true" || v == "false":
				c.boolValue = v == "true"
			default:
				n, err := strconv.ParseInt(v, 10, 64)
				if err != nil {
					return encCase{}, false
				}
				c.value = n
			}
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
