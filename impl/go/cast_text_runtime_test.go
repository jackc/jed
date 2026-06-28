package jed

// Runtime text → numeric/boolean casts — the parts the PG-clean oracle corpus cannot express (the
// runtime-text-cast slice; spec/design/grammar.md §36, spec/design/types.md §5, spec/types/casts.toml).
// The accepted-grammar int/decimal/boolean cases AGREE with PostgreSQL and are oracle-checked in
// suites/cast/text_to_scalar.test (run on every core); this file covers only what that corpus cannot:
// (a) the jed-stricter grammar DIVERGENCES — hex / digit-underscore / NaN trap 22P02 where PG accepts
// them — and (b) runtime text → f32/f64, kept out of the corpus because the float renderer is in the
// determinism-exception ledger. Every cast is on a NON-LITERAL text column, so it exercises the
// per-row evalCast path, not the resolve-time literal fold. Mirrors impl/rust/tests/cast_text_runtime.rs.

import (
	"math"
	"testing"
)

// seededText builds t(id i32 pk, s text) with one row per string (id = 1..).
func seededText(t *testing.T, rows ...string) *engine {
	t.Helper()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, s text)")
	for i, s := range rows {
		if _, err := execute(db, "INSERT INTO t VALUES ("+itoaTest(i+1)+", '"+s+"')"); err != nil {
			t.Fatalf("seed %q: %v", s, err)
		}
	}
	return db
}

func itoaTest(n int) string {
	if n == 0 {
		return "0"
	}
	var b []byte
	for n > 0 {
		b = append([]byte{byte('0' + n%10)}, b...)
		n /= 10
	}
	return string(b)
}

func castAt(t *testing.T, db *engine, expr string, id int) Value {
	t.Helper()
	return castOne(t, db, "SELECT "+expr+" FROM t WHERE id = "+itoaTest(id))
}

func castErrAt(t *testing.T, db *engine, expr string, id int) string {
	t.Helper()
	return castErrCode(t, db, "SELECT "+expr+" FROM t WHERE id = "+itoaTest(id))
}

// --- (a) jed-stricter grammar divergences on the RUNTIME path -------------------------------------

func TestRuntimeTextCastGrammarDivergences(t *testing.T) {
	cases := []struct {
		s, expr string
	}{
		{"0x10", "s :: int"},    // PG: '0x10'::int4 → 16; jed: decimal digits only → 22P02
		{"1_000", "s :: int"},   // PG: '1_000'::int4 → 1000; jed: no underscores → 22P02
		{"NaN", "s :: numeric"}, // PG: 'NaN'::numeric → NaN; jed decimal is finite → 22P02
	}
	for _, c := range cases {
		db := seededText(t, c.s)
		if code := castErrAt(t, db, c.expr, 1); code != "22P02" {
			t.Fatalf("%q :: %q: got %s, want 22P02", c.s, c.expr, code)
		}
	}
}

// --- (b) runtime text → f32/f64 (out of the corpus: float render is determinism-exempt) ----------

func TestRuntimeTextToF64Finite(t *testing.T) {
	db := seededText(t, "1.5", "-0.25", "100", "1e3")
	for id, want := range map[int]float64{1: 1.5, 2: -0.25, 3: 100, 4: 1000} {
		v := castAt(t, db, "s :: float8", id)
		if v.Kind != ValFloat64 || v.F64() != want {
			t.Fatalf("id %d: got %v, want float64 %v", id, v, want)
		}
	}
}

func TestRuntimeTextToF32Frounds(t *testing.T) {
	db := seededText(t, "0.5", "3.14")
	if v := castAt(t, db, "s :: float4", 1); v.Kind != ValFloat32 || v.F32() != 0.5 {
		t.Fatalf("0.5::float4 = %v", v)
	}
	if v := castAt(t, db, "s :: float4", 2); v.Kind != ValFloat32 || v.F32() != float32(3.14) {
		t.Fatalf("3.14::float4 = %v", v)
	}
}

func TestRuntimeTextToFloatSpecialWords(t *testing.T) {
	db := seededText(t, "NaN", "Infinity", "-inf")
	if v := castAt(t, db, "s :: float8", 1); v.Kind != ValFloat64 || !math.IsNaN(v.F64()) {
		t.Fatalf("NaN::float8 = %v", v)
	}
	if v := castAt(t, db, "s :: float8", 2); v.Kind != ValFloat64 || v.F64() != math.Inf(1) {
		t.Fatalf("Infinity::float8 = %v", v)
	}
	if v := castAt(t, db, "s :: float8", 3); v.Kind != ValFloat64 || v.F64() != math.Inf(-1) {
		t.Fatalf("-inf::float8 = %v", v)
	}
}

func TestRuntimeTextToFloatOverflowAndMalformed(t *testing.T) {
	db := seededText(t, "1e400", "abc")
	// a FINITE literal beyond binary64 range traps 22003 (not ±Inf — the finite-overflow rule)
	if code := castErrAt(t, db, "s :: float8", 1); code != "22003" {
		t.Fatalf("1e400::float8: got %s, want 22003", code)
	}
	if code := castErrAt(t, db, "s :: float8", 2); code != "22P02" {
		t.Fatalf("abc::float8: got %s, want 22P02", code)
	}
}

func TestRuntimeTextToFloatNullPropagates(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, s text)",
		"INSERT INTO t VALUES (1, NULL)")
	if v := castAt(t, db, "s :: float8", 1); v.Kind != ValNull {
		t.Fatalf("NULL::float8 = %v, want NULL", v)
	}
}
