package jed

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

	// The storable scalar types are exactly the three integers; each maps to a
	// ScalarType with matching width/range/rank/encoding (CLAUDE.md §5 cross-check).
	integers := 0
	var boolean *tomlRow
	for i := range tables {
		row := tables[i]
		if row.str("family") != "integer" {
			if row.str("id") == "boolean" {
				boolean = &tables[i]
			}
			continue
		}
		integers++
		id := row.str("id")
		st, ok := ScalarTypeFromName(id)
		if !ok {
			t.Fatalf("unknown type id %q", id)
		}
		if st.CanonicalName() != id {
			t.Errorf("%s: canonical name mismatch", id)
		}
		if !row.boolVal("storable") {
			t.Errorf("%s: should be storable", id)
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
	if integers != 3 {
		t.Fatalf("expected 3 storable integer scalar types, got %d", integers)
	}

	// boolean is the first non-integer scalar: expression-only (storable = false), so it
	// is NOT a column ScalarType, only a recognized non-storable type name.
	if boolean == nil {
		t.Fatal("boolean type missing from scalars.toml")
	}
	if boolean.str("family") != "boolean" {
		t.Errorf("boolean: family mismatch")
	}
	if boolean.boolVal("storable") {
		t.Errorf("boolean must not be storable this slice")
	}
	if _, ok := ScalarTypeFromName("boolean"); ok {
		t.Errorf("boolean is not a storable column type")
	}
	if _, ok := ScalarTypeFromName("bool"); ok {
		t.Errorf("bool is not a storable column type")
	}
	if !IsBooleanTypeName("boolean") || !IsBooleanTypeName("BOOL") {
		t.Errorf("boolean type name should be recognized (case-insensitively)")
	}
}

func TestErrorCodesAreRegistered(t *testing.T) {
	tables := readTomlTables(t, specPath(t, "errors/registry.toml"), "error")
	codes := map[string]string{} // code -> name
	for _, row := range tables {
		codes[row.str("code")] = row.str("name")
	}
	for _, st := range []SqlState{
		NumericValueOutOfRange, DivisionByZero,
		InvalidRowCountInLimitClause, InvalidRowCountInOffsetClause,
		NotNullViolation, UniqueViolation,
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
	// Operators are overloaded across operand families (one row per (name, arg_families)
	// — e.g. `eq` for integer and for text), so match on the full signature, not the name.
	find := func(name string, fams []string) (OperatorDesc, bool) {
		for _, d := range Operators {
			if d.Name == name && strings.Join(d.ArgFamilies, ",") == strings.Join(fams, ",") {
				return d, true
			}
		}
		return OperatorDesc{}, false
	}
	for _, row := range rows {
		name := row.str("name")
		desc, ok := find(name, row.strs("arg_families"))
		if !ok {
			t.Fatalf("generated table missing operator %q %v", name, row.strs("arg_families"))
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
		wantPrec := int64(0)
		if row.has("precedence") {
			wantPrec = row.int("precedence")
		}
		if int64(desc.Precedence) != wantPrec {
			t.Errorf("%s: precedence got %d want %d", name, desc.Precedence, wantPrec)
		}
		if strings.Join(desc.ArgFamilies, ",") != strings.Join(row.strs("arg_families"), ",") {
			t.Errorf("%s: arg_families mismatch", name)
		}
		if strings.Join(desc.Errors, ",") != strings.Join(row.strs("errors"), ",") {
			t.Errorf("%s: errors mismatch", name)
		}
		if row.has("symbol") {
			if desc.Symbol != row.str("symbol") {
				t.Errorf("%s: symbol got %q want %q", name, desc.Symbol, row.str("symbol"))
			}
		} else if desc.Symbol != "" {
			t.Errorf("%s: expected empty symbol, got %q", name, desc.Symbol)
		}
	}
}

func TestCostScheduleMatchesSpec(t *testing.T) {
	// The generated cost schedule (codegen middle path, CLAUDE.md §5/§13) must match the
	// canonical schedule.toml weight-for-weight. Cost is a cross-core contract (§8):
	// every core reads these weights.
	rows := readTomlTables(t, specPath(t, "cost/schedule.toml"), "unit")
	if len(rows) != 3 {
		t.Fatalf("expected the three phase-1 cost units, got %d", len(rows))
	}
	// Every unit id maps to a field on Costs; a new unit forces this cross-check to be
	// updated (so a core cannot silently ignore a unit the schedule adds).
	weight := func(id string) int64 {
		switch id {
		case "storage_row_read":
			return Costs.StorageRowRead
		case "row_produced":
			return Costs.RowProduced
		case "operator_eval":
			return Costs.OperatorEval
		default:
			t.Fatalf("cost unit %q has no Costs field — update this cross-check", id)
			return 0
		}
	}
	for _, row := range rows {
		id := row.str("id")
		if got, want := weight(id), row.int("weight"); got != want {
			t.Errorf("%s: weight got %d want %d", id, got, want)
		}
	}
}
