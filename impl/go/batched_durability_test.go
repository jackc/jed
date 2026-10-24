package jed

// Batched durability under synchronous = off (spec/design/validated-cow.md §4): host-API and byte-level
// assertions the corpus cannot express — barrier counts, meta-slot placement, the batch cap, close's
// flush, byte identity between the background and inline cap flush, and the time-based flush (§4.1) on
// a manual clock. Crash recovery under relaxed mode is the simulator's
// (spec/design/simulation-testing.md, the relaxed profile).

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"testing"
	"time"
)

// relaxedDB is a relaxed database with no time-based flush, so wall time never moves a flush.
func relaxedDB(t *testing.T, path string, pageSize uint32, inline bool) *Database {
	t.Helper()
	return relaxedDBTimed(t, path, pageSize, inline, 0, nil)
}

// relaxedDBTimed is a relaxed database whose batches end intervalMs after they start on env's clock.
func relaxedDBTimed(t *testing.T, path string, pageSize uint32, inline bool, intervalMs uint64, env *Environment) *Database {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: pageSize, Synchronous: SynchronousOff, FlushIntervalMs: &intervalMs, Environment: env})
	if err != nil {
		t.Fatal(err)
	}
	st := db.core.storage
	st.mu.Lock()
	st.inlineFlush = inline
	st.mu.Unlock()
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
	if st.pageCount > midway+uint32(manifestInlineCapacity(256, plainCap(256)))*2 {
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
	zero := uint64(0)
	reopened, err := OpenDatabaseWithOptions(path, OpenOptions{Synchronous: SynchronousOff, FlushIntervalMs: &zero})
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

// TestRelaxedBackgroundFlushWithConcurrentReaders runs readers on their own goroutines while a writer
// commits across many background cap flushes (run under -race): readers fault pages while a flush
// holds the pager, and every reader sees a consistent snapshot.
func TestRelaxedBackgroundFlushWithConcurrentReaders(t *testing.T) {
	path := filepath.Join(t.TempDir(), "readers.jed")
	db := relaxedDB(t, path, 256, false)
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i64)")
	mustExecDB(t, db, "INSERT INTO t SELECT g, 0 FROM generate_series(1, 200) g")
	done := make(chan struct{})
	errs := make(chan error, 4)
	for r := 0; r < 4; r++ {
		go func() {
			for {
				select {
				case <-done:
					errs <- nil
					return
				default:
				}
				s := db.ReadSession()
				rows, err := s.queryValues("SELECT count(*), min(v), max(v) FROM t", nil)
				if err != nil {
					s.Close()
					errs <- err
					return
				}
				rows.Next()
				var n, lo, hi int64
				if err := rows.Scan(&n, &lo, &hi); err != nil || n != 200 || lo != hi {
					rows.Close()
					s.Close()
					errs <- fmt.Errorf("reader saw count %d, v from %d to %d (%v)", n, lo, hi, err)
					return
				}
				rows.Close()
				s.Close()
			}
		}()
	}
	for i := 0; i < 150; i++ {
		mustExecDB(t, db, "UPDATE t SET v = v + 1")
	}
	close(done)
	for r := 0; r < 4; r++ {
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := OpenDatabase(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if got := queryInts(t, reopened, "SELECT count(*) FROM t WHERE v = 150"); got != 200 {
		t.Fatalf("reopened with %d rows at v = 150, want all 200", got)
	}
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

// manualEnv is a manual clock and an environment that reads it.
func manualEnv() (*ManualClock, *Environment) {
	clock := NewManualClock(0)
	b := NewEnvironmentBuilder()
	b.SetClockSource(clock)
	return clock, b.Build()
}

// storagePending reports whether st holds a relaxed batch pending.
func storagePending(st *storage) bool {
	st.mu.Lock()
	defer st.mu.Unlock()
	n := 0
	_ = st.paging.withPager(func(p *pager) error { n = len(p.pending); return nil })
	return n > 0
}

// syncsOf reads count's barriers under st.mu, which the idle flusher holds while it flushes.
func syncsOf(st *storage, count *commitCountingStore) int {
	st.mu.Lock()
	defer st.mu.Unlock()
	return count.syncs
}

// eventually waits up to five seconds for cond.
func eventually(cond func() bool) bool {
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if cond() {
			return true
		}
		time.Sleep(5 * time.Millisecond)
	}
	return cond()
}

// TestRelaxedDeadlineFlushesBeforeTheNextCommit: a commit that reads the clock at or past its batch's
// deadline flushes the batch before it plans (validated-cow.md §4.1 rule 3), and starts the next batch
// itself.
func TestRelaxedDeadlineFlushesBeforeTheNextCommit(t *testing.T) {
	path := filepath.Join(t.TempDir(), "deadline.jed")
	clock, env := manualEnv()
	db := relaxedDBTimed(t, path, 0, true, 1000, env)
	defer db.Close()
	st := db.core.storage
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	count := countCommitIO(t, st)
	mustExecDB(t, db, "INSERT INTO t VALUES (1)")
	clock.Advance(999_999)
	mustExecDB(t, db, "INSERT INTO t VALUES (2)")
	if count.syncs != 0 {
		t.Fatal("a commit before the deadline flushed")
	}
	clock.Advance(1)
	before := db.Txid()
	mustExecDB(t, db, "INSERT INTO t VALUES (3)")
	if count.syncs != 1 {
		t.Fatalf("the commit at the deadline issued %d barriers, want the one flush first", count.syncs)
	}
	// The flush made before the floor, so the new commit wrote the other slot and is pending.
	if got := metaTxidAt(t, path, int(before&1)); got != before {
		t.Fatalf("slot %d holds txid %d, want the flushed floor %d", before&1, got, before)
	}
	if got := metaTxidAt(t, path, int(before&1^1)); got != db.Txid() {
		t.Fatalf("slot %d holds txid %d, want the new pending commit %d", before&1^1, got, db.Txid())
	}
	if !storagePending(st) {
		t.Fatal("the commit after the flush is not pending")
	}
	// Its batch started at the reading of the flushing commit: the next deadline is a whole interval on.
	clock.Advance(999_999)
	mustExecDB(t, db, "INSERT INTO t VALUES (4)")
	if count.syncs != 1 {
		t.Fatal("the new batch ended early")
	}
}

// TestRelaxedIdleFlusherWaitsForTheClock: the idle flusher flushes a batch no commit follows once the
// clock passes its deadline, and never before (validated-cow.md §4.1 rule 5).
func TestRelaxedIdleFlusherWaitsForTheClock(t *testing.T) {
	path := filepath.Join(t.TempDir(), "idle.jed")
	clock, env := manualEnv()
	db := relaxedDBTimed(t, path, 0, false, 20, env)
	defer db.Close()
	st := db.core.storage
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	st.mu.Lock()
	count := countCommitIO(t, st)
	st.mu.Unlock()
	mustExecDB(t, db, "INSERT INTO t VALUES (1)")
	time.Sleep(150 * time.Millisecond)
	if syncsOf(st, count) != 0 {
		t.Fatal("the flusher flushed before the clock passed the deadline")
	}
	if !storagePending(st) {
		t.Fatal("the batch is not pending")
	}
	clock.Advance(20_000)
	if !eventually(func() bool { return !storagePending(st) }) {
		t.Fatal("the idle flusher never flushed the batch")
	}
	if n := syncsOf(st, count); n != 1 {
		t.Fatalf("the idle flush issued %d barriers, want 1", n)
	}
}

// TestRelaxedIdleFlusherWritesTheSameBytes: the same commits and clock readings write the same file
// whether the idle flusher ran each flush or the next commit did (validated-cow.md §4.1).
func TestRelaxedIdleFlusherWritesTheSameBytes(t *testing.T) {
	dir := t.TempDir()
	images := map[bool][]byte{}
	for _, background := range []bool{true, false} {
		path := filepath.Join(dir, "idle"+strconv.FormatBool(background)+".jed")
		clock, env := manualEnv()
		db := relaxedDBTimed(t, path, 256, !background, 10, env)
		st := db.core.storage
		mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
		for i := 1; i <= 40; i++ {
			mustExecDB(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, repeat('v', %d))", i, i%30))
			if i%6 == 0 {
				mustExecDB(t, db, fmt.Sprintf("DELETE FROM t WHERE id = %d", i-2))
			}
			if i%5 == 0 {
				clock.Advance(10_000)
				if background && !eventually(func() bool { return !storagePending(st) }) {
					t.Fatalf("the idle flusher never flushed after commit %d", i)
				}
			}
		}
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		img, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		images[background] = img
	}
	if !bytes.Equal(images[true], images[false]) {
		t.Fatal("the idle flusher changed the file's bytes")
	}
}

// TestRelaxedDeadlineIsPerDatabase: an attachment follows its handle's interval and clock but keeps its
// own batch and deadline.
func TestRelaxedDeadlineIsPerDatabase(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "main.jed")
	clock, env := manualEnv()
	db := relaxedDBTimed(t, path, 0, true, 1000, env)
	defer db.Close()
	other := filepath.Join(dir, "other.jed")
	created, err := CreateDatabase(CreateOptions{Path: other, Environment: env})
	if err != nil {
		t.Fatal(err)
	}
	if err := created.Close(); err != nil {
		t.Fatal(err)
	}
	if err := db.Attach("o", AttachFile(other), false); err != nil {
		t.Fatal(err)
	}
	attached := func() *storage {
		att := db.core.attachment("o")
		if att.svc != nil {
			return att.svc.core.storage
		}
		return att.storage
	}
	mustExecDB(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	clock.Advance(500_000)
	mustExecDB(t, db, "CREATE TABLE o.u (id i32 PRIMARY KEY)")
	clock.Advance(500_000)
	// Main's deadline has passed; the attachment's has not.
	mustExecDB(t, db, "INSERT INTO o.u VALUES (1)")
	if !storagePending(attached()) {
		t.Fatal("the attachment's batch ended at main's deadline")
	}
	if !storagePending(db.core.storage) {
		t.Fatal("main flushed without a commit of its own")
	}
	mustExecDB(t, db, "INSERT INTO t VALUES (1)")
	if !storagePending(attached()) {
		t.Fatal("main's commit ended the attachment's batch")
	}
}

// The max_storage_bytes precheck of a commit that also writes an in-memory attachment plans a pending
// batch's reuse restriction, while the commit itself may first flush the batch at its deadline
// (validated-cow.md §4.1) and plan without it. The answer cannot differ: the file form limits live
// pages (memory.md §8.7), which do not depend on which page ids a commit takes. Run the same
// transactions with no batching, with batches, and with a deadline flush before every commit: every
// 54P06 lands on the same transaction. Mirrors the Rust test.
func TestRelaxedStoragePrecheckIgnoresTheDeadline(t *testing.T) {
	dir := t.TempDir()
	var runs [][]string
	for k, mode := range []string{"on", "batched", "deadline"} {
		path := filepath.Join(dir, fmt.Sprintf("precheck%d.jed", k))
		clock, env := manualEnv()
		var db *Database
		if mode == "on" {
			var err error
			db, err = CreateDatabase(CreateOptions{Path: path, PageSize: 512, Environment: env})
			if err != nil {
				t.Fatal(err)
			}
		} else {
			interval := uint64(0)
			if mode == "deadline" {
				interval = 1
			}
			db = relaxedDBTimed(t, path, 512, true, interval, env)
		}
		if err := db.Attach("m", AttachMemory(), false); err != nil {
			t.Fatal(err)
		}
		mustExecDB(t, db, "CREATE TABLE t (id i64 PRIMARY KEY, v text)")
		mustExecDB(t, db, "CREATE TABLE m.log (id i64 PRIMARY KEY)")
		if err := db.SetMaxStorageBytes("main", 40*512); err != nil {
			t.Fatal(err)
		}
		var outcomes []string
		tripped, admitted := false, false
		for i := int64(0); i < 120; i++ {
			var sql string
			switch i % 4 {
			case 0, 1:
				sql = fmt.Sprintf("INSERT INTO t SELECT g, md5(g::text) || md5((g + 1)::text) FROM generate_series(%d, %d) g", i*10, i*10+9)
			case 2:
				sql = fmt.Sprintf("DELETE FROM t WHERE id %% 50 = %d", i%50)
			default:
				sql = fmt.Sprintf("UPDATE t SET v = left(md5(v), %d) || v WHERE id %% 5 = 0", 1+i%20)
			}
			s := db.Session(SessionOptions{})
			err := s.Begin(true)
			if err == nil {
				_, err = queryOutcome(s, sql, nil)
			}
			if err == nil {
				_, err = queryOutcome(s, "INSERT INTO m.log VALUES ("+strconv.FormatInt(i, 10)+")", nil)
			}
			if err == nil {
				err = s.Commit()
			}
			s.Close()
			code := ""
			if err != nil {
				code = errCodeOf(err)
				if code == "" {
					code = err.Error()
				}
			}
			tripped = tripped || code == "54P06"
			admitted = admitted || code == ""
			outcomes = append(outcomes, code)
			clock.Advance(1_000)
		}
		if !tripped || !admitted {
			t.Fatalf("%s: want both admitted and 54P06 commits, got %v", mode, outcomes)
		}
		runs = append(runs, outcomes)
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
	}
	for k, name := range []string{"", "batching", "the deadline flush"} {
		if k > 0 && fmt.Sprint(runs[k]) != fmt.Sprint(runs[0]) {
			t.Fatalf("%s moved a 54P06:\n on: %v\n got: %v", name, runs[0], runs[k])
		}
	}
}
