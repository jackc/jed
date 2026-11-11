package jed

import (
	"context"
	"path/filepath"
	"testing"
)

// BenchmarkDecimalLanes runs the bench corpus's decimal_* statements (bench/corpus/benchmarks.toml)
// over the same 200k-row file-backed ledger, so the lanes can be profiled in-package:
//
//	go test -run '^$' -bench 'DecimalLanes/sum_scan' -cpuprofile cpu.out
func BenchmarkDecimalLanes(b *testing.B) {
	path := filepath.Join(b.TempDir(), "ledger.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		b.Fatal(err)
	}
	defer db.Close()
	ctx := context.Background()
	for _, sql := range []string{
		"CREATE TABLE ledger (id bigint PRIMARY KEY, account integer, region integer, qty bigint, amount numeric(12,2), fee numeric(12,2))",
		"INSERT INTO ledger SELECT g, ((g::bigint * 7919) % 5000)::integer, (g % 50)::integer, (g::bigint * 104729) % 1000000, ((g::bigint * 7919) % 100000000)::numeric / 100, ((g::bigint * 104729) % 10000)::numeric / 100 FROM generate_series(1, 200000) AS g",
	} {
		if _, err := db.Exec(ctx, sql); err != nil {
			b.Fatal(err)
		}
	}
	for _, q := range []struct{ name, sql string }{
		{"sum_scan", "SELECT count(*), sum(amount)::text FROM ledger"},
		{"sum_bigint", "SELECT sum(qty)::text FROM ledger"},
		{"avg_scan", "SELECT avg(amount)::text FROM ledger"},
		{"filter_scan", "SELECT count(*), max(amount)::text FROM ledger WHERE amount > 500000::bigint"},
		{"arith_scan", "SELECT sum(amount * fee)::text, sum(amount + fee)::text FROM ledger"},
		{"group_by", "SELECT count(*), sum(g.n)::bigint, sum(g.t)::text, sum(g.m)::text FROM (SELECT region, count(*) AS n, sum(amount) AS t, max(fee) AS m FROM ledger GROUP BY region) g"},
		{"group_by_high_card", "SELECT count(*), sum(g.n)::bigint, sum(g.t)::text FROM (SELECT account, count(*) AS n, sum(amount) AS t FROM ledger GROUP BY account) g"},
		{"group_by_ordered", "SELECT region, count(*), sum(amount)::text FROM ledger GROUP BY region ORDER BY region"},
		{"order_by_limit", "SELECT id, amount::text AS amount_text FROM ledger ORDER BY amount DESC, id ASC LIMIT 100"},
	} {
		b.Run(q.name, func(b *testing.B) {
			s := db.Session(SessionOptions{})
			defer s.Close()
			b.ReportAllocs()
			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				rows, err := s.Query(ctx, q.sql)
				if err != nil {
					b.Fatal(err)
				}
				for rows.Next() {
					_ = rows.Row()
				}
				if err := rows.Err(); err != nil {
					b.Fatal(err)
				}
				rows.Close()
			}
		})
	}
}
