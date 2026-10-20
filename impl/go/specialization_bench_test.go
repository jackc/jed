package jed

import (
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

// Complete queries, including cursor drain, checksum and exact cost checks.
// Kept tag-independent so the same source measures the clean HEAD engine.
func BenchmarkSpecialization(b *testing.B) {
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
					b.ReportMetric(float64(sums[0]), "checksum")
					b.ReportMetric(float64(costs[0]), "cost")
					b.ReportMetric(float64(sums[1]), "checksum1")
					b.ReportMetric(float64(costs[1]), "cost1")
				})
			}
		})
	}
}
