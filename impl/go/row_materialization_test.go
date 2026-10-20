package jed

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
)

type rowReuseReadStore struct {
	blockStore
	reads, overflow int
	failOverflow    bool
}

func (s *rowReuseReadStore) readAt(off int64, n int) ([]byte, error) {
	s.reads++
	b, err := s.blockStore.readAt(off, n)
	if err == nil && len(b) > 0 && b[0] == pageOverflow {
		s.overflow++
		if s.failOverflow {
			return nil, errors.New("injected overflow read failure")
		}
	}
	return b, err
}

func TestRowReuseDeferredReads(t *testing.T) {
	path := filepath.Join(t.TempDir(), "reads.jed")
	db, err := create(path, databaseOptions{PageSize: lazyPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	lazySeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	paging := db.readSnap().store("t").paging
	device := &rowReuseReadStore{blockStore: paging.pgr.store, failOverflow: true}
	paging.pgr.store = device
	for _, sql := range []string{"SELECT id FROM t ORDER BY id", "SELECT body FROM t ORDER BY id", "SELECT body FROM t ORDER BY id LIMIT 0"} {
		rows, err := db.queryValues(sql, nil)
		if err != nil {
			t.Fatal(err)
		}
		if device.overflow != 0 {
			t.Fatal("open fetched a deferred body")
		}
		before := device.reads
		if strings.HasSuffix(sql, "LIMIT 0") && rows.Next() {
			t.Fatal("LIMIT 0 produced a row")
		}
		_ = rows.Close()
		if rows.Next() || device.reads != before {
			t.Fatal("close/LIMIT 0 read ahead")
		}
	}
	rows, err := db.queryValues("SELECT id FROM t ORDER BY id LIMIT 1", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() || rows.Row()[0].Int != 1 {
		t.Fatal(rows.Err())
	}
	before := device.reads
	if rows.Next() || device.reads != before || device.overflow != 0 {
		t.Fatal("LIMIT fetched an unused row/body")
	}
	_ = rows.Close()
	rows, err = db.queryValues("SELECT body FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if rows.Next() || rows.Err() == nil || device.overflow != 1 {
		t.Fatal("deferred failure did not occur at first pull")
	}
	_ = rows.Close()
}

func rowReuseDB(t *testing.T, file bool) *Database {
	t.Helper()
	opts := CreateOptions{SkipFsync: true}
	if file {
		opts.Path = filepath.Join(t.TempDir(), "rows.jed")
	}
	db, err := CreateDatabase(opts)
	if err != nil {
		t.Fatal(err)
	}
	for _, sql := range []string{
		"CREATE TABLE t (id i32 PRIMARY KEY, a i32, txt text, bytes bytea, arr i64[], j jsonb)",
		"INSERT INTO t SELECT n, CASE WHEN n%7=0 THEN NULL ELSE n*2 END, 'row-' || n::text, decode('abcdef', 'hex'), ARRAY[n,n+1], jsonb_build_object('n',n) FROM generate_series(1,256) AS g(n)",
	} {
		if _, err := queryOutcome(db, sql, nil); err != nil {
			t.Fatal(err)
		}
	}
	if file {
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		db, err = OpenDatabaseWithOptions(opts.Path, OpenOptions{SkipFsync: true})
		if err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { _ = db.Close() })
	return db
}

// These host ownership promises cannot be expressed by SQL corpus records.
func TestRowReuseRetainedResults(t *testing.T) {
	for _, file := range []bool{false, true} {
		t.Run(fmt.Sprint(file), func(t *testing.T) {
			db := rowReuseDB(t, file)
			s := db.Session(SessionOptions{MaxQueryMemoryBytes: 1024})
			defer s.Close()
			for _, dirty := range []bool{false, true} {
				if dirty {
					mustCreate(t, s, "BEGIN")
					// Give the writer a budget for its pending versions, then hold it
					// fixed throughout the scan (Q3 must not be released by scratch).
					s.SetMaxQueryMemoryBytes(1 << 20)
					mustCreate(t, s, "UPDATE t SET txt='changed' WHERE id=2")
				}
				rows, err := s.Query(context.Background(), "SELECT id,a,txt,bytes,arr,j,ROW(id,txt),ARRAY[id,a],(SELECT t.txt) FROM t ORDER BY id")
				if err != nil {
					t.Fatal(err)
				}
				c, ok := rows.cursor.(*streamingCursor)
				if !ok {
					t.Fatalf("want streaming cursor, got %T", rows.cursor)
				}
				var retained [][]Value
				var rendered [][]string
				var natural [][]any
				var scanned [][]any
				var input *Value
				for rows.Next() {
					if input == nil {
						input = &c.input[0]
					} else if input != &c.input[0] {
						t.Fatal("input backing slice was not reused")
					}
					retained = append(retained, rows.Row())
					rendered = append(rendered, renderRows([][]Value{rows.Row()})[0])
					v, err := rows.Values()
					if err != nil {
						t.Fatal(err)
					}
					natural = append(natural, v)
					v = make([]any, len(rows.Row()))
					dest := make([]any, len(v))
					for i := range v {
						dest[i] = &v[i]
					}
					if err := rows.Scan(dest...); err != nil {
						t.Fatal(err)
					}
					scanned = append(scanned, v)
				}
				if err := rows.Err(); err != nil {
					t.Fatal(err)
				}
				_ = rows.Close()
				if c.input != nil {
					t.Fatal("close retained input scratch")
				}
				if len(retained) != 256 || !reflect.DeepEqual(renderRows(retained), rendered) {
					t.Fatal("advancing/closing mutated retained Row results")
				}
				for i, row := range retained {
					for j, v := range row {
						if !reflect.DeepEqual(natural[i][j], valueToAny(v)) || !reflect.DeepEqual(scanned[i][j], valueToAny(v)) {
							t.Fatalf("retained Values/Scan changed at %d/%d", i, j)
						}
					}
				}
				if dirty {
					mustCreate(t, s, "ROLLBACK")
				}
			}
		})
	}
}

func TestRowReusePreparedIsolation(t *testing.T) {
	db := rowReuseDB(t, false)
	stmt, err := db.Prepare("SELECT id,a+1,txt,arr FROM t WHERE id >= $1 ORDER BY id LIMIT 12")
	if err != nil {
		t.Fatal(err)
	}
	// Same-session interleaving and cross-session concurrent shared-plan execution.
	run := func(start int) error {
		s := db.Session(SessionOptions{})
		defer s.Close()
		a, err := s.QueryPrepared(context.Background(), stmt, start)
		if err != nil {
			return err
		}
		defer a.Close()
		b, err := s.QueryPrepared(context.Background(), stmt, start+20)
		if err != nil {
			return err
		}
		defer b.Close()
		var held [][]Value
		for i := 0; i < 12; i++ {
			if !a.Next() || !b.Next() {
				return fmt.Errorf("early end: %v / %v", a.Err(), b.Err())
			}
			held = append(held, a.Row(), b.Row())
		}
		_ = a.Close() // early close without an exhaustion pull
		_ = b.Close()
		for i, row := range held {
			want := int64(start + i/2 + (i%2)*20)
			if row[0].Int != want || row[2].Render() != fmt.Sprintf("row-%d", want) {
				return fmt.Errorf("aliased row %d: %v", i, row)
			}
		}
		return nil
	}
	if err := run(1); err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	for i := 1; i <= 4; i++ {
		wg.Go(func() {
			for j := 0; j < 8; j++ {
				if err := run(i + j); err != nil {
					t.Error(err)
					return
				}
			}
		})
	}
	wg.Wait()
}
