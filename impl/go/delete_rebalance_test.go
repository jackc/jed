package jed

// The delete path's second merge-abandon rule (spec/fileformat/format.md "Delete"): a merge-overflow
// split whose new separator would overflow the PARENT is abandoned, so a delete can never leave an
// interior node over the page capacity (which the commit refuses with 0A000 — the row then being
// undeletable). The corpus pins the deterministic 8192-byte-page shape
// (storage/delete_merge_separator.test) and the simulation trace
// delete-merge-separator-overflow.sim pins a 256-byte-page shape's image hash across the cores; this
// test drives a randomized insert/delete workload over random-length text keys at page size 256 —
// where the rule fires often — and requires every statement, every commit, and every integrity check
// to succeed, logging the final file's SHA-256 for a by-hand cross-core comparison.

import (
	"context"
	"crypto/sha256"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDeleteRebalanceNeverOverflowsTheParent(t *testing.T) {
	t.Parallel()
	const alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
	for seed := int64(1); seed <= 6; seed++ {
		t.Run(fmt.Sprintf("seed%d", seed), func(t *testing.T) {
			t.Parallel()
			rng := rand.New(rand.NewSource(seed))
			path := filepath.Join(t.TempDir(), "rebalance.jed")
			db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 256, SkipFsync: true})
			if err != nil {
				t.Fatal(err)
			}
			exec := func(sql string) {
				t.Helper()
				if _, err := queryOutcome(db, sql, nil); err != nil {
					t.Fatalf("%s: %v", sql, err)
				}
			}
			exec("CREATE TABLE t (k text PRIMARY KEY)")
			live := map[string]bool{}
			var keys []string
			randomKey := func() string {
				var b strings.Builder
				n := 4 + rng.Intn(44)
				for i := 0; i < n; i++ {
					b.WriteByte(alphabet[rng.Intn(len(alphabet))])
				}
				return b.String()
			}
			check := func() {
				t.Helper()
				report, err := db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{Constraints: true})
				if err != nil || !report.OK() {
					t.Fatalf("integrity: %v %+v", err, report.Findings)
				}
				if int(report.Rows) != len(live) {
					t.Fatalf("integrity rows %d, want %d", report.Rows, len(live))
				}
			}
			for batch := 0; batch < 40; batch++ {
				// Grow for the first half, then drain: deletes dominate once the tree is a few levels deep.
				deleteBias := 30
				if batch >= 20 {
					deleteBias = 75
				}
				for i := 0; i < 12; i++ {
					if len(keys) > 0 && rng.Intn(100) < deleteBias {
						j := rng.Intn(len(keys))
						k := keys[j]
						keys[j] = keys[len(keys)-1]
						keys = keys[:len(keys)-1]
						delete(live, k)
						exec("DELETE FROM t WHERE k = '" + k + "'")
						continue
					}
					k := randomKey()
					if live[k] {
						continue
					}
					live[k] = true
					keys = append(keys, k)
					exec("INSERT INTO t VALUES ('" + k + "')")
				}
				check()
			}
			for _, k := range keys {
				delete(live, k)
				exec("DELETE FROM t WHERE k = '" + k + "'")
			}
			check()
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			raw, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			t.Logf("seed %d: %d bytes, sha256 %x", seed, len(raw), sha256.Sum256(raw))
		})
	}
}
