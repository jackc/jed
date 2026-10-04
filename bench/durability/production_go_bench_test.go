package jed

// Copy into impl/go as production_durability_bench_test.go to run against either
// revision. No added dependencies. Database files must use the explicit bench
// directory, so /tmp tmpfs cannot silently turn the durability experiment into RAM.

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

type productionDurabilityStore struct {
	blockStore
	bytes, flushes, growths int64
}

func (s *productionDurabilityStore) writeAt(off int64, data []byte) error {
	s.bytes += int64(len(data))
	return s.blockStore.writeAt(off, data)
}

func (s *productionDurabilityStore) sync() error {
	s.flushes++
	return s.blockStore.sync()
}

func (s *productionDurabilityStore) setSize(n int64) error {
	old, err := s.blockStore.size()
	if err != nil {
		return err
	}
	if n > old {
		s.bytes += n - old
		s.flushes++
		s.growths++
	}
	return s.blockStore.setSize(n)
}

func BenchmarkProductionDurableCommit(b *testing.B) {
	root := os.Getenv("JED_DURABILITY_BENCH_DIR")
	if root == "" {
		b.Skip("set JED_DURABILITY_BENCH_DIR to a persistent filesystem directory")
	}
	for _, batch := range []int{1, 64} {
		b.Run(fmt.Sprintf("rows_%d", batch), func(b *testing.B) {
			const rows = 512
			dir, err := os.MkdirTemp(root, "production-durability-")
			if err != nil {
				b.Fatal(err)
			}
			defer os.RemoveAll(dir)
			path := filepath.Join(dir, "db.jed")
			db, err := create(path, defaultDatabaseOptions())
			if err != nil {
				b.Fatal(err)
			}
			exec := func(sql string) {
				if _, err := execute(db, sql); err != nil {
					b.Fatal(err)
				}
			}
			exec("CREATE TABLE t (id i32 PRIMARY KEY, v i64)")
			var seed strings.Builder
			seed.WriteString("INSERT INTO t VALUES ")
			for i := 0; i < rows; i++ {
				if i > 0 {
					seed.WriteString(",")
				}
				fmt.Fprintf(&seed, "(%d,0)", i)
			}
			exec(seed.String())
			counts := make([]int64, rows)
			store := &productionDurabilityStore{blockStore: db.paging.pgr.store}
			db.paging.pgr.store = store
			b.ResetTimer()
			for tx := 0; tx < b.N; tx++ {
				lo := (tx * 137) % (rows - batch + 1)
				exec(fmt.Sprintf("UPDATE t SET v=v+1 WHERE id >= %d AND id < %d", lo, lo+batch))
				for i := lo; i < lo+batch; i++ {
					counts[i]++
				}
			}
			b.StopTimer()
			b.ReportMetric(float64(store.bytes)/float64(b.N), "bytes/commit")
			b.ReportMetric(float64(store.flushes)/float64(b.N), "flushes/commit")
			b.ReportMetric(float64(store.growths)/float64(b.N), "growths/commit")
			if err := db.Close(); err != nil {
				b.Fatal(err)
			}
			db, err = open(path)
			if err != nil {
				b.Fatal(err)
			}
			defer db.Close()
			out, err := execute(db, "SELECT id,v FROM t ORDER BY id")
			if err != nil {
				b.Fatal(err)
			}
			if len(out.Rows) != rows {
				b.Fatalf("reopen rows=%d", len(out.Rows))
			}
			for i, row := range out.Rows {
				if row[0].Int != int64(i) || row[1].Int != counts[i] {
					b.Fatalf("row%d=%v expectedcount%d", i, row, counts[i])
				}
			}
		})
	}
}
