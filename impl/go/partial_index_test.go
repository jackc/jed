package jed

import (
	"strings"
	"testing"
)

// Partial-index behaviors the shared corpus cannot express (a PG divergence — jed's syntactic
// implication + timestamptz hazard; on-disk byte round-trip; catalog introspection). The PG-agreeing
// behavior (23505 among qualifying rows, error codes, planner rows) lives in the corpus
// (spec/conformance/suites/ddl/partial_index.test).

// A UNIQUE partial index constrains ONLY its qualifying rows (indexes.md §9): two active rows may
// not share amt, but an inactive row may duplicate an active one. Survives reload (v27).
func TestPartialUniqueConstrainsOnlyQualifyingAndPersists(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE pt (id i32 PRIMARY KEY, status text, amt i32)")
	mustExec(t, db, "INSERT INTO pt VALUES (1, 'active', 10)")
	mustExec(t, db, "CREATE UNIQUE INDEX pt_uact ON pt (amt) WHERE status = 'active'")
	// An inactive row may duplicate the active amt=10 (it is not in the index).
	mustExec(t, db, "INSERT INTO pt VALUES (2, 'inactive', 10)")
	// A second active amt=10 collides (23505 names the partial index).
	if code := errCode(t, db, "INSERT INTO pt VALUES (3, 'active', 10)"); code != "23505" {
		t.Fatalf("two active rows sharing amt: want 23505, got %s", code)
	}
	// Round-trip: the v27 catalog re-parses the predicate, and it still enforces + exempts.
	img, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	re, err := loadEngine(img)
	if err != nil {
		t.Fatal(err)
	}
	res := re
	mustExec(t, res, "INSERT INTO pt VALUES (4, 'inactive', 10)")
	if code := errCode(t, res, "INSERT INTO pt VALUES (5, 'active', 10)"); code != "23505" {
		t.Fatalf("partial uniqueness survives reload: want 23505, got %s", code)
	}
}

// The planner uses a partial index ONLY when the WHERE contains the predicate conjunct (indexes.md
// §9) — the syntactic implication gate. EXPLAIN names it when gated, not otherwise.
func TestPartialPlannerGatesOnPredicateConjunct(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE pt (id i32 PRIMARY KEY, status text, amt i32)")
	mustExec(t, db, "INSERT INTO pt VALUES (1,'active',10),(2,'inactive',10),(3,'active',30)")
	mustExec(t, db, "CREATE INDEX pt_amt_active ON pt (amt) WHERE status = 'active'")
	planNames := func(sql string) string {
		rows := queryRows(t, db, sql)
		var s string
		for _, r := range rows {
			s += r[2].Render() + "\n"
		}
		return s
	}
	gated := planNames("EXPLAIN SELECT id FROM pt WHERE status = 'active' AND amt = 10")
	if !strings.Contains(gated, "pt_amt_active") {
		t.Fatalf("gated plan should use the partial index:\n%s", gated)
	}
	ungated := planNames("EXPLAIN SELECT id FROM pt WHERE amt = 10")
	if strings.Contains(ungated, "pt_amt_active") {
		t.Fatalf("ungated plan must NOT use the partial index:\n%s", ungated)
	}
	// Rows are correct either way (the residual filter re-applies the full WHERE).
	rows := queryRows(t, db, "SELECT id FROM pt WHERE status = 'active' AND amt = 10")
	if len(rows) != 1 || rows[0][0].Render() != "1" {
		t.Fatalf("want only the active amt=10 row (id 1), got %v", rows)
	}
}

// A timestamptz-referencing predicate is 42P17 (the session-tz hazard, a jed divergence); a
// non-boolean predicate is 42804; a partial GIN index is 0A000.
func TestPartialPredicateRejections(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz, a i32, arr i32[])")
	if code := errCode(t, db, "CREATE INDEX ON t (a) WHERE ts IS NULL"); code != "42P17" {
		t.Fatalf("timestamptz predicate: want 42P17, got %s", code)
	}
	if code := errCode(t, db, "CREATE INDEX ON t (a) WHERE a"); code != "42804" {
		t.Fatalf("non-boolean predicate: want 42804, got %s", code)
	}
	if code := errCode(t, db, "CREATE INDEX ON t USING gin (arr) WHERE a > 0"); code != "0A000" {
		t.Fatalf("partial gin: want 0A000, got %s", code)
	}
}

// jed_indexes surfaces a partial index's predicate canonical text; NULL for a non-partial index.
func TestPartialIntrospectionShowsPredicate(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, s text, a i32)")
	mustExec(t, db, "CREATE INDEX ipart ON t (a) WHERE s = 'x'")
	mustExec(t, db, "CREATE INDEX ifull ON t (a)")
	part := queryRows(t, db, "SELECT predicate FROM jed_indexes WHERE name = 'ipart'")
	if len(part) != 1 || !strings.Contains(part[0][0].Render(), "x") {
		t.Fatalf("partial predicate text: got %v", part)
	}
	full := queryRows(t, db, "SELECT predicate FROM jed_indexes WHERE name = 'ifull'")
	if len(full) != 1 || !full[0][0].IsNull() {
		t.Fatalf("non-partial predicate should be NULL: got %v", full)
	}
}
