package jed

// The experiment installs this same test in its clean baseline archive. Compare
// every pull and guard prefix, not merely the final answer. No candidate-only API.
import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRowMaterializationTrace(t *testing.T) {
	path := os.Getenv("JED_ROW_TRACE")
	if path == "" {
		t.Skip("experiment differential transcript")
	}
	var trace strings.Builder
	for _, file := range []bool{false, true} {
		opts := CreateOptions{SkipFsync: true}
		if file {
			opts.Path = filepath.Join(t.TempDir(), "trace.jed")
		}
		db, err := CreateDatabase(opts)
		if err != nil {
			t.Fatal(err)
		}
		for _, sql := range []string{
			"CREATE TABLE t (id i32 PRIMARY KEY, a i32, txt text)",
			"INSERT INTO t VALUES (1,3,'abc'),(2,NULL,NULL),(3,0,'def'),(4,2,'ghi')",
		} {
			if _, err := queryOutcome(db, sql, nil); err != nil {
				t.Fatal(err)
			}
		}
		if file {
			_ = db.Close()
			db, err = OpenDatabaseWithOptions(opts.Path, OpenOptions{SkipFsync: true})
			if err != nil {
				t.Fatal(err)
			}
		}
		for _, sql := range []string{
			"SELECT id,a+1,txt FROM t WHERE a IS NULL OR a+2>2 ORDER BY id",
			"SELECT id,txt FROM t ORDER BY id DESC LIMIT 2 OFFSET 1",
			"SELECT 12/a,txt FROM t ORDER BY id",
			"SELECT 12/a FROM t ORDER BY id LIMIT 0",
			"SELECT txt,ROW(id,a),ARRAY[a,id],(SELECT t.txt) FROM t ORDER BY id",
			"SELECT DISTINCT a FROM t ORDER BY a LIMIT 2",
			"SELECT id,a FROM t", // columnar control and finite-memory failure
			"SELECT txt FROM t WHERE id=1",
		} {
			s := db.Session(SessionOptions{})
			out, err := queryOutcome(s, "EXPLAIN "+sql, nil)
			if err != nil {
				t.Fatal(err)
			}
			fmt.Fprintf(&trace, "host=%v sql=%s plan=%v\n", file, sql, renderRows(out.Rows))
			s.Close()
			for _, budget := range []int64{1, 128, 1 << 20} {
				for mode := 0; mode < 3; mode++ { // statement ceiling, lifetime ceiling, cancellation boundary
					for prefix := int64(0); prefix <= 96; prefix++ {
						s := db.Session(SessionOptions{MaxQueryMemoryBytes: budget})
						if mode == 0 {
							s.SetMaxCost(prefix)
						} else if mode == 1 {
							s.SetLifetimeMaxCost(prefix)
						}
						pulls := int64(0)
						ResetQueryMemoryPeak()
						ctx, cancel := context.WithCancel(context.Background())
						if mode == 2 && prefix == 1 {
							cancel()
						}
						rows, err := s.Query(ctx, sql)
						fmt.Fprintf(&trace, "budget=%d mode=%d prefix=%d open=%v", budget, mode, prefix, err)
						if err == nil {
							fmt.Fprintf(&trace, " cost=%d types=%v", rows.Cost(), rows.ColumnTypes())
							for {
								pulls++
								if mode == 2 && prefix > 1 && pulls >= prefix-1 {
									cancel()
								}
								if !rows.Next() {
									break
								}
								fmt.Fprintf(&trace, " | %v cost=%d used=%d pulls=%d", renderRows([][]Value{rows.Row()}), rows.Cost(), *s.engine.session.queryBytes, pulls)
							}
							fmt.Fprintf(&trace, " end=%v cost=%d used=%d peak=%d pulls=%d", rows.Err(), rows.Cost(), *s.engine.session.queryBytes, QueryMemoryPeak(), pulls)
							_ = rows.Close()
						}
						cancel()
						s.Close()
						trace.WriteByte('\n')
					}
				}
			}
		}
		db.Close()
	}
	if err := os.WriteFile(path, []byte(trace.String()), 0o600); err != nil {
		t.Fatal(err)
	}
}
