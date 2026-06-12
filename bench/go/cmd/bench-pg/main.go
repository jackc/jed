// bench-pg benchmarks PostgreSQL via pgx (spec/design/benchmarks.md §6/§7). Connection
// settings come from the standard PG* env — the devcontainer points PGHOST at the Unix
// socket. Benchmark data lives in the jed_bench_<dataset> databases created by
// bench-setup.
package main

import (
	"context"
	"fmt"

	"github.com/jackc/pgx/v5"

	"jed-bench/internal/bench"
)

func main() {
	bench.Main(bench.Config{Engine: "postgres", Lang: "go", Variant: "pgx", Open: open})
}

type engine struct {
	conn *pgx.Conn
	n    int // prepared-statement name counter
}

func open(_, dataset string) (bench.Engine, error) {
	conn, err := pgx.Connect(context.Background(), "dbname=jed_bench_"+dataset)
	if err != nil {
		return nil, err
	}
	if dataset == "scratch" {
		// bench-setup created the empty scratch database once; reset it per run.
		if _, err := conn.Exec(context.Background(), "DROP TABLE IF EXISTS scratch"); err != nil {
			conn.Close(context.Background())
			return nil, err
		}
	}
	return &engine{conn: conn}, nil
}

func (e *engine) Exec(sql string) error {
	_, err := e.conn.Exec(context.Background(), sql)
	return err
}

func (e *engine) QueryInt(sql string) (int64, error) {
	var n int64
	err := e.conn.QueryRow(context.Background(), sql).Scan(&n)
	return n, err
}

func (e *engine) StoredFingerprint() (string, error) {
	var fp string
	err := e.conn.QueryRow(context.Background(),
		"SELECT value FROM _bench_meta WHERE key = 'fingerprint'").Scan(&fp)
	if err != nil {
		return "", nil // absent table/row reads as no fingerprint → stale
	}
	return fp, nil
}

func (e *engine) Close() error { return e.conn.Close(context.Background()) }

func (e *engine) Prepare(sql string) (bench.Stmt, error) {
	e.n++
	name := fmt.Sprintf("bench_%d", e.n)
	if _, err := e.conn.Prepare(context.Background(), name, sql); err != nil {
		return nil, err
	}
	return &pgStmt{conn: e.conn, name: name}, nil
}

type pgStmt struct {
	conn *pgx.Conn
	name string
}

func (s *pgStmt) Exec(args []any) error {
	_, err := s.conn.Exec(context.Background(), s.name, args...)
	return err
}

func (s *pgStmt) Query(args []any, sum *bench.Checksum) (int, error) {
	rows, err := s.conn.Query(context.Background(), s.name, args...)
	if err != nil {
		return 0, err
	}
	defer rows.Close()
	n := 0
	for rows.Next() {
		n++
		if sum == nil {
			continue
		}
		vals, err := rows.Values()
		if err != nil {
			return n, err
		}
		for _, v := range vals {
			switch x := v.(type) {
			case nil:
				sum.Null()
			case int16:
				sum.Int(int64(x))
			case int32:
				sum.Int(int64(x))
			case int64:
				sum.Int(x)
			case string:
				sum.Text(x)
			default:
				return n, fmt.Errorf("unexpected result type %T", v)
			}
		}
		sum.EndRow()
	}
	return n, rows.Err()
}

func (s *pgStmt) Close() error { return nil }
