// bench-jed benchmarks the Go jed core (spec/design/benchmarks.md §6/§7).
package main

import (
	"fmt"
	jed "github.com/jackc/jed/impl/go"
	"os"
	"path/filepath"

	"jed-bench/internal/bench"
)

func main() {
	bench.Main(bench.Config{Engine: "jed", Lang: "go", Variant: "core", Open: open})
}

type engine struct {
	db      *jed.Database
	sess    *jed.Session // the persistent connection the bench drives (BEGIN/COMMIT span calls)
	dataDir string
	dataset string
	scratch string // temp dir holding the scratch file ("" otherwise)
}

func open(dataDir, dataset string) (bench.Engine, error) {
	e := &engine{dataDir: dataDir, dataset: dataset}
	if dataset == "scratch" {
		dir, err := os.MkdirTemp(dataDir, "scratch-")
		if err != nil {
			return nil, err
		}
		e.scratch = dir
		db, err := jed.CreateDatabase(filepath.Join(dir, "scratch.jed"), jed.DefaultDatabaseOptions())
		if err != nil {
			os.RemoveAll(dir)
			return nil, err
		}
		e.db = db
		e.sess = db.Session(jed.SessionOptions{})
		return e, nil
	}
	db, err := jed.OpenDatabase(filepath.Join(dataDir, dataset+".jed"))
	if err != nil {
		return nil, err
	}
	e.db = db
	e.sess = db.Session(jed.SessionOptions{})
	return e, nil
}

func (e *engine) Exec(sql string) error {
	_, err := e.sess.Execute(sql, nil)
	return err
}

func (e *engine) QueryInt(sql string) (int64, error) {
	rows, err := e.sess.Query(sql, nil)
	if err != nil {
		return 0, err
	}
	if !rows.Next() {
		return 0, fmt.Errorf("no row from %q", sql)
	}
	return rows.Row()[0].Int, nil
}

func (e *engine) StoredFingerprint() (string, error) {
	return bench.ReadSidecar(e.dataDir, e.dataset, "jed"), nil
}

func (e *engine) Close() error {
	if e.sess != nil {
		e.sess.Close()
	}
	if e.scratch != "" {
		os.RemoveAll(e.scratch)
	}
	return nil
}

func (e *engine) Prepare(sql string) (bench.Stmt, error) {
	stmt, err := e.sess.Prepare(sql)
	if err != nil {
		return nil, err
	}
	return &jedStmt{stmt: stmt}, nil
}

type jedStmt struct {
	stmt *jed.PreparedStatement
}

func bindArgs(args []any) []jed.Value {
	if len(args) == 0 {
		return nil
	}
	params := make([]jed.Value, len(args))
	for i, a := range args {
		switch x := a.(type) {
		case int64:
			params[i] = jed.IntValue(x)
		case string:
			params[i] = jed.TextValue(x)
		default:
			panic(fmt.Sprintf("unsupported arg type %T", a))
		}
	}
	return params
}

func (s *jedStmt) Exec(args []any) error {
	_, err := s.stmt.Execute(bindArgs(args))
	return err
}

func (s *jedStmt) Query(args []any, sum *bench.Checksum) (int, error) {
	// QueryValues is the raw []Value prepared-statement path; the bare `Query` name is now the
	// ergonomic ctx/variadic form (impl/go/ergonomic.go, the cancellation work).
	rows, err := s.stmt.QueryValues(bindArgs(args))
	if err != nil {
		return 0, err
	}
	n := 0
	for rows.Next() {
		n++
		if sum == nil {
			continue
		}
		for _, v := range rows.Row() {
			switch v.Kind {
			case jed.ValNull:
				sum.Null()
			case jed.ValInt:
				sum.Int(v.Int)
			case jed.ValText:
				sum.Text(v.Render())
			default:
				return n, fmt.Errorf("unexpected result kind %d", v.Kind)
			}
		}
		sum.EndRow()
	}
	return n, nil
}

func (s *jedStmt) Close() error { return nil }

// NewReaderPool opens a Database over the dataset file and mints n reader Sessions — the
// slice-7 concurrent-session path (spec/design/benchmarks.md §8.1). Each Session shares
// the one Database's committed snapshot + buffer pool and reads without blocking (§3), so
// the concurrent_read bench measures how the lock-free read path scales with readers.
func (e *engine) NewReaderPool(n int) (bench.ReaderPool, error) {
	db, err := jed.OpenDatabase(filepath.Join(e.dataDir, e.dataset+".jed"))
	if err != nil {
		return nil, err
	}
	readers := make([]*jed.Session, n)
	for i := range readers {
		readers[i] = db.ReadSession()
	}
	return &jedPool{db: db, readers: readers}, nil
}

type jedPool struct {
	db      *jed.Database
	readers []*jed.Session
}

func (p *jedPool) Reader(i int) bench.Reader { return jedReader{s: p.readers[i]} }

func (p *jedPool) Close() error {
	for _, s := range p.readers {
		s.Close()
	}
	return p.db.Close()
}

// jedReader runs one query through a reader Session, re-parsing the SQL each call — the
// host session API has no prepared-statement form (benchmarks.md §8.1).
type jedReader struct{ s *jed.Session }

func (r jedReader) Query(sql string, args []any, sum *bench.Checksum) (int, error) {
	rows, err := r.s.Query(sql, bindArgs(args))
	if err != nil {
		return 0, err
	}
	n := 0
	for rows.Next() {
		n++
		if sum == nil {
			continue
		}
		for _, v := range rows.Row() {
			switch v.Kind {
			case jed.ValNull:
				sum.Null()
			case jed.ValInt:
				sum.Int(v.Int)
			case jed.ValText:
				sum.Text(v.Render())
			default:
				return n, fmt.Errorf("unexpected result kind %d", v.Kind)
			}
		}
		sum.EndRow()
	}
	return n, nil
}
