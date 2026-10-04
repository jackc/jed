package jed

import (
	"encoding/binary"
	"fmt"
	"os"
	"strings"
	"testing"
)

func TestCompleteManifestWithSelfFreeingDependenciesIsCorrupt(t *testing.T) {
	image, err := os.ReadFile("../../spec/fileformat/fixtures/cow_invalid_free_list.jed")
	if err != nil {
		t.Fatal(err)
	}
	_, err = loadEngine(image)
	engineErr, ok := err.(*EngineError)
	if !ok || engineErr.Code() != "XX001" {
		t.Fatalf("complete manifest with corrupt free-list must fail after selection: %v", err)
	}
}

func TestCommitCRC64ECMA182Vector(t *testing.T) {
	if got := commitCRC64(0, []byte("123456789")); got != 0x6C40DF5F0B497347 {
		t.Fatalf("CRC64/ECMA-182: %016x", got)
	}
}

func recordedCommitEngine(t *testing.T, image []byte) (*engine, *recordingStore, *memoryBlockStore) {
	t.Helper()
	base := newMemoryBlockStore(image)
	rec := &recordingStore{base: base}
	p, err := pagerFromStore(rec)
	if err != nil {
		t.Fatal(err)
	}
	db, err := loadEnginePaged(p, cacheLeaves(defaultCacheBytes, p.pageSize))
	if err != nil {
		t.Fatal(err)
	}
	db.path = "recorded-validated-commit.jed"
	return db, rec, base
}

func TestValidatedCommitUsesOneBarrierAndPoisonsFailedWriter(t *testing.T) {
	image, err := newSnapshot().ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	db, rec, base := recordedCommitEngine(t, image)
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	rec.ops = nil
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	syncs := 0
	for _, op := range rec.ops {
		if op.kind == opSync {
			syncs++
		}
	}
	if syncs != 1 {
		t.Fatalf("steady commit barriers = %d, want one", syncs)
	}
	m, err := selectMeta(base.buf, 256)
	if err != nil || m.dirtyCount == 0 || m.manifestPages != 0 {
		t.Fatalf("small commit should use inline manifest: %+v %v", m, err)
	}
	armCommitFault(t, db, commitFault{point: faultSync, n: 1, tearBytes: -1})
	if _, err := execute(db, "INSERT INTO t VALUES (2)"); err == nil {
		t.Fatal("expected sync fault")
	}
	n := len(rec.ops)
	if _, err := execute(db, "INSERT INTO t VALUES (3)"); err == nil {
		t.Fatal("failed writer accepted another commit")
	}
	if len(rec.ops) != n {
		t.Fatal("poisoned writer issued more storage operations")
	}
	recovered, recoveryLog, _ := recordedCommitEngine(t, base.buf)
	mustExec(t, recovered, "INSERT INTO t VALUES (4)")
	if len(recoveryLog.ops) == 0 || recoveryLog.ops[0].kind != opSync {
		t.Fatal("adopted manifest must sync before first new write")
	}
	recoveryLog.ops = nil
	mustExec(t, recovered, "INSERT INTO t VALUES (5)")
	if recoveryLog.ops[0].kind == opSync {
		t.Fatal("recovery barrier repeated for locally acknowledged generation")
	}
}

func TestValidatedManifestRejectsStaleChecksummedBodyAndBrokenChain(t *testing.T) {
	prior, ops, priorIDs, postIDs := recordFatCommit(t)
	complete := applyCrash(prior, ops, len(ops), -1, 0)
	m, err := selectMeta(complete, 256)
	if err != nil {
		t.Fatal(err)
	}
	if m.manifestPages == 0 {
		t.Fatal("fat commit must overflow the inline manifest")
	}
	slot := int(m.txid&1) * 256
	firstBody := binary.BigEndian.Uint32(complete[slot+64:])
	stale := append([]byte(nil), complete...)
	page := stale[int(firstBody)*256 : int(firstBody+1)*256]
	page[pageHeader] ^= 0x40
	binary.BigEndian.PutUint32(page[12:], pageCRC(page))
	ids, err := guardedScanIDs(stale)
	if err != nil || !equalIDs(ids, priorIDs) {
		t.Fatalf("internally valid wrong body must reject newest manifest: %v %v", ids, err)
	}
	for n := uint32(0); n < m.manifestPages; n++ {
		broken := append([]byte(nil), complete...)
		id := m.manifestHead
		for j := uint32(0); j < n; j++ {
			id = binary.BigEndian.Uint32(broken[int(id)*256+8:])
		}
		broken[int(id)*256+32] ^= 1
		ids, err := guardedScanIDs(broken)
		if err != nil || !equalIDs(ids, priorIDs) {
			t.Fatalf("broken manifest page %d must fall back: %v %v", n, ids, err)
		}
	}
	assertRecovers(t, complete, priorIDs, postIDs, "complete overflowing commit")
}

func TestValidatedManifestDependenciesAreNotReusedUntilNextGeneration(t *testing.T) {
	image, err := newSnapshot().ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	db, rec, base := recordedCommitEngine(t, image)
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	var sql strings.Builder
	sql.WriteString("INSERT INTO t VALUES ")
	for i := 1; i <= 100; i++ {
		if i > 1 {
			sql.WriteString(",")
		}
		fmt.Fprintf(&sql, "(%d, 'a long enough string to make several leaf pages')", i)
	}
	mustExec(t, db, sql.String())
	for round := 0; round < 40; round++ {
		prior := append([]byte(nil), base.buf...)
		previous, err := selectMeta(prior, 256)
		if err != nil {
			t.Fatal(err)
		}
		rec.ops = nil
		mustExec(t, db, fmt.Sprintf("UPDATE t SET pad='round %d with many dirty leaves and overflow manifest pages'", round))
		current, err := selectMeta(base.buf, 256)
		if err != nil {
			t.Fatalf("round %d: %v", round, err)
		}
		if current.manifestPages == 0 {
			t.Fatal("large update must retain overflowing manifest")
		}
		free := make(map[uint32]bool)
		for _, id := range db.freePages {
			free[id] = true
		}
		raw := base.buf[int(current.txid&1)*256:][:256]
		check := func(entries []byte, n uint32) {
			for i := uint32(0); i < n; i++ {
				if id := binary.BigEndian.Uint32(entries[int(i)*12:]); free[id] {
					t.Fatalf("manifest dependency %d is reusable", id)
				}
			}
		}
		check(raw[64:], min(current.dirtyCount, uint32(manifestInlineCapacity(256))))
		for id := current.manifestHead; id != 0; {
			if free[id] {
				t.Fatalf("manifest page %d is reusable", id)
			}
			b := base.buf[int(id)*256:][:256]
			check(b[32:], binary.BigEndian.Uint32(b[4:]))
			id = binary.BigEndian.Uint32(b[8:])
		}
		crashed := applyCrash(prior, rec.ops, len(rec.ops)-1, -1, 1)
		fallback, err := selectMeta(crashed, 256)
		if err != nil || fallback.txid != previous.txid {
			t.Fatalf("reuse round %d lost fallback: %+v %v", round, fallback, err)
		}
	}
}

func TestValidatedAllocatorRejectsPageHighWaterOverflow(t *testing.T) {
	_, err := newSnapshot().incrementalImage(256, ^uint32(0), nil, true, nil)
	if e, ok := err.(*EngineError); !ok || e.Code() != "54000" {
		t.Fatalf("body high-water overflow: %v", err)
	}
	_, _, _, _, err = serializeFreeList([]uint32{2, 3}, nil, 240, 256, ^uint32(0))
	if e, ok := err.(*EngineError); !ok || e.Code() != "54000" {
		t.Fatalf("free-list high-water overflow: %v", err)
	}
}
