package jed

// Runtime text → date cast — the parts the PG-clean oracle corpus cannot express (the text→date
// cast follow-on; spec/design/date.md §6, spec/types/casts.toml). The strict-ISO accepted grammar
// AGREES with PostgreSQL and is oracle-checked in suites/cast/text_to_date.test (run on every
// core, including the 42P17 index rejections — PG's date_in is stable too); this file covers only
// the jed-stricter grammar DIVERGENCES: the DateStyle-dependent / non-ISO spellings PostgreSQL
// accepts and jed rejects (22007), and the `:60` leap-second roll-forward PG performs and jed
// rejects (22008) — identical to the literal path (date.md §2). Every cast is on a NON-LITERAL
// text column, so it exercises the per-row evalDateConvert path, not the resolve-time literal
// fold. Mirrors impl/rust/tests/cast_text_date.rs.

import "testing"

func TestRuntimeTextToDateGrammarDivergences(t *testing.T) {
	t.Parallel()
	cases := []struct {
		s, want string
	}{
		{"01/15/2024", "22007"},          // PG (DateStyle MDY): 2024-01-15; jed: strict ISO only
		{"Jan 15, 2024", "22007"},        // PG: month-name spelling; jed: strict ISO only
		{"20240115", "22007"},            // PG: compact ISO; jed: dashed year-month-day only
		{"2024-01-01 12:30:60", "22008"}, // PG rolls :60 forward; jed rejects leap seconds
	}
	for _, c := range cases {
		db := seededText(t, c.s)
		if code := castErrAt(t, db, "s :: date", 1); code != c.want {
			t.Fatalf("%q :: date: got %s, want %s", c.s, code, c.want)
		}
	}
}

func TestRuntimeTextToDateNullPropagates(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, s text)",
		"INSERT INTO t VALUES (1, NULL)")
	if v := castAt(t, db, "s :: date", 1); v.Kind != ValNull {
		t.Fatalf("NULL::date = %v, want NULL", v)
	}
}
