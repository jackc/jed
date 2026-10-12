package jed

// Views through the host API (spec/design/views.md). The corpus pins view semantics through the
// materialized execute path; these per-core tests pin only what it cannot express: a prepared
// statement over a view never serves a stale plan (across OR REPLACE and renames too), the lazy Query
// lanes classify a read of a nextval view as a write, function EXECUTE is checked against the reader
// through a view body, views survive a file commit + reopen + compaction, and the dependency-order
// accessor the CLI dump uses. Mirrors impl/rust/tests/views.rs.

import (
	"path/filepath"
	"reflect"
	"testing"
)

func viewsExec(t *testing.T, s *Session, sql string) {
	t.Helper()
	if _, err := queryOutcome(s, sql, nil); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

func viewsCode(t *testing.T, s *Session, sql string) string {
	t.Helper()
	_, err := queryOutcome(s, sql, nil)
	if err == nil {
		t.Fatalf("%q: expected an error", sql)
	}
	return err.(*EngineError).Code()
}

// A prepared statement over a view re-plans after the view is dropped and recreated with another
// definition, and after its underlying table changes (views.md §4: a view expands at every planning,
// so it is never served from a stale plan).
func TestPreparedViewQueryNeverServesAStalePlan(t *testing.T) {
	s := memDB().Session(SessionOptions{})
	viewsExec(t, s, "CREATE TABLE t (a i32 PRIMARY KEY, b i32)")
	viewsExec(t, s, "INSERT INTO t VALUES (1, 10), (2, 20)")
	viewsExec(t, s, "CREATE VIEW v AS SELECT a, b FROM t WHERE a = 1")
	stmt, err := s.Prepare("SELECT b FROM v")
	if err != nil {
		t.Fatal(err)
	}
	check := func(want int64) {
		t.Helper()
		rows, _ := drainQ(t, s, stmt)
		if !planCacheRowsEq(rows, [][]int64{{want}}) {
			t.Fatalf("rows = %v, want [[%d]]", rows, want)
		}
	}
	check(10)
	check(10)

	viewsExec(t, s, "DROP VIEW v")
	viewsExec(t, s, "CREATE VIEW v AS SELECT a, b FROM t WHERE a = 2")
	check(20)

	viewsExec(t, s, "UPDATE t SET b = 21 WHERE a = 2")
	check(21)

	viewsExec(t, s, "ALTER TABLE t RENAME COLUMN b TO bb")
	check(21)

	viewsExec(t, s, "DROP VIEW v")
	rows, err := s.queryStmt(stmt.ast, nil, &stmt.sc, &stmt.ic)
	if err == nil {
		for rows.Next() {
		}
		err = rows.Err()
		rows.Close()
	}
	if err == nil || err.(*EngineError).Code() != "42P01" {
		t.Fatalf("query over a dropped view: want 42P01, got %v", err)
	}
}

// A prepared statement over a view follows CREATE OR REPLACE VIEW and a view rename (views.md
// §1.2/§7.1): the replacement's rows come back on the next execution, and a renamed view is gone under
// its old name while a view that reads it keeps working.
func TestPreparedViewQueryFollowsReplaceAndRename(t *testing.T) {
	s := memDB().Session(SessionOptions{})
	viewsExec(t, s, "CREATE TABLE t (a i32 PRIMARY KEY, b i32)")
	viewsExec(t, s, "INSERT INTO t VALUES (1, 10), (2, 20)")
	viewsExec(t, s, "CREATE VIEW v AS SELECT a, b FROM t WHERE a = 1")
	viewsExec(t, s, "CREATE VIEW w AS SELECT b FROM v")
	direct, err := s.Prepare("SELECT b FROM v")
	if err != nil {
		t.Fatal(err)
	}
	through, err := s.Prepare("SELECT b FROM w")
	if err != nil {
		t.Fatal(err)
	}
	check := func(stmt *PreparedStatement, want int64) {
		t.Helper()
		rows, _ := drainQ(t, s, stmt)
		if !planCacheRowsEq(rows, [][]int64{{want}}) {
			t.Fatalf("rows = %v, want [[%d]]", rows, want)
		}
	}
	check(direct, 10)
	check(through, 10)

	viewsExec(t, s, "CREATE OR REPLACE VIEW v AS SELECT a, b, a + b AS c FROM t WHERE a = 2")
	check(direct, 20)
	check(through, 20)

	viewsExec(t, s, "ALTER VIEW v RENAME TO v2")
	rows, err := s.queryStmt(direct.ast, nil, &direct.sc, &direct.ic)
	if err == nil {
		for rows.Next() {
		}
		err = rows.Err()
		rows.Close()
	}
	if err == nil || err.(*EngineError).Code() != "42P01" {
		t.Fatalf("query over a renamed view: want 42P01, got %v", err)
	}
	check(through, 20)

	viewsExec(t, s, "ALTER VIEW v2 RENAME COLUMN b TO bb")
	check(through, 20)
}

// The lazy Query lanes classify a read of a view whose body calls nextval as a write (views.md §4.1,
// sequences.md §4): inside a READ ONLY block it is 25006, and in autocommit it advances the sequence
// exactly once per read.
func TestQueryLaneSeesANextvalViewAsAWrite(t *testing.T) {
	s := memDB().Session(SessionOptions{})
	viewsExec(t, s, "CREATE SEQUENCE s")
	viewsExec(t, s, "CREATE VIEW vs AS SELECT nextval('s') AS n")
	query := func(sql string) ([][]Value, error) {
		rows, err := s.queryValues(sql, nil)
		if err != nil {
			return nil, err
		}
		defer rows.Close()
		var out [][]Value
		for rows.Next() {
			out = append(out, append([]Value(nil), rows.Row()...))
		}
		return out, rows.Err()
	}
	if got, err := query("SELECT n FROM vs"); err != nil || !reflect.DeepEqual(got, [][]Value{{IntValue(1)}}) {
		t.Fatalf("first read = %v, %v", got, err)
	}
	viewsExec(t, s, "CREATE VIEW vvs AS SELECT n FROM vs")
	if got, err := query("SELECT n FROM vvs"); err != nil || !reflect.DeepEqual(got, [][]Value{{IntValue(2)}}) {
		t.Fatalf("second read = %v, %v", got, err)
	}

	viewsExec(t, s, "BEGIN READ ONLY")
	if _, err := query("SELECT n FROM vvs"); err == nil || err.(*EngineError).Code() != "25006" {
		t.Fatalf("a nextval view read in a READ ONLY block: want 25006, got %v", err)
	}
	viewsExec(t, s, "ROLLBACK")
	// A CTE of the same name shadows the view, so this read is not a write.
	viewsExec(t, s, "BEGIN READ ONLY")
	if got, err := query("WITH vs AS (SELECT 7 AS n) SELECT n FROM vs"); err != nil || !reflect.DeepEqual(got, [][]Value{{IntValue(7)}}) {
		t.Fatalf("shadowing CTE read = %v, %v", got, err)
	}
	viewsExec(t, s, "ROLLBACK")
}

// Reading a view needs SELECT on the view only, but the functions its body calls need the reader's
// EXECUTE, as in a derived table (views.md §9).
func TestViewBodyFunctionsNeedTheReadersExecute(t *testing.T) {
	s := memDB().Session(SessionOptions{})
	viewsExec(t, s, "CREATE TABLE t (a i32 PRIMARY KEY)")
	viewsExec(t, s, "INSERT INTO t VALUES (-5)")
	viewsExec(t, s, "CREATE VIEW v AS SELECT abs(a) AS x FROM t")
	viewsExec(t, s, "CREATE VIEW w AS SELECT x FROM v")
	if got := queryRows(t, s, "SELECT x FROM w"); !reflect.DeepEqual(got, [][]Value{{IntValue(5)}}) {
		t.Fatalf("SELECT x FROM w = %v", got)
	}
	s.Revoke(PrivSetEmpty.With(PrivExecute), "abs")
	if code := viewsCode(t, s, "SELECT x FROM w"); code != "42501" {
		t.Fatalf("SELECT x FROM w after revoke: %s", code)
	}
	if code := viewsCode(t, s, "SELECT x FROM v"); code != "42501" {
		t.Fatalf("SELECT x FROM v after revoke: %s", code)
	}
}

// Views persist through a file commit and reopen, keep their stored text, and still read; a rename
// rewrite made in one session is what the next open sees; compaction keeps them (views.md §3/§7,
// api.md §2.6).
func TestViewsSurviveCommitReopenAndCompaction(t *testing.T) {
	path := filepath.Join(t.TempDir(), "views.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	viewsExec(t, s, "CREATE TABLE t (a i32 PRIMARY KEY, b text)")
	viewsExec(t, s, "INSERT INTO t VALUES (1, 'x'), (2, 'y')")
	viewsExec(t, s, "CREATE VIEW v AS SELECT * FROM t WHERE a > 1")
	viewsExec(t, s, "CREATE VIEW w (k) AS SELECT a FROM v")
	viewsExec(t, s, "ALTER TABLE t RENAME TO t2")
	s.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = OpenDatabase(path)
	if err != nil {
		t.Fatal(err)
	}
	s = db.Session(SessionOptions{})
	got := queryRows(t, s, "SELECT name, definition FROM jed_views ORDER BY name")
	want := [][]Value{
		{TextValue("v"), TextValue(`select "t" . "a" , "t" . "b" from "t2" as "t" where a > 1`)},
		{TextValue("w"), TextValue("select a from v")},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("jed_views = %v, want %v", got, want)
	}
	if got := queryRows(t, s, "SELECT k FROM w"); !reflect.DeepEqual(got, [][]Value{{IntValue(2)}}) {
		t.Fatalf("SELECT k FROM w = %v", got)
	}
	if code := viewsCode(t, s, "DROP TABLE t2"); code != "2BP01" {
		t.Fatalf("DROP TABLE t2: %s", code)
	}
	s.Close()
	// Compaction streams the from-scratch image, view entries included (api.md §2.6).
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = OpenDatabase(path)
	if err != nil {
		t.Fatal(err)
	}
	s = db.Session(SessionOptions{})
	if got := queryRows(t, s, "SELECT k FROM w"); !reflect.DeepEqual(got, [][]Value{{IntValue(2)}}) {
		t.Fatalf("SELECT k FROM w after compaction = %v", got)
	}
	s.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

// The dependency-order accessor lists views in dependency order, ties in name byte order (views.md
// §11).
func TestViewsInDependencyOrder(t *testing.T) {
	s := memDB().Session(SessionOptions{})
	viewsExec(t, s, "CREATE TABLE t (a i32 PRIMARY KEY)")
	viewsExec(t, s, "CREATE VIEW z AS SELECT a FROM t")
	viewsExec(t, s, "CREATE VIEW b AS SELECT a FROM z")
	viewsExec(t, s, "CREATE VIEW a AS SELECT a FROM b")
	viewsExec(t, s, "CREATE VIEW m AS SELECT a FROM t")
	var order []string
	for _, v := range s.engine.viewsInDependencyOrder() {
		order = append(order, v.Name)
	}
	if !reflect.DeepEqual(order, []string{"m", "z", "b", "a"}) {
		t.Fatalf("order = %v", order)
	}
}

// A file containing views can be attached, but its views are unreachable through the attachment and
// kept intact: a read through the qualifier is 0A000, a write 55000, CREATE VIEW in the attachment
// 0A000, and any ALTER TABLE of a table one of its views reads 0A000 (views.md §10).
func TestAttachedDatabaseViews(t *testing.T) {
	t.Parallel()
	ref := makeFileDB(t, t.TempDir(), "ref.jed", 0,
		"CREATE TABLE t (a i32 PRIMARY KEY, b i32)",
		"INSERT INTO t VALUES (1, 10)",
		"CREATE TABLE u (k i32 PRIMARY KEY)",
		"CREATE VIEW v AS SELECT a FROM t")

	db := memDB()
	if err := db.Attach("reports", AttachFile(ref), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	defer db.Detach("reports")
	s := db.Session(SessionOptions{})
	defer s.Close()
	if got := queryRows(t, s, "SELECT a FROM reports.t"); !reflect.DeepEqual(got, [][]Value{{IntValue(1)}}) {
		t.Fatalf("SELECT a FROM reports.t = %v", got)
	}
	if got := queryRows(t, s, "SELECT name FROM reports.jed_views"); !reflect.DeepEqual(got, [][]Value{{TextValue("v")}}) {
		t.Fatalf("reports.jed_views = %v", got)
	}
	for sql, want := range map[string]string{
		"SELECT * FROM reports.v":                     "0A000",
		"INSERT INTO reports.v VALUES (2)":            "0A000",
		"DELETE FROM reports.v":                       "0A000",
		"CREATE VIEW reports.w AS SELECT 1 AS one":    "0A000",
		"CREATE VIEW w AS SELECT a FROM reports.t":    "0A000",
		"DROP VIEW reports.v":                         "0A000",
		"CREATE TABLE reports.v (x i32)":              "42P07",
		"ALTER TABLE reports.t ADD COLUMN z i32":      "0A000",
		"ALTER TABLE reports.t RENAME COLUMN b TO bb": "0A000",
	} {
		if code := viewsCode(t, s, sql); code != want {
			t.Errorf("%q: want %s, got %s", sql, want, code)
		}
	}
	// A table no view reads alters normally.
	viewsExec(t, s, "ALTER TABLE reports.u ADD COLUMN z i32")
}
