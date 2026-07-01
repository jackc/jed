package jed

// UNIQUE constraints + unique indexes (spec/design/constraints.md §5, indexes.md §8) —
// covers what the corpus suite (ddl/unique.test) cannot: catalog introspection (the
// unique flag, fold results, name order), the v6 on-disk round-trip, transactional DDL,
// and the documented PG divergences (end-state UPDATE validation, droppable
// constraint-backed indexes). Mirrored in impl/rust/tests/unique.rs and
// impl/ts/tests/unique.test.ts.

import (
	"bytes"
	"fmt"
	"slices"
	"strings"
	"testing"
)

func uqRun(t *testing.T, db dbHandle, sql string) Outcome {
	t.Helper()
	o, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return o
}

func uqCost(t *testing.T, db dbHandle, sql string) int64 {
	t.Helper()
	return uqRun(t, db, sql).Cost
}

func uqIds(t *testing.T, db dbHandle, sql string) []int64 {
	t.Helper()
	o := uqRun(t, db, sql)
	out := make([]int64, 0, len(o.Rows))
	for _, r := range o.Rows {
		if r[0].Kind != ValInt {
			t.Fatalf("expected an int id, got %v", r[0])
		}
		out = append(out, r[0].Int)
	}
	return out
}

func uqErr(t *testing.T, db dbHandle, sql string) (string, string) {
	t.Helper()
	_, err := db.Execute(sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected an EngineError from %q, got %v", sql, err)
	}
	return ee.State.Code(), ee.Message
}

// uqNames is each index of the table as "name" or "name!" (unique), in catalog order.
func uqNames(t *testing.T, db dbHandle, table string) []string {
	t.Helper()
	tbl, ok := db.Table(table)
	if !ok {
		t.Fatalf("table %s missing", table)
	}
	out := make([]string, 0, len(tbl.Indexes))
	for _, ix := range tbl.Indexes {
		n := ix.Name
		if ix.Unique {
			n += "!"
		}
		out = append(out, n)
	}
	return out
}

// Constraint naming matches PostgreSQL (oracle-probed, constraints.md §5.3): the
// lowercased <table>_<cols>_key base with the smallest free suffix, walked past BOTH the
// relation namespace and the table's check names; an explicit CONSTRAINT name is the
// index name as written.
func TestUniqueConstraintNamingMatchesPostgres(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE other (x i32)")
	uqRun(t, db, "CREATE INDEX walk_a_key ON other (x)") // occupies the derived base
	uqRun(t, db, "CREATE TABLE Walk (a i32 UNIQUE, b i32, CONSTRAINT Named UNIQUE (b, a), "+
		"CONSTRAINT walk_b_check CHECK (b > 0), UNIQUE (b))")
	if got := uqNames(t, db, "walk"); !slices.Equal(got, []string{"Named!", "walk_a_key1!", "walk_b_key!"}) {
		t.Fatalf("walk indexes = %v", got)
	}
	// A derived name walks past a CHECK name too (PG-probed: w1_a_key -> w1_a_key1).
	uqRun(t, db, "CREATE TABLE w1 (a i32, CONSTRAINT w1_a_key CHECK (a > 0), UNIQUE (a))")
	if got := uqNames(t, db, "w1"); !slices.Equal(got, []string{"w1_a_key1!"}) {
		t.Fatalf("w1 indexes = %v", got)
	}
}

// The dedup/fold rules match PostgreSQL (oracle-probed, constraints.md §5.2): identical
// member lists fold into one (the first explicitly-named one's name wins); a list
// identical to the primary key's folds away entirely; a differing ORDER is distinct.
func TestUniqueDedupAndPKFoldMatchPostgres(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE e3 (a i32 UNIQUE UNIQUE, UNIQUE (a))")
	if got := uqNames(t, db, "e3"); !slices.Equal(got, []string{"e3_a_key!"}) {
		t.Fatalf("e3 indexes = %v", got)
	}
	// An unnamed-then-named pair keeps the NAME (PG: p1 kept "named").
	uqRun(t, db, "CREATE TABLE p1 (a i32 UNIQUE, CONSTRAINT named UNIQUE (a))")
	if got := uqNames(t, db, "p1"); !slices.Equal(got, []string{"named!"}) {
		t.Fatalf("p1 indexes = %v", got)
	}
	// Two named duplicates keep the FIRST (PG: e7 kept "x").
	uqRun(t, db, "CREATE TABLE e7 (a i32, CONSTRAINT x UNIQUE (a), CONSTRAINT y UNIQUE (a))")
	if got := uqNames(t, db, "e7"); !slices.Equal(got, []string{"x!"}) {
		t.Fatalf("e7 indexes = %v", got)
	}
	// The PK absorbs an identical list — regardless of declaration order or form.
	uqRun(t, db, "CREATE TABLE e5 (a i32 PRIMARY KEY UNIQUE)")
	if got := uqNames(t, db, "e5"); len(got) != 0 {
		t.Fatalf("e5 indexes = %v", got)
	}
	uqRun(t, db, "CREATE TABLE p2 (a i32 UNIQUE, PRIMARY KEY (a))")
	if got := uqNames(t, db, "p2"); len(got) != 0 {
		t.Fatalf("p2 indexes = %v", got)
	}
	uqRun(t, db, "CREATE TABLE e9 (a i32, b i32, PRIMARY KEY (a, b), UNIQUE (a, b))")
	if got := uqNames(t, db, "e9"); len(got) != 0 {
		t.Fatalf("e9 indexes = %v", got)
	}
	// A differing member ORDER is a distinct constraint (PG: p3 kept both).
	uqRun(t, db, "CREATE TABLE p3 (a i32, b i32, PRIMARY KEY (a, b), UNIQUE (b, a))")
	if got := uqNames(t, db, "p3"); !slices.Equal(got, []string{"p3_b_a_key!"}) {
		t.Fatalf("p3 indexes = %v", got)
	}
}

// DDL errors match PostgreSQL (oracle-probed, constraints.md §5.1/§5.3): member
// resolution 42703/42701/0A000 (before any CHECK validates), explicit-name collisions
// 42P07 (relation namespace, including the table being created) before 42710 (the
// table's constraint names).
func TestUniqueDDLErrorsMatchPostgres(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE other (x i32)")
	if code, _ := uqErr(t, db, "CREATE TABLE e2 (a i32, UNIQUE (nosuch))"); code != "42703" {
		t.Fatalf("unknown member = %s", code)
	}
	if code, _ := uqErr(t, db, "CREATE TABLE e1 (a i32, UNIQUE (a, a))"); code != "42701" {
		t.Fatalf("dup member = %s", code)
	}
	// f64 IS now a valid UNIQUE member (the float-order-preserving key, encoding.md §2.8 — every
	// scalar is keyable; text/bytea covered in ddl/unique.test).
	uqRun(t, db, "CREATE TABLE e6 (a i32, s f64 UNIQUE)")
	// UNIQUE members resolve BEFORE any CHECK validates (PG: z1/z2), in either order.
	if code, _ := uqErr(t, db, "CREATE TABLE z1 (a i32, CHECK (nosuch1 > 0), UNIQUE (nosuch2))"); code != "42703" {
		t.Fatalf("z1 = %s", code)
	}
	if _, msg := uqErr(t, db, "CREATE TABLE z2 (a i32, UNIQUE (nosuch2), CHECK (nosuch1 > 0))"); !strings.Contains(msg, "nosuch2") {
		t.Fatalf("unique member first: %q", msg)
	}
	// An explicit constraint name collides in the RELATION namespace: an existing table,
	// the table being created (PG: p4), and a same-statement sibling (PG: e8).
	if code, _ := uqErr(t, db, "CREATE TABLE c2 (a i32, CONSTRAINT other UNIQUE (a))"); code != "42P07" {
		t.Fatalf("vs table = %s", code)
	}
	if code, _ := uqErr(t, db, "CREATE TABLE p4 (a i32, CONSTRAINT p4 UNIQUE (a))"); code != "42P07" {
		t.Fatalf("vs self = %s", code)
	}
	if code, _ := uqErr(t, db,
		"CREATE TABLE e8 (a i32, CONSTRAINT x UNIQUE (a), b i32, CONSTRAINT x UNIQUE (b))"); code != "42P07" {
		t.Fatalf("vs sibling = %s", code)
	}
	// ... and with a CHECK constraint's name it is 42710, in either declaration order
	// (PG: z4/z5 — both report when the unique constraint is created).
	if code, _ := uqErr(t, db,
		"CREATE TABLE z4 (a i32, CONSTRAINT zc CHECK (a > 0), CONSTRAINT zc UNIQUE (a))"); code != "42710" {
		t.Fatalf("z4 = %s", code)
	}
	if code, _ := uqErr(t, db,
		"CREATE TABLE z5 (a i32, CONSTRAINT zc UNIQUE (a), CONSTRAINT zc CHECK (a > 0))"); code != "42710" {
		t.Fatalf("z5 = %s", code)
	}
	// CREATE UNIQUE <not-index> is a syntax error.
	if code, _ := uqErr(t, db, "CREATE UNIQUE TABLE t (a i32)"); code != "42601" {
		t.Fatalf("create unique table = %s", code)
	}
}

// INSERT enforcement (indexes.md §8): a duplicate against the store or within the batch
// traps 23505 naming the index; NULLS DISTINCT exempts any tuple with a NULL component;
// the violation precedence is CHECK before PK before UNIQUE, and among unique indexes
// the catalog (name) order.
func TestUniqueInsertEnforcement(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 UNIQUE, w i32, "+
		"CONSTRAINT wv UNIQUE (w, v), CHECK (id < 100))")
	uqRun(t, db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 100)")
	// A stored duplicate; the message names the violated index.
	if code, msg := uqErr(t, db, "INSERT INTO t VALUES (3, 10, 200)"); code != "23505" || !strings.Contains(msg, "t_v_key") {
		t.Fatalf("stored dup = %s %q", code, msg)
	}
	// An in-batch duplicate (two-phase: nothing stored).
	if code, _ := uqErr(t, db, "INSERT INTO t VALUES (3, 30, 1), (4, 30, 2)"); code != "23505" {
		t.Fatalf("in-batch dup = %s", code)
	}
	if got := uqIds(t, db, "SELECT id FROM t ORDER BY id"); !slices.Equal(got, []int64{1, 2}) {
		t.Fatalf("all-or-nothing: %v", got)
	}
	// NULLS DISTINCT: any number of NULLs coexist, and a NULL component exempts the
	// multi-column tuple — (100, NULL) twice is fine even though w matches.
	uqRun(t, db, "INSERT INTO t VALUES (5, NULL, 100)")
	uqRun(t, db, "INSERT INTO t VALUES (6, NULL, 300), (7, NULL, 300)")
	// A fully non-NULL composite duplicate traps, naming the composite index (its own
	// table — beside t_v_key the component dup would always be reported first).
	uqRun(t, db, "CREATE TABLE c (id i32 PRIMARY KEY, w i32, v i32, CONSTRAINT wv2 UNIQUE (w, v))")
	uqRun(t, db, "INSERT INTO c VALUES (1, 40, 400)")
	if _, msg := uqErr(t, db, "INSERT INTO c VALUES (2, 40, 400)"); !strings.Contains(msg, "wv2") {
		t.Fatalf("composite dup: %q", msg)
	}
	// A distinct pair sharing one component is a different tuple — allowed.
	uqRun(t, db, "INSERT INTO c VALUES (2, 40, 401)")
	uqRun(t, db, "INSERT INTO t VALUES (8, 40, 400)")
	// INSERT ... SELECT takes the same path.
	if code, _ := uqErr(t, db, "INSERT INTO t SELECT id + 20, v, w FROM t WHERE id = 8"); code != "23505" {
		t.Fatalf("insert-select = %s", code)
	}
	// Precedence: the PK's 23505 wins over UNIQUE (PG-probed), naming <table>_pkey.
	if _, msg := uqErr(t, db, "INSERT INTO t VALUES (1, 10, 999)"); !strings.Contains(msg, "t_pkey") {
		t.Fatalf("PK first: %q", msg)
	}
	// ... and CHECK (23514) fires before either (PG-probed).
	if code, _ := uqErr(t, db, "INSERT INTO t VALUES (101, 10, 999)"); code != "23514" {
		t.Fatalf("check first = %s", code)
	}
	// Two violated unique indexes report in catalog (name) order: t_v_key < wv.
	if _, msg := uqErr(t, db, "INSERT INTO t VALUES (10, 40, 400)"); !strings.Contains(msg, "t_v_key") {
		t.Fatalf("name order: %q", msg)
	}
}

// UPDATE validates uniqueness against the statement's END STATE (indexes.md §8 — the
// documented PG divergence): self-resolving rewrites succeed; genuine conflicts with
// untouched rows and in-batch collisions trap 23505; nothing is written on failure.
func TestUniqueUpdateEnforcementEndState(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE m (id i32 PRIMARY KEY, v i32 UNIQUE)")
	uqRun(t, db, "INSERT INTO m VALUES (1, 1), (2, 2), (3, 30)")
	// PG fails both of these on the transient per-row collision; jed's end state is unique.
	uqRun(t, db, "UPDATE m SET v = v + 1 WHERE id < 3") // 1,2 -> 2,3
	if got := uqIds(t, db, "SELECT v FROM m ORDER BY id"); !slices.Equal(got, []int64{2, 3, 30}) {
		t.Fatalf("shift: %v", got)
	}
	uqRun(t, db, "UPDATE m SET v = 5 - v WHERE id < 3") // swap: 2,3 -> 3,2
	if got := uqIds(t, db, "SELECT v FROM m ORDER BY id"); !slices.Equal(got, []int64{3, 2, 30}) {
		t.Fatalf("swap: %v", got)
	}
	// A no-op rewrite of the same value is fine (its own old entry never conflicts).
	uqRun(t, db, "UPDATE m SET v = v WHERE id = 1")
	// A genuine conflict with an untouched row.
	if code, _ := uqErr(t, db, "UPDATE m SET v = 30 WHERE id = 1"); code != "23505" {
		t.Fatalf("untouched conflict = %s", code)
	}
	// An in-batch collision: two rewritten rows landing on one value.
	if code, _ := uqErr(t, db, "UPDATE m SET v = 7 WHERE id < 3"); code != "23505" {
		t.Fatalf("in-batch collision = %s", code)
	}
	// All-or-nothing: the failed statements wrote nothing.
	if got := uqIds(t, db, "SELECT v FROM m ORDER BY id"); !slices.Equal(got, []int64{3, 2, 30}) {
		t.Fatalf("all-or-nothing: %v", got)
	}
	// NULL is exempt on UPDATE too: several rows may go NULL at once.
	uqRun(t, db, "UPDATE m SET v = NULL WHERE id < 3")
	if got := uqIds(t, db, "SELECT id FROM m WHERE v IS NULL ORDER BY id"); !slices.Equal(got, []int64{1, 2}) {
		t.Fatalf("NULL exempt: %v", got)
	}
}

// CREATE UNIQUE INDEX verifies existing rows before registering (indexes.md §2/§8): a
// duplicate traps 23505 and creates nothing (the name stays free); NULLs are exempt;
// thereafter it enforces like a constraint-backed index. The auto-name keeps _idx.
func TestCreateUniqueIndexBuild(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE d (id i32 PRIMARY KEY, a i32, n i32)")
	uqRun(t, db, "INSERT INTO d VALUES (1, 7, NULL), (2, 7, NULL), (3, 8, 5)")
	// Build over duplicates fails and registers nothing.
	if code, msg := uqErr(t, db, "CREATE UNIQUE INDEX dup ON d (a)"); code != "23505" || !strings.Contains(msg, "dup") {
		t.Fatalf("build over dups = %s %q", code, msg)
	}
	if got := uqNames(t, db, "d"); len(got) != 0 {
		t.Fatalf("not registered: %v", got)
	}
	// The name is free again (nothing was created).
	uqRun(t, db, "CREATE TABLE dup (x i32)")
	uqRun(t, db, "DROP TABLE dup")
	// NULLs are exempt at build time (two NULLs in n).
	uqRun(t, db, "CREATE UNIQUE INDEX ON d (n)") // d_n_idx — the _idx auto-name
	if got := uqNames(t, db, "d"); !slices.Equal(got, []string{"d_n_idx!"}) {
		t.Fatalf("d indexes = %v", got)
	}
	// ... and it enforces thereafter.
	if code, _ := uqErr(t, db, "INSERT INTO d VALUES (4, 9, 5)"); code != "23505" {
		t.Fatalf("post-build enforcement = %s", code)
	}
	uqRun(t, db, "INSERT INTO d VALUES (4, 9, NULL)")
}

// DROP INDEX of a constraint-backed unique index is allowed and drops the constraint
// (the documented PG divergence — indexes.md §7: jed has no ALTER TABLE, so the index
// name is the constraint's only handle).
func TestDropIndexDropsTheConstraint(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 UNIQUE)")
	uqRun(t, db, "INSERT INTO t VALUES (1, 10)")
	if code, _ := uqErr(t, db, "INSERT INTO t VALUES (2, 10)"); code != "23505" {
		t.Fatalf("enforced = %s", code)
	}
	uqRun(t, db, "DROP INDEX t_v_key")
	uqRun(t, db, "INSERT INTO t VALUES (2, 10)") // no longer enforced
	if got := uqNames(t, db, "t"); len(got) != 0 {
		t.Fatalf("t indexes = %v", got)
	}
}

// Uniqueness validation is unmetered (cost.md §3): an INSERT into a uniquely-indexed
// table still costs 0, and a CREATE UNIQUE INDEX build charges exactly the plain build's
// scan. The planner treats a unique index like any other (the bounded-scan cost).
func TestUniqueCostsAreUnchanged(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)")
	for i := 1; i <= 20; i++ {
		uqRun(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d, %d)", i, i%5, i))
	}
	// The unique build charges the same page_read(1) + 20 rows = 21 as a plain build.
	if c := uqCost(t, db, "CREATE UNIQUE INDEX t_w_idx ON t (w)"); c != 21 {
		t.Fatalf("build cost = %d, want 21", c)
	}
	// INSERT ... VALUES stays zero-cost — the probe is unmetered.
	if c := uqCost(t, db, "INSERT INTO t VALUES (21, 9, 21)"); c != 0 {
		t.Fatalf("insert cost = %d, want 0", c)
	}
	// The unique index bounds a scan exactly like a plain one: 1 index node + 1 point
	// lookup + 1 row + 1 filter eval + 1 produced = 5.
	if c := uqCost(t, db, "SELECT id FROM t WHERE w = 7"); c != 5 {
		t.Fatalf("bounded scan cost = %d, want 5", c)
	}
}

// The v6 round-trip: the unique flag survives serialize -> load (and the reloaded
// database still enforces), and the image is byte-stable across a second serialize.
func TestUniqueRoundTrip(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 UNIQUE, w i32)")
	uqRun(t, db, "CREATE INDEX plain ON t (w)")
	uqRun(t, db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 100)")
	image, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	loaded, err := loadEngine(image)
	if err != nil {
		t.Fatal(err)
	}
	if got := uqNames(t, loaded, "t"); !slices.Equal(got, []string{"plain", "t_v_key!"}) {
		t.Fatalf("loaded indexes = %v", got)
	}
	if code, _ := uqErr(t, loaded, "INSERT INTO t VALUES (3, 10, 1)"); code != "23505" {
		t.Fatalf("loaded enforcement = %s", code)
	}
	uqRun(t, loaded, "INSERT INTO t VALUES (3, NULL, 1)")
	again, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(image, again) {
		t.Fatal("image not byte-stable")
	}
}

// Transactional DDL: a UNIQUE created inside a rolled-back block leaves no trace — no
// definition, no store, no enforcement (the §3 snapshot model).
func TestUniqueTransactionalDDLRollsBack(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	uqRun(t, db, "INSERT INTO t VALUES (1, 10)")
	uqRun(t, db, "BEGIN")
	uqRun(t, db, "CREATE UNIQUE INDEX u ON t (v)")
	if code, _ := uqErr(t, db, "INSERT INTO t VALUES (2, 10)"); code != "23505" {
		t.Fatalf("in-tx enforcement = %s", code)
	}
	uqRun(t, db, "ROLLBACK")
	if got := uqNames(t, db, "t"); len(got) != 0 {
		t.Fatalf("t indexes after rollback = %v", got)
	}
	uqRun(t, db, "INSERT INTO t VALUES (2, 10)") // not enforced after rollback
}
