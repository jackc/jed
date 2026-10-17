package jed

// Batched durability under synchronous = off (spec/design/validated-cow.md §4): host-API and byte-level
// assertions the corpus cannot express — barrier counts, meta-slot placement, the batch cap, close's
// flush, and byte identity between the background and inline cap flush. Crash recovery under relaxed
// mode is the simulator's (spec/design/simulation-testing.md, the relaxed profile).

import (
	"bytes"
	"encoding/binary"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"testing"
)

func relaxedDB(t *testing.T, path string, pageSize uint32, inline bool) *Database {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: pageSize, Synchronous: SynchronousOff})
	if err != nil {
		t.Fatal(err)
	}
	db.core.storage.inlineFlush = inline
	return db
}

func metaTxidAt(t *testing.T, path string, slot int) uint64 {
	t.Helper()
	img, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	ps := int(binary.BigEndian.Uint32(img[8:12]))
	return binary.BigEndian.Uint64(img[slot*ps+12 : slot*ps+20])
}

func TestRelaxedCommitsDeferTheBarrier(t *testing.T) {
	path := filepath.Join(t.TempDir(), "relaxed.jed")
	db := relaxedDB(t, path, 0, false)
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	count := countCommitIO(t, db.core.storage)
	for i := 1; i <= 10; i++ {
		mustExecDB(t, db, "INSERT INTO t VALUES ("+strconv.Itoa(i)+", 'x')")
	}
	if count.syncs != 0 {
		t.Fatalf("relaxed commits issued %d barriers below the batch cap, want 0", count.syncs)
	}
	// Every pending commit writes the slot that does not hold the floor (create's txid 1 in slot 1).
	if got := metaTxidAt(t, path, 0); got != db.Txid() {
		t.Fatalf("slot 0 holds txid %d, want the newest pending commit %d", got, db.Txid())
	}
	if got := metaTxidAt(t, path, 1); got != 1 {
		t.Fatalf("slot 1 holds txid %d, want the floor (txid 1)", got)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if count.syncs != 1 {
		t.Fatalf("close issued %d barriers, want the one flush", count.syncs)
	}
	reopened, err := OpenDatabase(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	rows := queryInts(t, reopened, "SELECT count(*) FROM t")
	if rows != 10 {
		t.Fatalf("reopened database has %d rows, want 10", rows)
	}
}

func TestRelaxedBatchCapFlushesInBackgroundWithSameBytes(t *testing.T) {
	dir := t.TempDir()
	images := map[bool][]byte{}
	barriers := map[bool]int{}
	for _, inline := range []bool{false, true} {
		path := filepath.Join(dir, "cap"+strconv.FormatBool(inline)+".jed")
		db := relaxedDB(t, path, 256, inline)
		mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
		mustExecDB(t, db, "CREATE INDEX t_v ON t (v)")
		count := countCommitIO(t, db.core.storage)
		for i := 1; i <= 120; i++ {
			mustExecDB(t, db, "INSERT INTO t VALUES ("+strconv.Itoa(i)+", repeat('v', "+strconv.Itoa(i%40)+"))")
			if i%7 == 0 {
				mustExecDB(t, db, "DELETE FROM t WHERE id = "+strconv.Itoa(i-3))
			}
		}
		st := db.core.storage
		st.mu.Lock()
		_ = st.awaitFlush()
		st.mu.Unlock()
		barriers[inline] = count.syncs
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		img, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		images[inline] = img
	}
	if barriers[true] == 0 || barriers[true] != barriers[false] {
		t.Fatalf("cap flushes: inline %d, background %d — want equal and nonzero", barriers[true], barriers[false])
	}
	if !bytes.Equal(images[true], images[false]) {
		t.Fatal("background and inline cap flushes wrote different files")
	}
}

// TestRelaxedBatchReusesOnlyTheFloorsFreePages churns updates through several batches: a commit inside
// a batch may reuse only pages free at the floor, never one freed during the batch, and no free list
// may hold a pending page (validated-cow.md §4 rules 3 and 4). The file must also stop growing: each
// batch's orphans become reusable once a flush moves the floor past them.
func TestRelaxedBatchReusesOnlyTheFloorsFreePages(t *testing.T) {
	path := filepath.Join(t.TempDir(), "reuse.jed")
	db := relaxedDB(t, path, 256, true)
	defer db.Close()
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	for i := 1; i <= 40; i++ {
		mustExecDB(t, db, "INSERT INTO t VALUES ("+strconv.Itoa(i)+", repeat('v', 30))")
	}
	st := db.core.storage
	state := func() (pending map[uint32]uint64, free []uint32, floorFree map[uint32]bool, pageCount uint32) {
		st.mu.Lock()
		defer st.mu.Unlock()
		_ = st.paging.withPager(func(p *pager) error {
			pending = make(map[uint32]uint64, len(p.pending))
			for k, v := range p.pending {
				pending[k] = v
			}
			return nil
		})
		return pending, append([]uint32(nil), st.freePages...), st.floorFree, st.pageCount
	}
	var midway uint32
	for i := 0; i < 400; i++ {
		before, _, floorFree, highWater := state()
		mustExecDB(t, db, "UPDATE t SET v = repeat('"+string(rune('a'+i%26))+"', 30) WHERE id = "+strconv.Itoa(1+i%40))
		after, free, _, _ := state()
		for _, f := range free {
			if _, ok := after[f]; ok {
				t.Fatalf("update %d: the free list holds pending page %d", i, f)
			}
		}
		if len(before) == 0 || len(after) == 0 {
			continue // the batch's first commit plans as under synchronous = on; or a cap flush followed
		}
		for pg := range after {
			if _, old := before[pg]; old || pg >= highWater {
				continue
			}
			if !floorFree[pg] {
				t.Fatalf("update %d reused page %d, which was not free at the floor", i, pg)
			}
		}
		if i == 200 {
			midway = st.pageCount
		}
	}
	if st.pageCount > midway+uint32(manifestInlineCapacity(256))*2 {
		t.Fatalf("the file kept growing under relaxed churn: %d pages at update 200, %d at 400", midway, st.pageCount)
	}
}

// TestRelaxedNewestMetaVouchesForTheWholeBatch crashes a two-commit batch so that only the second
// commit's writes (its body pages and meta) landed: the first commit's pages, which the second
// commit's tree still references, are missing. The second meta's cumulative manifest lists them, so
// recovery must reject it and fall back to the floor — never accept a root whose dependencies did not
// land (validated-cow.md §4 rule 2).
func TestRelaxedNewestMetaVouchesForTheWholeBatch(t *testing.T) {
	path := filepath.Join(t.TempDir(), "vouch.jed")
	db := relaxedDB(t, path, 256, true)
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	mustExecDB(t, db, "INSERT INTO t SELECT g, repeat('a', 20) FROM generate_series(1, 60) g")
	if err := db.Close(); err != nil { // the floor: every row 'a'
		t.Fatal(err)
	}
	prior, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	reopened, err := OpenDatabaseWithOptions(path, OpenOptions{Synchronous: SynchronousOff})
	if err != nil {
		t.Fatal(err)
	}
	reopened.core.storage.inlineFlush = true
	// The first commit after an open stabilizes the adopted generation with a barrier (validated-cow.md
	// §3); it then starts the batch.
	mustExecDB(t, reopened, "UPDATE t SET v = repeat('w', 20) WHERE id = 30")
	var rec *recordingStore
	_ = reopened.core.storage.paging.withPager(func(p *pager) error {
		rec = &recordingStore{base: p.store}
		p.store = rec
		return nil
	})
	mustExecDB(t, reopened, "UPDATE t SET v = repeat('b', 20) WHERE id = 1") // leaf A
	first := len(rec.ops)
	mustExecDB(t, reopened, "UPDATE t SET v = repeat('c', 20) WHERE id = 60") // leaf B
	for _, op := range rec.ops {
		if op.kind == opSync {
			t.Fatal("the two-commit batch reached a barrier; the test needs it pending")
		}
	}
	// Power loss: the floor, plus every op of the second commit, and none of the first commit's writes.
	img := append([]byte(nil), prior...)
	for _, op := range rec.ops[first:] {
		switch op.kind {
		case opWrite:
			if end := op.off + int64(len(op.data)); end > int64(len(img)) {
				img = append(img, make([]byte, end-int64(len(img)))...)
			}
			copy(img[op.off:], op.data)
		case opSetSize:
			if op.n > int64(len(img)) {
				img = append(img, make([]byte, op.n-int64(len(img)))...)
			}
		}
	}
	rows, err := guardedQuery(img, "SELECT v FROM t WHERE id IN (1, 60) ORDER BY id")
	if err != nil {
		t.Fatalf("recovery over a half-landed batch failed: %v", err)
	}
	if len(rows) != 2 || rows[0][0].Render() != "aaaaaaaaaaaaaaaaaaaa" || rows[1][0].Render() != "aaaaaaaaaaaaaaaaaaaa" {
		t.Fatalf("recovery over a half-landed batch returned %v, want the floor's rows", rows)
	}
	_ = reopened.Close()
}

// failingSyncStore fails every barrier, as a dying device would.
type failingSyncStore struct{ blockStore }

func (s failingSyncStore) sync() error { return newError(IoError, "injected sync failure") }

func TestRelaxedCloseReportsAFailedFlush(t *testing.T) {
	path := filepath.Join(t.TempDir(), "flushfail.jed")
	db := relaxedDB(t, path, 0, false)
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExecDB(t, db, "INSERT INTO t VALUES (1)")
	_ = db.core.storage.paging.withPager(func(p *pager) error {
		p.store = failingSyncStore{p.store}
		return nil
	})
	err := db.Close()
	var engine *EngineError
	if !errors.As(err, &engine) || engine.Code() != "58030" {
		t.Fatalf("close with a failing flush returned %v, want 58030", err)
	}
}

func TestSynchronousOnKeepsTxidParitySlots(t *testing.T) {
	path := filepath.Join(t.TempDir(), "on.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	for i := 1; i <= 5; i++ {
		mustExecDB(t, db, "INSERT INTO t VALUES ("+strconv.Itoa(i)+")")
		v := db.Txid()
		if got := metaTxidAt(t, path, int(v&1)); got != v {
			t.Fatalf("txid %d not in slot %d (holds %d)", v, v&1, got)
		}
	}
	_ = db.Close()
}

func mustExecDB(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := db.ExecuteScript(sql); err != nil {
		t.Fatalf("%s: %v", sql, err)
	}
}

func queryInts(t *testing.T, db *Database, sql string) int64 {
	t.Helper()
	rows, err := db.queryValues(sql, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	if !rows.Next() {
		t.Fatalf("%s: no row", sql)
	}
	var n int64
	if err := rows.Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}
