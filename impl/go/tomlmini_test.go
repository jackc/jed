package abide

// A deliberately tiny TOML reader for the cross-check tests ONLY. It understands
// just enough of the spec tables' shape — arrays of tables (`[[type]]`), scalar
// key = value pairs (string / integer / bool), inline string arrays, and one level
// of inline table for `encoding = { ... }` (read via dotted access). It is NOT a
// general TOML parser and lives in _test.go so it is never built into the engine
// (CLAUDE.md §5: TOML is test-time only). Keeping it dependency-free preserves the
// pure-Go, no-FFI rule (CLAUDE.md §2) without vendoring a TOML library.

import (
	"os"
	"strconv"
	"strings"
	"testing"
)

type tomlRow struct {
	t    *testing.T
	vals map[string]string
	// arrVals holds inline-array values (e.g. aliases = ["a", "b"]).
	arrVals map[string][]string
}

func (r tomlRow) str(key string) string {
	v, ok := r.vals[key]
	if !ok {
		r.t.Fatalf("missing key %q", key)
	}
	return v
}

func (r tomlRow) int(key string) int64 {
	n, err := strconv.ParseInt(r.str(key), 10, 64)
	if err != nil {
		r.t.Fatalf("key %q not an integer: %v", key, err)
	}
	return n
}

func (r tomlRow) strs(key string) []string {
	return r.arrVals[key]
}

// has reports whether a scalar key is present.
func (r tomlRow) has(key string) bool {
	_, ok := r.vals[key]
	return ok
}

// boolVal reads a TOML boolean (stored as the literal "true"/"false").
func (r tomlRow) boolVal(key string) bool {
	return r.str(key) == "true"
}

// readTomlTables parses every `[[section]]` array-of-tables entry from a TOML file.
// Only keys directly under each entry are captured (sufficient for the spec tables).
func readTomlTables(t *testing.T, path, section string) []tomlRow {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	var rows []tomlRow
	var cur *tomlRow
	header := "[[" + section + "]]"

	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if line == header {
			rows = append(rows, tomlRow{t: t, vals: map[string]string{}, arrVals: map[string][]string{}})
			cur = &rows[len(rows)-1]
			continue
		}
		if strings.HasPrefix(line, "[[") || strings.HasPrefix(line, "[") {
			cur = nil // a different section starts
			continue
		}
		if cur == nil {
			continue
		}
		key, val, ok := strings.Cut(line, "=")
		if !ok {
			continue
		}
		key = strings.TrimSpace(key)
		val = strings.TrimSpace(stripComment(val))
		switch {
		case strings.HasPrefix(val, "["):
			cur.arrVals[key] = parseInlineStringArray(val)
		case strings.HasPrefix(val, "{"):
			// inline table (e.g. encoding = { method = "...", width_bytes = 2 });
			// expose nested keys via dotted access "encoding.width_bytes".
			for k, v := range parseInlineTable(val) {
				cur.vals[key+"."+k] = v
			}
		default:
			cur.vals[key] = unquote(val)
		}
	}
	return rows
}

func stripComment(s string) string {
	// Remove a trailing ` # ...` comment that is outside quotes/brackets. The spec
	// tables never put '#' inside a string value, so a simple scan suffices.
	inStr := false
	for i := 0; i < len(s); i++ {
		switch s[i] {
		case '"':
			inStr = !inStr
		case '#':
			if !inStr {
				return s[:i]
			}
		}
	}
	return s
}

func unquote(s string) string {
	s = strings.TrimSpace(s)
	if len(s) >= 2 && s[0] == '"' && s[len(s)-1] == '"' {
		return s[1 : len(s)-1]
	}
	return s
}

func parseInlineStringArray(s string) []string {
	s = strings.TrimSpace(s)
	s = strings.TrimPrefix(s, "[")
	s = strings.TrimSuffix(s, "]")
	var out []string
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		out = append(out, unquote(part))
	}
	return out
}

func parseInlineTable(s string) map[string]string {
	s = strings.TrimSpace(s)
	s = strings.TrimPrefix(s, "{")
	s = strings.TrimSuffix(s, "}")
	out := map[string]string{}
	for _, part := range strings.Split(s, ",") {
		k, v, ok := strings.Cut(part, "=")
		if !ok {
			continue
		}
		out[strings.TrimSpace(k)] = unquote(strings.TrimSpace(v))
	}
	return out
}
