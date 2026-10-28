package jed

// IntegrityCheck on a deserialized image whose NEWEST meta slot names a generation the loader
// rejected (spec/design/integrity-check.md §2): the pinned snapshot is the older generation the
// loader fell back to, so the check must read that slot — not the newest slot that parses — or every
// pin looks stale and the check spins to 55006.

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

func TestIntegrityCheckDeserializedFallbackGeneration(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "fallback.jed")
	db := createCompactFile(t, path)
	serializeExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	serializeExec(t, db, "INSERT INTO t SELECT g, repeat('v', 40) FROM generate_series(1, 20) g")
	serializeExec(t, db, "INSERT INTO t SELECT g, repeat('w', 40) FROM generate_series(21, 40) g")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	// Tear the newest generation: zero its catalog root, a page only its manifest lists, so recovery
	// rejects the newest slot and falls back to the previous generation (validated-cow.md §1).
	newest, older := parseMetaOrFatal(t, raw[:compactPage]), parseMetaOrFatal(t, raw[compactPage:2*compactPage])
	if newest.txid < older.txid {
		newest, older = older, newest
	}
	root := int(newest.rootPage)
	for i := root * compactPage; i < (root+1)*compactPage; i++ {
		raw[i] = 0
	}
	cp := mustDeserialize(t, raw, DeserializeOptions{})
	defer cp.Close()
	if cp.Version() != older.txid {
		t.Fatalf("loaded version %d, want the fallback generation %d", cp.Version(), older.txid)
	}
	rows, err := queryOutcome(cp, "SELECT count(*) FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := renderRows(rows.Rows); len(got) != 1 || got[0][0] != "20" {
		t.Fatalf("fallback rows: %v", got)
	}
	report, err := cp.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{})
	if err != nil || !report.OK() {
		t.Fatalf("integrity check of the fallback generation: %v %+v", err, report.Findings)
	}
	if report.Rows != 20 {
		t.Fatalf("integrity rows %d, want 20", report.Rows)
	}
}

func parseMetaOrFatal(t *testing.T, slot []byte) meta {
	t.Helper()
	m, ok := parseMeta(slot)
	if !ok {
		t.Fatal("meta slot does not parse")
	}
	return m
}
