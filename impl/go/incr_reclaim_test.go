package jed

import (
	"fmt"
	"path/filepath"
	"testing"
)

// Under incremental reclamation (incr_reclaim.go) a pinned reader holds back only the
// generations listed after its version. Its snapshot stays intact while the writer churns, the writer
// still reuses pages listed before the pin, and once the reader closes the next commits merge the held
// generations and stop growing the file.
func TestIncrReclaimReaderHoldsOnlyNewerGenerations(t *testing.T) {
	if !incrOn() {
		t.Skip("a Go-only prototype that disables incremental reclamation is on")
	}
	path := filepath.Join(t.TempDir(), "incr.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 256, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	execDB(t, db, "CREATE TABLE t (id i64 PRIMARY KEY, v text)")
	execDB(t, db, "INSERT INTO t SELECT g, repeat('a', 30) FROM generate_series(1, 200) g")
	for i := 0; i < 50; i++ {
		execDB(t, db, fmt.Sprintf("UPDATE t SET v = repeat('b', 30) WHERE id = %d", 1+i%200))
	}
	st := db.core.storage
	steady := db.PageCount()
	rd := db.ReadSession()
	pinned, err := countOf(db, rd)
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 300; i++ {
		execDB(t, db, fmt.Sprintf("UPDATE t SET v = repeat('c', 30) WHERE id = %d", 1+i%200))
		if i%50 == 0 {
			execDB(t, db, fmt.Sprintf("DELETE FROM t WHERE id = %d", 1000+i)) // no-op delete: an empty commit
		}
	}
	st.mu.Lock()
	gens := len(st.incr.gens)
	st.mu.Unlock()
	if gens == 0 {
		t.Fatal("a pinned reader left no newer generation unmerged")
	}
	if n, err := countOf(db, rd); err != nil || n != pinned {
		t.Fatalf("pinned reader sees %d rows (err %v), want %d", n, err, pinned)
	}
	rd.Close()
	grown := db.PageCount()
	if grown <= steady {
		t.Fatalf("the file did not grow under the pin (%d → %d): the test does not exercise the gate", steady, grown)
	}
	for i := 0; i < 300; i++ {
		execDB(t, db, fmt.Sprintf("UPDATE t SET v = repeat('d', 30) WHERE id = %d", 1+i%200))
	}
	st.mu.Lock()
	gens, leaked := len(st.incr.gens), st.incrLeaked()
	st.mu.Unlock()
	if gens > 1 {
		t.Fatalf("%d generations remain unmerged with no reader", gens)
	}
	if leaked != 0 {
		t.Fatalf("%d dead pages left unlisted", leaked)
	}
	if after := db.PageCount(); after > grown {
		t.Fatalf("the file kept growing after the reader closed: %d → %d", grown, after)
	}
}
