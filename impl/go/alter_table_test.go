package jed

import (
	"bytes"
	"slices"
	"testing"
)

func TestAlterAddColumnRewriteMatchesFreshTableBytes(t *testing.T) {
	t.Parallel()
	altered := memDB().Session(SessionOptions{})
	uqRun(t, altered, "CREATE TABLE t (id i32 PRIMARY KEY)")
	uqRun(t, altered, "INSERT INTO t VALUES (1), (2)")
	uqRun(t, altered, "ALTER TABLE t ADD v i32 DEFAULT 7")

	fresh := memDB().Session(SessionOptions{})
	uqRun(t, fresh, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)")
	uqRun(t, fresh, "INSERT INTO t (id) VALUES (1), (2)")

	a, err := altered.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	b, err := fresh.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(a, b) {
		t.Fatal("ADD COLUMN rewrite differs from an equivalent fresh table")
	}
}

func TestAlterDropColumnRewriteMatchesFreshTableBytes(t *testing.T) {
	t.Parallel()
	altered := memDB().Session(SessionOptions{})
	uqRun(t, altered, "CREATE TABLE t (obsolete text, id i32 PRIMARY KEY, v i32 DEFAULT 7)")
	uqRun(t, altered, "INSERT INTO t VALUES ('a', 1, 7), ('b', 2, 8)")
	uqRun(t, altered, "ALTER TABLE t DROP obsolete")

	fresh := memDB().Session(SessionOptions{})
	uqRun(t, fresh, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)")
	uqRun(t, fresh, "INSERT INTO t VALUES (1, 7), (2, 8)")

	a, err := altered.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	b, err := fresh.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(a, b) {
		t.Fatal("DROP COLUMN rewrite differs from an equivalent fresh table")
	}
}

func TestAlterTypeAndPrimaryKeyRewritesMatchFreshTableBytes(t *testing.T) {
	t.Parallel()
	image := func(sqls ...string) []byte {
		db := memDB().Session(SessionOptions{})
		for _, sql := range sqls {
			uqRun(t, db, sql)
		}
		out, err := db.ToImage(8192, 1)
		if err != nil {
			t.Fatal(err)
		}
		return out
	}
	if !bytes.Equal(
		image("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1, 2), (2, 3)", "ALTER TABLE t ALTER v TYPE i64 USING v + 10"),
		image("CREATE TABLE t (id i32 PRIMARY KEY, v i64)", "INSERT INTO t VALUES (1, 12), (2, 13)"),
	) {
		t.Fatal("ALTER TYPE rewrite differs from an equivalent fresh table")
	}
	if !bytes.Equal(
		image("CREATE TABLE t (id i32 NOT NULL, v text)", "INSERT INTO t VALUES (2, 'b'), (1, 'a')", "ALTER TABLE t ADD PRIMARY KEY (id)", "ALTER TABLE t DROP PRIMARY KEY"),
		image("CREATE TABLE t (id i32 NOT NULL, v text)", "INSERT INTO t VALUES (1, 'a'), (2, 'b')"),
	) {
		t.Fatal("ADD/DROP PRIMARY KEY rewrite differs from an equivalent fresh table")
	}
}

// A RENAME rewrite substitutes the new name into persisted expression text as a QUOTED identifier
// (grammar.md §3, alter.md §2.2) — even a plain lowercase name — while untouched tokens keep their
// stored (folded) spelling, and the text survives a serialize → load round trip. The corpus
// (ddl/alter_table.test) pins the behavior; this pins the persisted bytes of the text.
func TestRenameRewriteQuotesTheSubstitutedName(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	uqRun(t, db, "CREATE TABLE r (a i32 CHECK (a > 0), b i32 CHECK (B IS NULL OR b > a))")
	uqRun(t, db, `ALTER TABLE r RENAME COLUMN a TO "null"`)
	uqRun(t, db, "ALTER TABLE r RENAME COLUMN b TO plain")
	want := []string{`"null" > 0`, `"plain" is null or "plain" > "null"`}
	texts := func(tbl *catTable) []string {
		var out []string
		for _, c := range tbl.Checks {
			out = append(out, c.ExprText)
		}
		return out
	}
	tbl, _ := db.Table("r")
	if got := texts(tbl); !slices.Equal(got, want) {
		t.Fatalf("check texts = %q, want %q", got, want)
	}
	image, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	loaded, err := loadEngine(image)
	if err != nil {
		t.Fatal(err)
	}
	ltbl, _ := loaded.Table("r")
	if got := texts(ltbl); !slices.Equal(got, want) {
		t.Fatalf("reloaded check texts = %q, want %q", got, want)
	}
}
