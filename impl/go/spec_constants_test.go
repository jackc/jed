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

	// boolean is a storable non-integer scalar (storable = true): it resolves to a column
	// ScalarType, canonical-names to "boolean", and its aliases resolve. It has no integer
	// fields (bits/min/max/rank), so those accessors are not exercised here.
	if boolean == nil {
		t.Fatal("boolean type missing from scalars.toml")
	}
	if boolean.str("family") != "boolean" {
		t.Errorf("boolean: family mismatch")
	}
	if !boolean.boolVal("storable") {
		t.Errorf("boolean must be storable this slice")
	}
	boolTy, ok := ScalarTypeFromName("boolean")
	if !ok {
		t.Fatalf("boolean should resolve to a ScalarType")
	}
	if boolTy.CanonicalName() != "boolean" {
		t.Errorf("boolean: canonical name mismatch")
	}
	for _, alias := range boolean.strs("aliases") {
		if a, ok := ScalarTypeFromName(alias); !ok || a != boolTy {
			t.Errorf("alias %q should resolve to boolean", alias)
		}
	}

	// decimal: storable, the decimal family; aliases resolve; the precision/scale caps match
	// the decimal module's constants (a cross-core contract, spec/design/decimal.md §2).
	var decimal *tomlRow
	var text *tomlRow
	for i := range tables {
		switch tables[i].str("id") {
		case "decimal":
			decimal = &tables[i]
		case "text":
			text = &tables[i]
		}
	}
	if text == nil || !text.boolVal("storable") || mustType(t, "text") != Text {
		t.Error("text type missing or not storable")
	}
	if decimal == nil {
		t.Fatal("decimal type missing from scalars.toml")
	}
	if decimal.str("family") != "decimal" {
		t.Errorf("decimal: family mismatch")
	}
	if !decimal.boolVal("storable") {
		t.Errorf("decimal must be storable")
	}
	if DecimalType.CanonicalName() != "decimal" {
		t.Errorf("decimal canonical name mismatch")
	}
	for _, name := range []string{"decimal", "numeric", "dec"} {
		if got, ok := ScalarTypeFromName(name); !ok || got != DecimalType {
			t.Errorf("%q should resolve to decimal", name)
		}
	}
	if got, want := decimal.int("max_precision"), int64(MaxPrecision); got != want {
		t.Errorf("max_precision: spec %d, module %d", got, want)
	}
	if got, want := decimal.int("max_scale"), int64(MaxScale); got != want {
		t.Errorf("max_scale: spec %d, module %d", got, want)
	}
	if got, want := decimal.int("max_int_digits"), int64(MaxIntDigits); got != want {
		t.Errorf("max_int_digits: spec %d, module %d", got, want)
	}

	// uuid: storable, the uuid family, fixed-width (the first non-integer with a width_bytes).
	// Its on-disk width (16) is a cross-core contract, so cross-check it against the spec.
	var uuid *tomlRow
	for i := range tables {
		if tables[i].str("id") == "uuid" {
			uuid = &tables[i]
		}
	}
	if uuid == nil {
		t.Fatal("uuid type missing from scalars.toml")
	}
	if uuid.str("family") != "uuid" {
		t.Errorf("uuid: family mismatch")
	}
	if !uuid.boolVal("storable") {
		t.Errorf("uuid must be storable")
	}
	if got, ok := ScalarTypeFromName("uuid"); !ok || got != Uuid {
		t.Errorf("\"uuid\" should resolve to Uuid")
	}
	if Uuid.CanonicalName() != "uuid" {
		t.Errorf("uuid canonical name mismatch")
	}
	if Uuid.WidthBytes() != 16 {
		t.Errorf("uuid should be fixed 16 bytes, got %d", Uuid.WidthBytes())
	}
}

func mustType(t *testing.T, name string) ScalarType {
	t.Helper()
	ty, ok := ScalarTypeFromName(name)
	if !ok {
		t.Fatalf("type %q should resolve", name)
	}
	return ty
}

func TestErrorCodesAreRegistered(t *testing.T) {
	tables := readTomlTables(t, specPath(t, "errors/registry.toml"), "error")
	codes := map[string]string{} // code -> name
	for _, row := range tables {
		codes[row.str("code")] = row.str("name")
	}
	for _, st := range []SqlState{
		DataException,
		NumericValueOutOfRange, InvalidDatetimeFormat, DatetimeFieldOverflow,
		DivisionByZero, InvalidParameterValue, ArraySubscriptError,
		InvalidRowCountInLimitClause, InvalidRowCountInOffsetClause,
		NotNullViolation, UniqueViolation, CheckViolation,
		UndefinedParameter, DuplicateObject, WrongObjectType,
		ActiveSqlTransaction, ReadOnlySqlTransaction, InFailedSqlTransaction,
		SyntaxError, UndefinedTable, UndefinedColumn, UndefinedObject,
		DatatypeMismatch, DuplicateTable, DuplicateColumn,
		InvalidTableDefinition, IndeterminateDatatype, FeatureNotSupported,
		NameTooLong, ProgramLimitExceeded, StatementTooComplex, CostLimitExceeded,
		IoError, UndefinedFile, DuplicateFile, DataCorrupted,
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
		// Optional named/default-argument metadata (functions.md §11); absent ⇒ empty slice.
		if strings.Join(desc.ArgNames, ",") != strings.Join(row.strs("arg_names"), ",") {
			t.Errorf("%s: arg_names mismatch", name)
		}
		if strings.Join(desc.ArgDefaults, ",") != strings.Join(row.strs("arg_defaults"), ",") {
			t.Errorf("%s: arg_defaults mismatch", name)
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

func TestAggregatesMatchSpec(t *testing.T) {
	// The generated aggregate descriptor table must match the canonical catalog's
	// [[aggregate]] rows field-for-field (codegen middle path, CLAUDE.md §5). Aggregates are
	// overloaded across operand families (one row per (name, arg_families)), like operators.
	rows := readTomlTables(t, specPath(t, "functions/catalog.toml"), "aggregate")
	if len(rows) != len(Aggregates) {
		t.Fatalf("aggregate count: spec %d, generated %d", len(rows), len(Aggregates))
	}
	find := func(name string, fams []string) (AggregateDesc, bool) {
		for _, d := range Aggregates {
			if d.Name == name && strings.Join(d.ArgFamilies, ",") == strings.Join(fams, ",") {
				return d, true
			}
		}
		return AggregateDesc{}, false
	}
	for _, row := range rows {
		name := row.str("name")
		desc, ok := find(name, row.strs("arg_families"))
		if !ok {
			t.Fatalf("generated table missing aggregate %q %v", name, row.strs("arg_families"))
		}
		if row.str("kind") != "aggregate" {
			t.Errorf("%s: kind got %q want aggregate", name, row.str("kind"))
		}
		if desc.Surface != row.str("surface") {
			t.Errorf("%s: surface mismatch", name)
		}
		if desc.Arg != row.str("arg") {
			t.Errorf("%s: arg mismatch", name)
		}
		if desc.Result != row.str("result") {
			t.Errorf("%s: result mismatch", name)
		}
		if desc.Null != row.str("null") {
			t.Errorf("%s: null mismatch", name)
		}
		if strings.Join(desc.Errors, ",") != strings.Join(row.strs("errors"), ",") {
			t.Errorf("%s: errors mismatch", name)
		}
	}
}

func TestSetReturningMatchSpec(t *testing.T) {
	// The generated set-returning descriptor table must match the canonical catalog's
	// [[set_returning]] rows field-for-field (codegen middle path, CLAUDE.md §5). SRFs are
	// overloaded across ARITY (one row per (name, arity)) — functions.md §10.
	rows := readTomlTables(t, specPath(t, "functions/catalog.toml"), "set_returning")
	if len(rows) != len(SetReturning) {
		t.Fatalf("set_returning count: spec %d, generated %d", len(rows), len(SetReturning))
	}
	find := func(name string, arity int64) (SetReturningDesc, bool) {
		for _, d := range SetReturning {
			if d.Name == name && int64(d.Arity) == arity {
				return d, true
			}
		}
		return SetReturningDesc{}, false
	}
	for _, row := range rows {
		name := row.str("name")
		arity := row.int("arity")
		desc, ok := find(name, arity)
		if !ok {
			t.Fatalf("generated table missing set_returning %q/arity-%d", name, arity)
		}
		if row.str("kind") != "set_returning" {
			t.Errorf("%s: kind got %q want set_returning", name, row.str("kind"))
		}
		if desc.Surface != row.str("surface") {
			t.Errorf("%s: surface mismatch", name)
		}
		if strings.Join(desc.ArgFamilies, ",") != strings.Join(row.strs("arg_families"), ",") {
			t.Errorf("%s: arg_families mismatch", name)
		}
		if desc.ArgResolution != row.str("arg_resolution") {
			t.Errorf("%s: arg_resolution mismatch", name)
		}
		if desc.Result != row.str("result") {
			t.Errorf("%s: result mismatch", name)
		}
		if desc.Column != row.str("column") {
			t.Errorf("%s: column mismatch", name)
		}
		if desc.Null != row.str("null") {
			t.Errorf("%s: null mismatch", name)
		}
		if strings.Join(desc.Errors, ",") != strings.Join(row.strs("errors"), ",") {
			t.Errorf("%s: errors mismatch", name)
		}
	}
}

func TestCostScheduleMatchesSpec(t *testing.T) {
	// The generated cost schedule (codegen middle path, CLAUDE.md §5/§13) must match the
	// canonical schedule.toml weight-for-weight. Cost is a cross-core contract (§8):
	// every core reads these weights.
	rows := readTomlTables(t, specPath(t, "cost/schedule.toml"), "unit")
	// The weight() switch below forces this cross-check to be updated whenever a unit is added
	// (a new unit with no Costs field fails), so we don't pin an exact count here.
	weight := func(id string) int64 {
		switch id {
		case "storage_row_read":
			return Costs.StorageRowRead
		case "page_read":
			return Costs.PageRead
		case "value_compress":
			return Costs.ValueCompress
		case "value_decompress":
			return Costs.ValueDecompress
		case "decimal_work":
			return Costs.DecimalWork
		case "row_produced":
			return Costs.RowProduced
		case "operator_eval":
			return Costs.OperatorEval
		case "aggregate_accumulate":
			return Costs.AggregateAccumulate
		case "cte_scan_row":
			return Costs.CteScanRow
		case "generated_row":
			return Costs.GeneratedRow
		case "sequence_advance":
			return Costs.SequenceAdvance
		case "gin_entry":
			return Costs.GinEntry
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

// TestRegistryCoversCatalog guards the function registry (extensibility.md §5): resolution is
// data-driven over the generated catalog tables, but the scalar kernel id (scalarFuncID) and the
// result-code / plan interpreters stay hand-written per core. A catalog row added without a
// matching kernel id, or with a result code no interpreter handles, fails here — not silently at
// some query's resolve.
func TestRegistryCoversCatalog(t *testing.T) {
	probe := func(fam string) resolvedType {
		switch fam {
		case "decimal":
			return resolvedType{kind: rtDecimal}
		case "float":
			return resolvedType{kind: rtFloat64}
		case "uuid":
			return resolvedType{kind: rtUuid}
		case "interval":
			return resolvedType{kind: rtInterval}
		case "text":
			return resolvedType{kind: rtText}
		case "boolean":
			return resolvedType{kind: rtBool}
		default: // "integer" or "any"
			return resolvedType{kind: rtInt, intTy: Int32}
		}
	}
	for i := range Operators {
		o := &Operators[i]
		if o.Kind != "function" {
			continue
		}
		if isArrayFuncName(o.Name) {
			// A polymorphic array function (array-functions.md §2): its kernel id comes from
			// arrayFuncID and its result is a reserved poly code or a scalar id.
			_ = arrayFuncID(o.Name) // panics if the name has no kernel id
			// A concrete array result `<scalar>[]` (array_positions → "i32[]") is also valid.
			concreteArray := false
			if base, ok := strings.CutSuffix(o.Result, "[]"); ok {
				_, concreteArray = ScalarTypeFromName(base)
			}
			if _, ok := ScalarTypeFromName(o.Result); o.Result != "anyarray" && o.Result != "anyelement" && !concreteArray && !ok {
				t.Fatalf("array function %s has unhandled result code %s", o.Name, o.Result)
			}
			continue
		}
		if isVariadicFuncName(o.Name) {
			// A VARIADIC function (array-functions.md §12): its kernel id comes from variadicFuncID
			// and its result is a concrete scalar id.
			_ = variadicFuncID(o.Name) // panics if the name has no kernel id
			if _, ok := ScalarTypeFromName(o.Result); !ok {
				t.Fatalf("variadic function %s has unhandled result code %s", o.Name, o.Result)
			}
			continue
		}
		if isRangeFuncName(o.Name) {
			// A polymorphic range accessor (range-functions.md §1): its kernel id comes from
			// rangeFuncID and its result is a reserved poly code (anyelement) or a scalar id (boolean).
			_ = rangeFuncID(o.Name) // panics if the name has no kernel id
			if _, ok := ScalarTypeFromName(o.Result); o.Result != "anyelement" && !ok {
				t.Fatalf("range function %s has unhandled result code %s", o.Name, o.Result)
			}
			continue
		}
		if isRangeCtorName(o.Name) {
			// A range constructor (range-functions.md §2): no scalar kernel id — the kernel is
			// evalRangeCtor, reached from the resolver. Its result is a concrete range id.
			if _, ok := rangeByName(o.Result); !ok {
				t.Fatalf("range constructor %s has non-range result code %s", o.Name, o.Result)
			}
			continue
		}
		tys := make([]resolvedType, len(o.ArgFamilies))
		for j, fam := range o.ArgFamilies {
			tys[j] = probe(fam)
		}
		_ = scalarFuncID(o.Name, tys) // panics if the name has no kernel id
		// The result code is "promoted" or a literal scalar-type id.
		if _, ok := ScalarTypeFromName(o.Result); o.Result != "promoted" && !ok {
			t.Fatalf("function %s has unhandled result code %s", o.Name, o.Result)
		}
		// make_interval resolves on its own named/defaulted path; the rest match via the registry.
		if o.Name != "make_interval" && lookupScalarOverload(o.Name, tys) == nil {
			t.Fatalf("function %s %v has no registry overload", o.Name, o.ArgFamilies)
		}
	}
	for i := range Aggregates {
		a := &Aggregates[i]
		switch a.Result {
		case "i64", "decimal", "sum_widen", "same_as_input":
		default:
			t.Fatalf("aggregate %s has unhandled result code %s", a.Name, a.Result)
		}
		surface := toLowerASCII(a.Surface)
		if a.Arg == "star" {
			if !aggregateHasStar(surface) {
				t.Fatalf("aggregate %s star overload not found", a.Surface)
			}
			continue
		}
		pt := probe(a.ArgFamilies[0])
		found := lookupAggregateOverload(surface, pt)
		if found == nil {
			t.Fatalf("aggregate %s expr overload not found for its family", a.Surface)
		}
		_, _ = aggregatePlan(surface, found.Result, pt) // panics if (surface,result) unhandled
	}
}
