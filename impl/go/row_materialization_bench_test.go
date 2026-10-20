package jed

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"runtime/pprof"
	"strings"
	"testing"
)

// Complete queries, including cursor drain, checksum and exact cost checks.
// Kept tag-independent so the same source measures the clean HEAD engine.
func BenchmarkRowMaterialization(b *testing.B) {
	if os.Getenv("JED_ROW_PROFILE") != "" {
		runtime.MemProfileRate = 1
	}
	for _, host := range []string{"memory", "file"} {
		b.Run(host, func(b *testing.B) {
			opts := CreateOptions{SkipFsync: true}
			if host == "file" {
				opts.Path = filepath.Join(b.TempDir(), "kernels.jed")
			}
			db, err := CreateDatabase(opts)
			if err != nil {
				b.Fatal(err)
			}
			exec := func(sql string) {
				if _, err := queryOutcome(db, sql, nil); err != nil {
					b.Fatal(err)
				}
			}
			exec("CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)")
			for start := 0; start < 4096; start += 256 {
				var sql strings.Builder
				sql.WriteString("INSERT INTO t VALUES ")
				for i := start; i < start+256; i++ {
					if i > start {
						sql.WriteByte(',')
					}
					fmt.Fprintf(&sql, "(%d,%d,%d)", i, i%100, i%17+1)
				}
				exec(sql.String())
			}
			exec("CREATE TABLE v (id i32 PRIMARY KEY, a i32, txt text, bytes bytea)")
			exec("INSERT INTO v SELECT id, CASE WHEN id%7=0 THEN NULL ELSE a END, CASE WHEN id%11=0 THEN NULL ELSE 'payload-' || id::text END, decode('abcdef','hex') FROM t")
			if host == "file" {
				db.Close()
				db, err = OpenDatabaseWithOptions(opts.Path, OpenOptions{SkipFsync: true})
				if err != nil {
					b.Fatal(err)
				}
			}
			defer db.Close()
			s := db.Session(SessionOptions{})
			defer s.Close()
			for _, q := range []struct {
				name, sql    string
				adhoc, first bool
				limit        int64
			}{
				{name: "projection", sql: "SELECT a,b FROM t ORDER BY id"},
				{name: "nullable", sql: "SELECT a+1 FROM v ORDER BY id"},
				{name: "varlen", sql: "SELECT txt,bytes FROM v ORDER BY id"},
				{name: "columnar_projection", sql: "SELECT a,b FROM t"},
				{name: "arithmetic", sql: "SELECT (a+b)*(a-b) FROM t ORDER BY id"},
				{name: "filter", sql: "SELECT id FROM t WHERE a+b > $1 ORDER BY id"},
				{name: "finite", sql: "SELECT (a+b)*(a-b) FROM t ORDER BY id", limit: 10000000},
				{name: "small", sql: "SELECT (a+b)*(a-b) FROM t ORDER BY id LIMIT 16"},
				{name: "offset", sql: "SELECT a+b FROM t WHERE a+b > $1 ORDER BY id LIMIT 16 OFFSET 16"},
				{name: "point_hit", sql: "SELECT a+b FROM t WHERE id=$1"},
				{name: "point_miss", sql: "SELECT a+b FROM t WHERE id=$1"},
				{name: "scalar", sql: "SELECT $1+2"},
				{name: "unsupported", sql: "SELECT abs(a) FROM t ORDER BY id"},
				{name: "columnar", sql: "SELECT sum(a) FROM t"},
				{name: "boolean", sql: "SELECT id FROM t WHERE (a+b > $1 AND b<12) OR a IS NULL ORDER BY id"},
				{name: "coalesce", sql: "SELECT coalesce(CASE WHEN a>50 THEN a+b ELSE NULL END,b) FROM t ORDER BY id"},
				{name: "case", sql: "SELECT CASE WHEN a>50 THEN a ELSE b END FROM t ORDER BY id"},
				{name: "adhoc", sql: "SELECT a+b FROM t ORDER BY id LIMIT 16", adhoc: true},
				{name: "first_call", sql: "SELECT a+b FROM t ORDER BY id LIMIT 16", first: true},
			} {
				b.Run(q.name, func(b *testing.B) {
					s.SetMaxCost(q.limit)
					stmt, err := s.Prepare(q.sql)
					if err != nil {
						b.Fatal(err)
					}
					var params [2][]Value
					if strings.Contains(q.sql, "$1") {
						params[0] = []Value{IntValue(50)}
						params[1] = []Value{IntValue(51)}
						if q.name == "point_miss" {
							params[0], params[1] = []Value{IntValue(10050)}, []Value{IntValue(10051)}
						}
					}
					drive := func(index int) (uint32, int64) {
						var rows *Rows
						var err error
						if q.adhoc {
							rows, err = s.queryValues(q.sql, params[index])
						} else {
							p := stmt
							if q.first {
								p, err = s.Prepare(q.sql)
								if err != nil {
									b.Fatal(err)
								}
							}
							rows, err = s.queryStmt(p.ast, params[index], &p.sc, &p.ic)
						}
						if err != nil {
							b.Fatal(err)
						}
						checksum := uint32(2166136261)
						for rows.Next() {
							for _, v := range rows.Row() {
								checksum = (checksum ^ uint32(v.Kind)) * 16777619
								checksum = (checksum ^ uint32(v.Int)) * 16777619
								if v.Kind == ValText || v.Kind == ValBytea {
									for k := range len(v.str()) {
										checksum = (checksum ^ uint32(v.str()[k])) * 16777619
									}
								}
							}
						}
						if err := rows.Err(); err != nil {
							b.Fatal(err)
						}
						cost := rows.Cost()
						rows.Close()
						return checksum, cost
					}
					var sums [2]uint32
					var costs [2]int64
					for i := 0; i < 2; i++ {
						sums[i], costs[i] = drive(i)
					}
					profile := os.Getenv("JED_ROW_PROFILE")
					snapshot := func(suffix string) {
						runtime.GC()
						f, err := os.Create(profile + suffix)
						if err != nil {
							b.Fatal(err)
						}
						if err := pprof.WriteHeapProfile(f); err != nil {
							b.Fatal(err)
						}
						if err := f.Close(); err != nil {
							b.Fatal(err)
						}
					}
					if profile != "" {
						snapshot("-setup.pprof")
					}
					b.ReportAllocs()
					b.ResetTimer()
					for i := 0; i < b.N; i++ {
						j := i % 2
						sum, cost := drive(j)
						if sum != sums[j] || cost != costs[j] {
							b.Fatal("result/cost drift")
						}
					}
					b.StopTimer()
					if profile != "" {
						snapshot("-total.pprof")
					}
					b.ReportMetric(float64(sums[0]), "checksum")
					b.ReportMetric(float64(costs[0]), "cost")
					b.ReportMetric(float64(sums[1]), "checksum1")
					b.ReportMetric(float64(costs[1]), "cost1")
				})
			}
		})
	}
}
