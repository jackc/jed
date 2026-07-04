package jed

// FOREIGN KEY constraints — `[CONSTRAINT name] FOREIGN KEY (cols) REFERENCES …` and the
// column-level `REFERENCES` (spec/design/constraints.md §6, grammar.md §43). Covers what the
// oracle corpus (ddl/foreign_key.test) cannot: the jed-specific divergences from PostgreSQL
// (strict same-type pairing, the deferred referential actions, the end-state parent UPDATE), and
// catalog introspection (constraint names, the resolved ordinals). The agreeing behavior — the
// 23503 enforcement at every write site, MATCH SIMPLE, the batch end state, 42830/2BP01 — is the
// corpus's job. Mirrors impl/rust/tests/foreign_key.rs and impl/ts/tests/foreign_key.test.ts.

import (
	"slices"
	"testing"
)

func fkSetup(t *testing.T, sql ...string) *Session {
	t.Helper()
	db := memDB().Session(SessionOptions{})
	for _, s := range sql {
		if _, err := queryOutcome(db, s, nil); err != nil {
			t.Fatalf("setup %q: %v", s, err)
		}
	}
	return db
}

func fkErr(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

func fkNames(t *testing.T, db dbHandle, table string) []string {
	t.Helper()
	tab, ok := db.Table(table)
	if !ok {
		t.Fatalf("table %s not found", table)
	}
	names := make([]string, len(tab.ForeignKeys))
	for i, f := range tab.ForeignKeys {
		names[i] = f.Name
	}
	return names
}

// Auto-naming follows PostgreSQL's <table>_<localcols>_fkey; an explicit CONSTRAINT name is used as
// written; the catalog holds FKs in ascending lowercased-name order.
func TestForeignKeyNamingAndOrder(t *testing.T) {
	db := fkSetup(
		t,
		"CREATE TABLE p (a i32, b i32, code i32 UNIQUE, PRIMARY KEY (a, b))",
		"CREATE TABLE c (id i32 PRIMARY KEY, pa i32, pb i32, pcode i32, "+
			"CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code), "+
			"FOREIGN KEY (pa, pb) REFERENCES p (a, b))",
	)
	if got := fkNames(t, db, "c"); !slices.Equal(got, []string{"c_code_fk", "c_pa_pb_fkey"}) {
		t.Fatalf("fk names: got %v", got)
	}

	db2 := fkSetup(
		t,
		"CREATE TABLE q (id i32 PRIMARY KEY)",
		"CREATE TABLE r (id i32 PRIMARY KEY, x i32 REFERENCES q, FOREIGN KEY (x) REFERENCES q (id))",
	)
	if got := fkNames(t, db2, "r"); !slices.Equal(got, []string{"r_x_fkey", "r_x_fkey1"}) {
		t.Fatalf("auto-name suffix walk: got %v", got)
	}
}

// jed is STRICTER than PostgreSQL on type pairing: corresponding columns must be the SAME scalar
// type (42804), where PG allows any comparable pair (e.g. i32 ↔ i64) — constraints.md §6.7.
func TestForeignKeyStrictTypePairing(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE p (id i32 PRIMARY KEY)")
	if got := fkErr(t, db, "CREATE TABLE c1 (x i64 REFERENCES p)"); got != "42804" {
		t.Fatalf("i64→i32 pk: got %s, want 42804", got)
	}
	if got := fkErr(t, db, "CREATE TABLE c2 (x text REFERENCES p)"); got != "42804" {
		t.Fatalf("text→i32 pk: got %s, want 42804", got)
	}
	if _, err := queryOutcome(db, "CREATE TABLE c3 (x i32 REFERENCES p)", nil); err != nil {
		t.Fatalf("same-type FK should be accepted: %v", err)
	}
}

// CASCADE / SET NULL / SET DEFAULT parse but are rejected at CREATE TABLE (0A000); NO ACTION and
// RESTRICT are accepted (constraints.md §6.6).
func TestForeignKeyReferentialActionsNarrowed(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE p (id i32 PRIMARY KEY)")
	for _, sql := range []string{
		"CREATE TABLE c1 (x i32 REFERENCES p ON DELETE CASCADE)",
		"CREATE TABLE c2 (x i32 REFERENCES p ON UPDATE SET NULL)",
		"CREATE TABLE c3 (x i32 REFERENCES p ON DELETE SET DEFAULT)",
	} {
		if got := fkErr(t, db, sql); got != "0A000" {
			t.Fatalf("%q: got %s, want 0A000", sql, got)
		}
	}
	if _, err := queryOutcome(db, "CREATE TABLE c4 (x i32 REFERENCES p ON DELETE NO ACTION ON UPDATE RESTRICT)", nil); err != nil {
		t.Fatalf("NO ACTION/RESTRICT should be accepted: %v", err)
	}
}

// jed validates the parent side against the statement's END STATE: a swap of two referenced UNIQUE
// values keeps every referenced tuple present, so the UPDATE succeeds where PG fails on the
// transient — a documented divergence (constraints.md §6.7).
func TestForeignKeyParentUpdateEndStateSwap(t *testing.T) {
	db := fkSetup(
		t,
		"CREATE TABLE p (id i32 PRIMARY KEY, code i32 UNIQUE)",
		"INSERT INTO p VALUES (1, 100), (2, 200)",
		"CREATE TABLE c (id i32 PRIMARY KEY, pc i32 REFERENCES p (code))",
		"INSERT INTO c VALUES (10, 100), (11, 200)",
	)
	if _, err := queryOutcome(db, "UPDATE p SET code = CASE code WHEN 100 THEN 200 ELSE 100 END", nil); err != nil {
		t.Fatalf("referenced-value swap should succeed (end state): %v", err)
	}
	if got := fkErr(t, db, "UPDATE p SET code = 999 WHERE id = 1"); got != "23503" {
		t.Fatalf("orphaning update: got %s, want 23503", got)
	}
}
