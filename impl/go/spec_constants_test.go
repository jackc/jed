package abide

// Cross-check: the hand-written type and error constants in the Go core must match
// the canonical spec data tables (CLAUDE.md §5). The spec TOML is parsed with a tiny
// purpose-built reader (no third-party dependency — the Go core is pure-Go and the
// spec tables are simple), so this stays a test-time-only concern. If the spec
// changes and the core doesn't (or vice versa), this fails.

import (
	"os"
	"path/filepath"
	"strings"
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
		NumericValueOutOfRange, NotNullViolation, UniqueViolation,
		SyntaxError, UndefinedTable, UndefinedColumn, UndefinedObject,
		DatatypeMismatch, DuplicateTable, DuplicateColumn,
		InvalidTableDefinition, FeatureNotSupported, DataCorrupted,
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

func TestOperatorsMatchSpec(t *testing.T) {
	// The generated operator descriptor table (codegen middle path, CLAUDE.md §5) must
	// match the canonical catalog field-for-field.
	rows := readTomlTables(t, specPath(t, "functions/catalog.toml"), "operator")
	if len(rows) != len(Operators) {
		t.Fatalf("operator count: spec %d, generated %d", len(rows), len(Operators))
	}
	byName := map[string]OperatorDesc{}
	for _, d := range Operators {
		byName[d.Name] = d
	}
	for _, row := range rows {
		name := row.str("name")
		desc, ok := byName[name]
		if !ok {
			t.Fatalf("generated table missing operator %q", name)
		}
		if desc.Kind != row.str("kind") {
			t.Errorf("%s: kind got %q want %q", name, desc.Kind, row.str("kind"))
		}
		if int64(desc.Arity) != row.int("arity") {
			t.Errorf("%s: arity mismatch", name)
		}
		if desc.ArgResolution != row.str("arg_resolution") {
			t.Errorf("%s: arg_resolution mismatch", name)
		}
		if desc.Result != row.str("result") {
			t.Errorf("%s: result mismatch", name)
		}
		if desc.Null != row.str("null") {
			t.Errorf("%s: null mismatch", name)
		}
		if strings.Join(desc.ArgFamilies, ",") != strings.Join(row.strs("arg_families"), ",") {
			t.Errorf("%s: arg_families mismatch", name)
		}
		if strings.Join(desc.Errors, ",") != strings.Join(row.strs("errors"), ",") {
			t.Errorf("%s: errors mismatch", name)
		}
		if row.str("kind") == "comparison" {
			if desc.Symbol != row.str("symbol") {
				t.Errorf("%s: symbol mismatch", name)
			}
		} else if desc.Symbol != "" {
			t.Errorf("%s: expected empty symbol, got %q", name, desc.Symbol)
		}
	}
}
