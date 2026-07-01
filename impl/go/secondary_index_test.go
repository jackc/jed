package jed

// Secondary indexes (spec/design/indexes.md) — covers what the corpus suite
// (ddl/create_index.test, query/index_scan.test) cannot: catalog introspection (index
// definitions, name order), the v5 on-disk round-trip with index trees, the file-backed
// paged-open + incremental-commit path, and transactional DDL. Mirrored in
// impl/rust/tests/secondary_index.rs and impl/ts/tests/secondary_index.test.ts.

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"testing"
)

func siRun(t *testing.T, db dbHandle, sql string) Outcome {
	t.Helper()
	o, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return o
}

func siCost(t *testing.T, db dbHandle, sql string) int64 {
	t.Helper()
	return siRun(t, db, sql).Cost
}

func siIds(t *testing.T, db dbHandle, sql string) []int64 {
	t.Helper()
	o := siRun(t, db, sql)
	out := make([]int64, 0, len(o.Rows))
	for _, r := range o.Rows {
		if r[0].Kind != ValInt {
			t.Fatalf("expected an int id, got %v", r[0])
		}
		out = append(out, r[0].Int)
	}
	return out
}

func siErr(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := db.Execute(sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected an EngineError from %q, got %v", sql, err)
	}
	return ee.State.Code()
}

// siDB20 is the 20-row fixture the planner/cost tests run against: v = i % 5 gives 4
// rows per value, so an equality admits 4 of 20.
func siDB20(t *testing.T) *Session {
	t.Helper()
	db := NewDatabase().Session(SessionOptions{})
	siRun(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)")
	for i := 1; i <= 20; i++ {
		siRun(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d, %d)", i, i%5, i))
	}
	return db
}

// Auto-naming matches PostgreSQL (oracle-probed, indexes.md §2): lowercased
// <table>_<cols>_idx + the smallest free suffix; duplicates in the column list are
// allowed and named through; an explicit name round-trips as written. The catalog holds
// indexes in ascending lowercased-name order.
func TestIndexAutoNamingMatchesPostgres(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	siRun(t, db, "CREATE TABLE T (A i32 PRIMARY KEY, B i32)")
	siRun(t, db, "CREATE INDEX ON T (B)")    // t_b_idx
	siRun(t, db, "CREATE INDEX ON T (B)")    // t_b_idx1
	siRun(t, db, "CREATE INDEX ON T (B)")    // t_b_idx2
	siRun(t, db, "CREATE INDEX ON T (A, B)") // t_a_b_idx
	siRun(t, db, "CREATE INDEX ON T (B, B)") // t_b_b_idx (duplicate column allowed — PG)
	siRun(t, db, "CREATE INDEX Mine ON T (B)")
	tab, _ := db.Table("t")
	names := make([]string, 0, len(tab.Indexes))
	for _, ix := range tab.Indexes {
		names = append(names, ix.Name)
	}
	want := []string{"Mine", "t_a_b_idx", "t_b_b_idx", "t_b_idx", "t_b_idx1", "t_b_idx2"}
	if !slices.Equal(names, want) {
		t.Fatalf("index names = %v, want %v", names, want)
	}
	if !slices.Equal(tab.Indexes[1].Columns, []int{0, 1}) || !slices.Equal(tab.Indexes[2].Columns, []int{1, 1}) {
		t.Fatalf("index columns wrong: %v / %v", tab.Indexes[1].Columns, tab.Indexes[2].Columns)
	}
	if !slices.Equal(tab.PKIndices(), []int{0}) {
		t.Fatalf("pk = %v, want [0]", tab.PKIndices())
	}
}

// DDL errors mirror PostgreSQL (oracle-probed, indexes.md §2): validation order is
// table → columns (list order) → name collision; the relation namespace is shared with
// tables; DROP mismatches are 42704/42809.
func TestIndexDDLErrorsMatchPostgres(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	siRun(t, db, "CREATE TABLE t (a i32 PRIMARY KEY, s f64)")
	if got := siErr(t, db, "CREATE INDEX i ON nosuch (nope)"); got != "42P01" {
		t.Fatalf("missing table: %s", got)
	}
	siRun(t, db, "CREATE INDEX taken ON t (a)")
	if got := siErr(t, db, "CREATE INDEX taken ON t (nope)"); got != "42703" {
		t.Fatalf("bad column before name collision: %s", got)
	}
	// f64 IS now a valid index column (the float-order-preserving key, encoding.md §2.8 — every
	// scalar is keyable; text/bytea covered in ddl/create_index.test).
	siRun(t, db, "CREATE INDEX i ON t (s)")
	if got := siErr(t, db, "CREATE INDEX taken ON t (a)"); got != "42P07" {
		t.Fatalf("dup index name: %s", got)
	}
	if got := siErr(t, db, "CREATE INDEX t ON t (a)"); got != "42P07" {
		t.Fatalf("index name vs table: %s", got)
	}
	if got := siErr(t, db, "CREATE TABLE taken (x i32)"); got != "42P07" {
		t.Fatalf("table name vs index: %s", got)
	}
	if got := siErr(t, db, "DROP INDEX nosuch"); got != "42704" {
		t.Fatalf("drop missing index: %s", got)
	}
	if got := siErr(t, db, "DROP INDEX t"); got != "42809" {
		t.Fatalf("drop index on a table: %s", got)
	}
	if got := siErr(t, db, "DROP TABLE taken"); got != "42809" {
		t.Fatalf("drop table on an index: %s", got)
	}
	siRun(t, db, "DROP INDEX taken")
	if got := siErr(t, db, "DROP INDEX taken"); got != "42704" {
		t.Fatalf("re-drop: %s", got)
	}
	siRun(t, db, "CREATE INDEX taken ON t (a)")
	siRun(t, db, "DROP TABLE t")
	siRun(t, db, "CREATE TABLE taken (x i32)") // DROP TABLE freed its index names
	// The lookahead keeps every word non-reserved (grammar.md §30): the unnamed form
	// over a table named `on`, and an index explicitly named `on`.
	siRun(t, db, "CREATE TABLE on (x i32)")
	siRun(t, db, "CREATE INDEX ON on (x)")
	onTab, _ := db.Table("on")
	if onTab.Indexes[0].Name != "on_x_idx" {
		t.Fatalf("auto-name over table on: %s", onTab.Indexes[0].Name)
	}
	siRun(t, db, "DROP TABLE on") // free the name `on` in the relation namespace
	siRun(t, db, "CREATE TABLE q (x i32)")
	siRun(t, db, "CREATE INDEX on ON q (x)")
	qTab, _ := db.Table("q")
	if qTab.Indexes[0].Name != "on" {
		t.Fatalf("index named on: %s", qTab.Indexes[0].Name)
	}
	siRun(t, db, "DROP INDEX on")
}

// The planner picks the index for a first-column equality and the cost drops to the
// index-bounded form (cost.md §3 "index-bounded scan"); a provably-empty bound reads
// nothing; the PK bound wins over an index; the lowest-named index breaks ties.
func TestIndexPlannerCostsArePinned(t *testing.T) {
	db := siDB20(t)
	pin := func(sql string, want int64) {
		t.Helper()
		if got := siCost(t, db, sql); got != want {
			t.Fatalf("%q cost = %d, want %d", sql, got, want)
		}
	}
	pin("SELECT id FROM t WHERE v = 3", 45) // full scan
	pin("CREATE INDEX t_v_idx ON t (v)", 21)
	pin("SELECT id FROM t WHERE v = 3", 17) // index-bounded
	if got := siIds(t, db, "SELECT id FROM t WHERE v = 3 ORDER BY id"); !slices.Equal(got, []int64{3, 8, 13, 18}) {
		t.Fatalf("ids = %v", got)
	}
	pin("SELECT id FROM t WHERE v = NULL", 0)         // 3VL-empty
	pin("SELECT id FROM t WHERE v = 1 AND v = 2", 0)  // contradiction
	pin("SELECT id FROM t WHERE id = 7 AND v = 2", 6) // the PK bound wins
	siRun(t, db, "CREATE INDEX two ON t (w, v)")
	pin("SELECT id FROM t WHERE v = 3", 17) // t_v_idx still serves v
	siRun(t, db, "DROP INDEX t_v_idx")
	pin("SELECT id FROM t WHERE v = 3", 45) // `two` cannot serve a non-first column
	// First-column equality on the composite index works (the entry's tail component is
	// skipped to reach the row key); the lowest lowercased name wins a tie.
	pin("SELECT id FROM t WHERE w = 7", 5)
	siRun(t, db, "CREATE INDEX a_first ON t (w)")
	pin("SELECT id FROM t WHERE w = 7", 5)
	siRun(t, db, "DROP INDEX a_first")
	siRun(t, db, "DROP INDEX two")
	pin("SELECT id FROM t WHERE w = 7", 42) // full scan again
}

// The v5 image round-trips: index trees (including a NULL entry), the out-of-order PK
// list, and a second-generation serialize are byte-stable; a reloaded database still
// uses (and maintains) its indexes.
func TestIndexRoundTripsThroughTheImage(t *testing.T) {
	db := siDB20(t)
	siRun(t, db, "CREATE INDEX t_v_idx ON t (v)")
	siRun(t, db, "INSERT INTO t VALUES (100, NULL, 0)")
	img, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	loaded, err := loadEngine(img)
	if err != nil {
		t.Fatal(err)
	}
	img2, err := loaded.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(img, img2) {
		t.Fatal("reload is not byte-stable")
	}
	tab, _ := loaded.Table("t")
	if len(tab.Indexes) != 1 || tab.Indexes[0].Name != "t_v_idx" || !slices.Equal(tab.Indexes[0].Columns, []int{1}) {
		t.Fatalf("reloaded indexes = %+v", tab.Indexes)
	}
	if got := siCost(t, loaded, "SELECT id FROM t WHERE v = 3"); got != 17 {
		t.Fatalf("reloaded index scan cost = %d, want 17", got)
	}
	siRun(t, loaded, "UPDATE t SET v = 3 WHERE id = 100")
	if got := siIds(t, loaded, "SELECT id FROM t WHERE v = 3 ORDER BY id"); !slices.Equal(got, []int64{3, 8, 13, 18, 100}) {
		t.Fatalf("ids after reload mutation = %v", got)
	}
}

// Index DDL is transactional (transactions.md §4.5): a CREATE INDEX inside a rolled-back
// block vanishes (definition and store), and one inside a committed block persists.
func TestIndexDDLIsTransactional(t *testing.T) {
	db := siDB20(t)
	siRun(t, db, "BEGIN")
	siRun(t, db, "CREATE INDEX t_v_idx ON t (v)")
	if got := siCost(t, db, "SELECT id FROM t WHERE v = 3"); got != 17 {
		t.Fatalf("in-tx index scan cost = %d, want 17", got)
	}
	siRun(t, db, "ROLLBACK")
	tab, _ := db.Table("t")
	if len(tab.Indexes) != 0 {
		t.Fatalf("rolled-back index survived: %+v", tab.Indexes)
	}
	if got := siCost(t, db, "SELECT id FROM t WHERE v = 3"); got != 45 {
		t.Fatalf("post-rollback cost = %d, want 45", got)
	}
	siRun(t, db, "BEGIN")
	siRun(t, db, "CREATE INDEX t_v_idx ON t (v)")
	siRun(t, db, "COMMIT")
	if got := siCost(t, db, "SELECT id FROM t WHERE v = 3"); got != 17 {
		t.Fatalf("post-commit cost = %d, want 17", got)
	}
}

// File-backed: an index survives the incremental commit + demand-paged reopen
// (format.md "Allocation & incremental commit"; pager.md), keeps the same pinned scan
// cost (page_read is logical — buffer-pool-invisible), and stays maintainable across
// commits.
func TestIndexFileBackedPagedReopen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "secondary_index_paged.jed")
	db, err := create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	siRun(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)")
	for i := 1; i <= 20; i++ {
		siRun(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d, %d)", i, i%5, i))
	}
	siRun(t, db, "CREATE INDEX t_v_idx ON t (v)")
	inMemoryCost := siCost(t, db, "SELECT id FROM t WHERE v = 3")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := siCost(t, reopened, "SELECT id FROM t WHERE v = 3"); got != inMemoryCost {
		t.Fatalf("paged index scan cost = %d, want %d", got, inMemoryCost)
	}
	if got := siIds(t, reopened, "SELECT id FROM t WHERE v = 3 ORDER BY id"); !slices.Equal(got, []int64{3, 8, 13, 18}) {
		t.Fatalf("paged ids = %v", got)
	}
	siRun(t, reopened, "UPDATE t SET v = 3 WHERE id = 4")
	siRun(t, reopened, "DELETE FROM t WHERE id = 13")
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
	again, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := siIds(t, again, "SELECT id FROM t WHERE v = 3 ORDER BY id"); !slices.Equal(got, []int64{3, 4, 8, 18}) {
		t.Fatalf("ids after incremental commits = %v", got)
	}
	if err := again.Close(); err != nil {
		t.Fatal(err)
	}
	_ = os.Remove(path)
}

// The CREATE INDEX build scan honors the cost ceiling (CLAUDE.md §13): a ceiling below
// the build cost aborts deterministically with 54P01 and registers nothing.
func TestCreateIndexHonorsTheCostCeiling(t *testing.T) {
	db := siDB20(t)
	db.SetMaxCost(10) // the build scan costs 21
	if got := siErr(t, db, "CREATE INDEX t_v_idx ON t (v)"); got != "54P01" {
		t.Fatalf("ceiling abort: %s", got)
	}
	db.SetMaxCost(0)
	tab, _ := db.Table("t")
	if len(tab.Indexes) != 0 {
		t.Fatalf("aborted CREATE INDEX registered: %+v", tab.Indexes)
	}
	siRun(t, db, "CREATE INDEX t_v_idx ON t (v)")
}
