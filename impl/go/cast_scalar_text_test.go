package jed

// Any built-in scalar → text (spec/types/casts.toml, spec/design/types.md §5) — only what the
// PG-clean oracle corpus cannot hold. The rendering of every type, the session-zone timestamptz form,
// the 42P17 index rejection, and element-wise T[] → text[] agree with PostgreSQL and live in
// suites/cast/scalar_to_text.test. Here: the casts are EXPLICIT only (an assignment or implicit use
// is 42804 where PG accepts it), composite/range → text stay 0A000 (PG renders them), and the
// $1::text parameter declaration. Mirrors impl/rust/tests/cast_scalar_text.rs.

import "testing"

// Only the explicit spelling converts: storing or comparing a non-text value against text stays a
// 42804 datatype mismatch (PostgreSQL assignment-casts to text and would accept the INSERT).
func TestTextCastIsExplicitOnly(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, s text)")
	for _, sql := range []string{
		"INSERT INTO t VALUES (1, 42)",
		"INSERT INTO t VALUES (1, true)",
		"INSERT INTO t SELECT 1, date '2024-01-02'",
		"SELECT id FROM t WHERE s = 42",
	} {
		if code := castErrCode(t, db, sql); code != "42804" {
			t.Fatalf("%q: got %s, want 42804", sql, code)
		}
	}
	if _, err := queryOutcome(db, "INSERT INTO t SELECT 1, 42::text", nil); err != nil {
		t.Fatal(err)
	}
	if v := castOne(t, db, "SELECT s FROM t WHERE s = 42::text"); v.Kind != ValText || v.str() != "42" {
		t.Fatalf("s = %v, want text 42", v)
	}
}

// A composite or range value has no text cast yet (0A000), though PostgreSQL renders both.
func TestCompositeAndRangeToTextAre0A000(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TYPE addr AS (street text, zip i32)")
	for _, sql := range []string{
		"SELECT ('(Main,90210)'::addr)::text",
		"SELECT ('[1,5)'::i32range)::text",
	} {
		if code := castErrCode(t, db, sql); code != "0A000" {
			t.Fatalf("%q: got %s, want 0A000", sql, code)
		}
	}
}

// $1::text declares the parameter as text (the cast-target parameter typing, api.md §5); a
// varchar(n) target truncates the bound value.
func TestParamCastToTextDeclaresText(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, c := range []struct {
		sql  string
		bind Value
		want Value
	}{
		{"SELECT $1::text", TextValue("abc"), TextValue("abc")},
		{"SELECT $1::varchar(2)", TextValue("abc"), TextValue("ab")},
		{"SELECT $1::text", NullValue(), NullValue()},
	} {
		out, err := queryOutcome(db, c.sql, []Value{c.bind})
		if err != nil {
			t.Fatalf("%q: %v", c.sql, err)
		}
		if got := out.Rows[0][0]; got.Kind != c.want.Kind || got.Render() != c.want.Render() {
			t.Fatalf("%q: got %v, want %v", c.sql, got, c.want)
		}
	}
}
