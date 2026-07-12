package jed

import (
	"bytes"
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
