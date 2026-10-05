package jed

// Host/storage assertions cannot be expressed by sqllogictest. The scenario table is shared
// with Rust and TS; the public session path must preserve both file bytes and I/O silence.
import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

type commitCountingStore struct {
	blockStore
	writes, syncs, grows int
}

func (s *commitCountingStore) writeAt(off int64, b []byte) error {
	s.writes++
	return s.blockStore.writeAt(off, b)
}
func (s *commitCountingStore) sync() error           { s.syncs++; return s.blockStore.sync() }
func (s *commitCountingStore) setSize(n int64) error { s.grows++; return s.blockStore.setSize(n) }
func countCommitIO(t *testing.T, st *storage) *commitCountingStore {
	t.Helper()
	var count *commitCountingStore
	if err := st.paging.withPager(func(p *pager) error {
		count = &commitCountingStore{blockStore: p.store}
		p.store = count
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	return count
}

func TestUnchangedCommits(t *testing.T) {
	path := filepath.Join(t.TempDir(), "noop.jed")
	db, err := CreateDatabase(CreateOptions{Path: path})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1, 1)", "CREATE SEQUENCE s"} {
		if _, err = queryOutcome(s, sql, nil); err != nil {
			t.Fatal(err)
		}
	}
	count := countCommitIO(t, db.core.storage)
	cases, err := os.ReadFile("../../spec/conformance/storage/unchanged_commits.tsv")
	if err != nil {
		t.Fatal(err)
	}
	for _, line := range strings.Split(string(cases), "\n") {
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		fields := strings.SplitN(line, "\t", 3)
		mode, sql := fields[0], fields[2]
		delta, _ := strconv.ParseUint(fields[1], 10, 64)
		t.Run(mode+" "+sql, func(t *testing.T) {
			before, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			version := db.Txid()
			count.writes, count.syncs, count.grows = 0, 0, 0
			if mode != "auto" && mode != "script" {
				if err := s.Begin(true); err != nil {
					t.Fatal(err)
				}
			}
			if mode == "script" {
				_, err = s.ExecuteScript(sql)
			} else {
				for _, stmt := range strings.Split(sql, ";") {
					if strings.TrimSpace(stmt) == "" {
						continue
					}
					_, err = queryOutcome(s, stmt, nil)
					if err != nil {
						break
					}
				}
			}
			if mode == "failed" {
				if err == nil {
					t.Fatal("expected statement failure")
				}
			} else if err != nil {
				t.Fatal(err)
			}
			switch mode {
			case "tx", "failed":
				err = s.Commit()
			case "rollback":
				err = s.Rollback()
			}
			if err != nil {
				t.Fatal(err)
			}
			if db.Txid() != version+delta {
				t.Fatalf("txid: %d -> %d, wanted delta %d", version, db.Txid(), delta)
			}
			if delta == 0 {
				after, err := os.ReadFile(path)
				if err != nil {
					t.Fatal(err)
				}
				if !bytes.Equal(before, after) || count.writes != 0 || count.syncs != 0 || count.grows != 0 {
					t.Fatalf("unchanged commit performed I/O: writes=%d syncs=%d grows=%d", count.writes, count.syncs, count.grows)
				}
			} else if count.writes == 0 || count.syncs == 0 {
				t.Fatal("mutation was not durable")
			}
		})
	}
	// Scoped callbacks share the same commit path and release the gate for the next writer.
	version := db.Txid()
	count.writes, count.syncs = 0, 0
	for _, body := range []func(*Transaction) error{func(*Transaction) error { return nil }, func(tx *Transaction) error { var n int; return tx.QueryRow(context.Background(), "SELECT 1").Scan(&n) }} {
		if err := db.Update(body); err != nil {
			t.Fatal(err)
		}
	}
	if db.Txid() != version || count.writes != 0 || count.syncs != 0 {
		t.Fatal("callback persisted")
	}
}

func BenchmarkWritableNoopCommit(b *testing.B) {
	for _, sql := range []string{"", "SELECT 1", "UPDATE t SET v = v WHERE id = 1"} {
		b.Run(sql, func(b *testing.B) {
			db, err := CreateDatabase(CreateOptions{Path: filepath.Join(b.TempDir(), "bench.jed")})
			if err != nil {
				b.Fatal(err)
			}
			defer db.Close()
			for _, q := range []string{"CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1,1)"} {
				if _, err := queryOutcome(db, q, nil); err != nil {
					b.Fatal(err)
				}
			}
			b.ResetTimer()
			for b.Loop() {
				if err := db.Update(func(tx *Transaction) error {
					if sql == "" {
						return nil
					}
					_, err := tx.Exec(context.Background(), sql)
					return err
				}); err != nil {
					b.Fatal(err)
				}
			}
		})
	}
}

func TestAttachmentCommitLeavesMainUntouched(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "main.jed")
	attached := filepath.Join(dir, "attached.jed")
	seed, err := CreateDatabase(CreateOptions{Path: attached})
	if err != nil {
		t.Fatal(err)
	}
	seed.Close()
	db, err := CreateDatabase(CreateOptions{Path: path})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if err := db.Attach("a", AttachFile(attached), false); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	defer s.Close()
	if _, err := queryOutcome(s, "CREATE TABLE t (id i32 PRIMARY KEY)", nil); err != nil {
		t.Fatal(err)
	}
	mainCount := countCommitIO(t, db.core.storage)
	attCount := countCommitIO(t, db.core.attachment("a").storage)
	before, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	version := db.Txid()
	// Main is a candidate write domain but no rows match. Only the attachment may persist.
	if err := s.Begin(true); err != nil {
		t.Fatal(err)
	}
	for _, sql := range []string{"DELETE FROM t WHERE id = 999", "CREATE TABLE a.t (id i32 PRIMARY KEY)", "INSERT INTO a.t VALUES (1)"} {
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatal(err)
		}
	}
	if err := s.Commit(); err != nil {
		t.Fatal(err)
	}
	after, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(before, after) || db.Txid() != version || mainCount.writes != 0 || mainCount.syncs != 0 {
		t.Fatal("attachment commit rewrote main")
	}
	if attCount.writes == 0 || attCount.syncs == 0 {
		t.Fatal("attachment was not durable")
	}
	attCount.writes, attCount.syncs = 0, 0
	if _, err := queryOutcome(s, "DELETE FROM a.t WHERE id = 999", nil); err != nil {
		t.Fatal(err)
	}
	if attCount.writes != 0 || attCount.syncs != 0 {
		t.Fatal("unchanged attachment persisted")
	}
	s.Close()
	if err := db.Detach("a"); err != nil {
		t.Fatal(err)
	}
	re, err := OpenDatabase(attached)
	if err != nil {
		t.Fatal(err)
	}
	defer re.Close()
	if rows := queryRows(t, re, "SELECT id FROM t"); len(rows) != 1 || rows[0][0].Int != 1 {
		t.Fatalf("reopened attachment: %v", rows)
	}
}
