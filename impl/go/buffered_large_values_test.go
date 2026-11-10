package jed

// Large values held by reference in eager-lane buffers (spec/design/large-values.md §16). A touched
// text column that is not a key column stays an unfetched reference in the eager lane's relation
// buffer and resolves only when an expression reads it. With every chain payload corrupt on disk,
// a query raises XX001 exactly when an expression reads a corrupt value, and succeeds when none
// does. Mirrors impl/rust/tests/buffered_large_values.rs and
// impl/ts/tests/buffered_large_values.test.ts. Uses lazySeed / corruptOverflowPayloads.

import (
	"fmt"
	"path/filepath"
	"slices"
	"testing"
)

// bufferedCases are queries over lazySeed's table, whose rows 1 and 2 hold the corrupt chains. ok
// lists the expected rows (rendered, sorted) when no expression reads a corrupt value; nil expects
// XX001.
var bufferedCases = []struct {
	sql string
	ok  []string
}{
	// Aggregate: the CASE reads body only for the intact rows 3 and 4, the FILTER only for them.
	{"SELECT sum(CASE WHEN id > 2 THEN length(body) ELSE 0 END) FROM t", []string{"604"}},
	{"SELECT count(body) FILTER (WHERE id > 2) FROM t", []string{"2"}},
	{"SELECT sum(length(body)) FROM t", nil},
	// Join: b.body is no join key, so it is read only where the CASE takes it.
	{
		"SELECT a.id, CASE WHEN a.id > 2 THEN length(b.body) END FROM t a JOIN t b ON a.id = b.id",
		[]string{"1|NULL", "2|NULL", "3|600", "4|4"},
	},
	{"SELECT a.id, length(b.body) FROM t a JOIN t b ON a.id = b.id", nil},
	// Window: the window reads only its key and the projection reads body only for rows 3 and 4.
	{
		"SELECT id, CASE WHEN id > 2 THEN length(body) END, row_number() OVER (ORDER BY id) FROM t",
		[]string{"1|NULL|1", "2|NULL|2", "3|600|3", "4|4|4"},
	},
	{"SELECT id, lag(length(body)) OVER (ORDER BY id) FROM t", nil},
	// A correlated subquery reads o.body into its primary-key bound without evaluating it, so body is
	// a key column: resolved when t is scanned, the corrupt rows raise XX001.
	{"SELECT id, (SELECT count(*) FROM u WHERE u.k = o.body), row_number() OVER (ORDER BY id) FROM t o", nil},
}

// bufferedSeed is lazySeed plus a text-keyed table the correlated case probes.
func bufferedSeed(t *testing.T, db dbHandle) {
	t.Helper()
	lazySeed(t, db)
	mustExec(t, db, "CREATE TABLE u (k text PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO u VALUES ('tiny')")
}

func renderSorted(rows [][]Value) []string {
	out := make([]string, len(rows))
	for i, r := range rows {
		for j, v := range r {
			if j > 0 {
				out[i] += "|"
			}
			out[i] += v.Render()
		}
	}
	slices.Sort(out)
	return out
}

func TestBufferedLargeValuesResolveWhenRead(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "buffered.jed")
	db, err := create(path, databaseOptions{PageSize: lazyPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	bufferedSeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	corruptOverflowPayloads(t, path)
	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	// An unlimited work_mem keeps aggregation and hash joins in the eager lane (memory.md §5.4).
	db.SetWorkMem(0)
	for _, c := range bufferedCases {
		out, err := queryOutcome(db, c.sql, nil)
		if c.ok == nil {
			if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
				t.Fatalf("%s: want XX001, got %v", c.sql, err)
			}
			continue
		}
		if err != nil {
			t.Fatalf("%s: %v", c.sql, err)
		}
		if got := renderSorted(out.Rows); !slices.Equal(got, c.ok) {
			t.Fatalf("%s: got %v, want %v", c.sql, got, c.ok)
		}
	}
}

// The same queries cost the same and return the same rows on disk-read rows (buffered references)
// as on resident rows.
func TestBufferedLargeValuesMatchResident(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "buffered_intact.jed")
	disk, err := create(path, databaseOptions{PageSize: lazyPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	bufferedSeed(t, disk)
	if err := disk.Close(); err != nil {
		t.Fatal(err)
	}
	disk, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer disk.Close()
	disk.SetWorkMem(0)
	resident, err := create(filepath.Join(t.TempDir(), "buffered_resident.jed"), databaseOptions{PageSize: lazyPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer resident.Close()
	resident.SetWorkMem(0)
	mustExec(t, resident, "BEGIN")
	bufferedSeed(t, resident)
	for _, c := range bufferedCases {
		a, aerr := queryOutcome(disk, c.sql, nil)
		b, berr := queryOutcome(resident, c.sql, nil)
		if aerr != nil || berr != nil {
			if c.ok == nil && aerr != nil {
				continue
			}
			t.Fatalf("%s: %v / %v", c.sql, aerr, berr)
		}
		if fmt.Sprint(renderSorted(a.Rows)) != fmt.Sprint(renderSorted(b.Rows)) || a.Cost != b.Cost {
			t.Fatalf("%s: disk %v cost %d, resident %v cost %d", c.sql, renderSorted(a.Rows), a.Cost, renderSorted(b.Rows), b.Cost)
		}
	}
}
