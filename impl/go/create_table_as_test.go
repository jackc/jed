package jed

// CREATE TABLE … AS (spec/design/create-table-as.md). The SQL-visible behavior is the corpus's job
// (ddl/create_table_as*.test); this covers the host surface it cannot drive — the query's bound
// parameters (§1), the affected-row count, and the column-definition form's refusal of parameters.
// Mirrors the Rust create_table_as_binds_parameters test and the TS create_table_as test.

import "testing"

func TestCreateTableAsBindsParameters(t *testing.T) {
	t.Parallel()
	sess := memDB().Session(SessionOptions{})
	defer sess.Close()
	sessExec(t, sess, "CREATE TABLE src (id i32 PRIMARY KEY, name text)")
	sessExec(t, sess, "INSERT INTO src VALUES (1, 'a'), (2, 'b'), (3, 'c')")
	sql := "CREATE TABLE t AS SELECT id, name, $2::text AS tag FROM src WHERE id >= $1"
	out, err := queryOutcome(sess, sql, []Value{IntValue(2), TextValue("x")})
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != outcomeStatement || !out.HasRowsAffected || out.RowsAffected != 2 {
		t.Fatalf("%q: got kind %v rows affected (%d, %v), want a statement affecting 2", sql, out.Kind, out.RowsAffected, out.HasRowsAffected)
	}
	rows := queryRows(t, sess, "SELECT id, name, tag FROM t ORDER BY id")
	if len(rows) != 2 ||
		rows[0][0].Int != 2 || rows[0][1].str() != "b" || rows[0][2].str() != "x" ||
		rows[1][0].Int != 3 || rows[1][1].str() != "c" || rows[1][2].str() != "x" {
		t.Fatalf("t rows = %v, want (2,b,x), (3,c,x)", rows)
	}
	// A missing parameter is the query's own count mismatch, and nothing is created.
	if code := paramErrCode(t, sess, "CREATE TABLE u AS SELECT $1::i32 AS a"); code != "42601" {
		t.Fatalf("missing parameter: got %s, want 42601", code)
	}
	if code := errCode(t, sess, "SELECT a FROM u"); code != "42P01" {
		t.Fatalf("u after a failed CREATE TABLE AS: got %s, want 42P01", code)
	}
	// WITH NO DATA binds the parameters but runs nothing.
	out, err = queryOutcome(sess, "CREATE TABLE v AS SELECT $1::i32 AS a WITH NO DATA", []Value{IntValue(1)})
	if err != nil {
		t.Fatalf("WITH NO DATA: %v", err)
	}
	if out.Kind != outcomeStatement || out.HasRowsAffected {
		t.Fatalf("WITH NO DATA: got kind %v rows affected (%d, %v), want a statement with none", out.Kind, out.RowsAffected, out.HasRowsAffected)
	}
	if rows := queryRows(t, sess, "SELECT a FROM v"); len(rows) != 0 {
		t.Fatalf("v rows = %v, want none", rows)
	}
	// The column-definition form is plain DDL: it takes no parameters.
	if code := paramErrCode(t, sess, "CREATE TABLE w (a i32)", IntValue(1)); code != "42601" {
		t.Fatalf("column-definition form with a parameter: got %s, want 42601", code)
	}
}
