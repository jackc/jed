package abide

// Cross-check: the hand-written type and error constants in the Go core must match
// the canonical spec data tables (CLAUDE.md §5). The spec TOML is parsed with a tiny
// purpose-built reader (no third-party dependency — the Go core is pure-Go and the
// spec tables are simple), so this stays a test-time-only concern. If the spec
// changes and the core doesn't (or vice versa), this fails.

import (
	"os"
	"path/filepath"
	"testing"
)

func specPath(t *testing.T, rel string) string {
	t.Helper()
	dir, _ := os.Getwd()
	for {
		p := filepath.Join(dir, "spec", rel)
		if _, err := os.Stat(p); err == nil {
			return p
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatalf("could not locate spec/%s from %s", rel, dir)
		}
		dir = parent
	}
}

func TestScalarTypesMatchSpec(t *testing.T) {
	tables := readTomlTables(t, specPath(t, "types/scalars.toml"), "type")
	if len(tables) != 3 {
		t.Fatalf("expected 3 scalar types, got %d", len(tables))
	}
	for _, row := range tables {
		id := row.str("id")
		st, ok := ScalarTypeFromName(id)
		if !ok {
			t.Fatalf("unknown type id %q", id)
		}
		if st.CanonicalName() != id {
			t.Errorf("%s: canonical name mismatch", id)
		}
		if got, want := int64(st.WidthBytes()*8), row.int("bits"); got != want {
			t.Errorf("%s: bits got %d want %d", id, got, want)
		}
		if got, want := st.Min(), row.int("min"); got != want {
			t.Errorf("%s: min got %d want %d", id, got, want)
		}
		if got, want := st.Max(), row.int("max"); got != want {
			t.Errorf("%s: max got %d want %d", id, got, want)
		}
		if got, want := int64(st.Rank()), row.int("rank"); got != want {
			t.Errorf("%s: rank got %d want %d", id, got, want)
		}
		for _, alias := range row.strs("aliases") {
			if a, ok := ScalarTypeFromName(alias); !ok || a != st {
				t.Errorf("alias %q should resolve to %s", alias, id)
			}
		}
	}
}

func TestErrorCodesAreRegistered(t *testing.T) {
	tables := readTomlTables(t, specPath(t, "errors/registry.toml"), "error")
	codes := map[string]string{} // code -> name
	for _, row := range tables {
		codes[row.str("code")] = row.str("name")
	}
	for _, st := range []SqlState{
		NumericValueOutOfRange, SyntaxError, UndefinedTable,
		UndefinedColumn, DatatypeMismatch, FeatureNotSupported,
	} {
		if _, ok := codes[st.Code()]; !ok {
			t.Errorf("code %s missing from registry", st.Code())
		}
	}
	if codes["22003"] != "numeric_value_out_of_range" {
		t.Errorf("22003 name mismatch: %q", codes["22003"])
	}
	if NumericValueOutOfRange.Code() != "22003" {
		t.Errorf("NumericValueOutOfRange code mismatch")
	}
}
