package bench

import (
	"fmt"
	"os"
	"slices"
	"strings"
	"time"
)

// Engine is one open handle onto one dataset's database. Adapters in the cmd/bench-*
// binaries implement it per driver; everything else (param streams, timing, checksums,
// output) is shared here. Exec runs a bare statement (BEGIN/ROLLBACK/setup_sql);
// Prepare takes the corpus's $N SQL as authored — a driver that needs another
// placeholder form (SQLite ?N) rewrites it itself.
type Engine interface {
	Exec(sql string) error
	Prepare(sql string) (Stmt, error)
	// QueryInt runs a one-row, one-integer-column query (the post-run count(*) sanity
	// probe).
	QueryInt(sql string) (int64, error)
	// StoredFingerprint returns the fingerprint recorded by bench-setup for this
	// engine/dataset, or "" if none (the runner then aborts as stale). Scratch
	// databases are exempt (the runner never asks).
	StoredFingerprint() (string, error)
	Close() error
}

// Stmt is one prepared statement. Query folds each result row into sum (nil during
// warmup — rows are still fully consumed) and returns the row count; Exec is the
// no-result form for write benches.
type Stmt interface {
	Query(args []any, sum *Checksum) (int, error)
	Exec(args []any) error
	Close() error
}

// Config is one binary's identity + driver: the Open hook receives the dataset name
// ("small" | "large" | "scratch") and returns a ready Engine — for "scratch" that means
// a fresh, empty database (file engines: new temp file; PG: jed_bench_scratch with any
// prior scratch table dropped).
type Config struct {
	Engine  string // jed | postgres | sqlite
	Lang    string // go | rust | ts
	Variant string // core | pgx | modernc | mattn-cgo | ...
	Open    func(dataDir, dataset string) (Engine, error)
}

// Result is one JSONL line (spec/design/benchmarks.md §6); field order is the contract.
type Result struct {
	Schema      int    `json:"schema"`
	Bench       string `json:"bench"`
	Dataset     string `json:"dataset"`
	Engine      string `json:"engine"`
	Lang        string `json:"lang"`
	Variant     string `json:"variant"`
	Iterations  int    `json:"iterations"`
	Warmup      int    `json:"warmup"`
	TotalNs     int64  `json:"total_ns"`
	NsPerOp     int64  `json:"ns_per_op"`
	MinNs       int64  `json:"min_ns"`
	P50Ns       int64  `json:"p50_ns"`
	RowsTotal   int64  `json:"rows_total"`
	Checksum    string `json:"checksum"`
	Fingerprint string `json:"fingerprint"`
	StartedAt   string `json:"started_at"`
}

// Run executes every corpus bench that matches the filter and applies to cfg.Engine,
// appending one Result per bench. The caller owns output.
func Run(cfg Config, corpusDir, dataDir, filter string) ([]Result, error) {
	corpus, err := LoadCorpus(corpusDir)
	if err != nil {
		return nil, err
	}
	datasets, err := LoadDatasets(corpusDir)
	if err != nil {
		return nil, err
	}
	want, err := CorpusFingerprint(corpusDir)
	if err != nil {
		return nil, err
	}

	var results []Result
	for i := range corpus.Bench {
		b := &corpus.Bench[i]
		if filter != "" && !strings.Contains(b.Name, filter) {
			continue
		}
		if !b.RunsOn(cfg.Engine) {
			continue
		}
		fmt.Fprintf(os.Stderr, "%s/%s/%s: %s (%s) ...\n", cfg.Engine, cfg.Lang, cfg.Variant, b.Name, b.Dataset)
		r, err := runOne(cfg, b, datasets, dataDir, want)
		if err != nil {
			return nil, fmt.Errorf("bench %q: %w", b.Name, err)
		}
		results = append(results, r)
	}
	return results, nil
}

func runOne(cfg Config, b *Bench, datasets *Datasets, dataDir, want string) (Result, error) {
	res := Result{
		Schema: 1, Bench: b.Name, Dataset: b.Dataset, Engine: cfg.Engine, Lang: cfg.Lang,
		Variant: cfg.Variant, Iterations: b.Iterations, Warmup: b.Warmup,
		Fingerprint: want, StartedAt: time.Now().UTC().Format(time.RFC3339),
	}

	eng, err := cfg.Open(dataDir, b.Dataset)
	if err != nil {
		return res, err
	}
	defer eng.Close()

	if b.Dataset != "scratch" {
		stored, err := eng.StoredFingerprint()
		if err != nil {
			return res, err
		}
		if stored != want {
			return res, StaleErr(b.Dataset, cfg.Engine)
		}
	}
	for _, sql := range b.SetupSQLFor(cfg.Engine) {
		if err := eng.Exec(sql); err != nil {
			return res, fmt.Errorf("setup_sql %q: %w", sql, err)
		}
	}

	stmt, err := eng.Prepare(b.SQLFor(cfg.Engine))
	if err != nil {
		return res, err
	}
	defer stmt.Close()

	stream := NewParamStream(b)
	sum := NewChecksum()
	elapsed := make([]int64, 0, b.Iterations)

	iter := func(measured bool) error {
		var s *Checksum
		if measured {
			s = sum
		}
		switch b.Kind {
		case "query":
			start := time.Now()
			n, err := stmt.Query(stream.Next(), s)
			d := time.Since(start)
			if err != nil {
				return err
			}
			if measured {
				elapsed = append(elapsed, d.Nanoseconds())
				res.RowsTotal += int64(n)
				if b.ExpectRowsPerIter > 0 && n != b.ExpectRowsPerIter {
					return fmt.Errorf("expected %d rows per iteration, got %d", b.ExpectRowsPerIter, n)
				}
			}
		case "write_rollback":
			start := time.Now()
			if err := eng.Exec("BEGIN"); err != nil {
				return err
			}
			for j := 0; j < b.Batch; j++ {
				if err := stmt.Exec(stream.Next()); err != nil {
					return err
				}
			}
			if err := eng.Exec("ROLLBACK"); err != nil {
				return err
			}
			if measured {
				elapsed = append(elapsed, time.Since(start).Nanoseconds())
			}
		case "write_durable":
			start := time.Now()
			if err := stmt.Exec(stream.Next()); err != nil {
				return err
			}
			if measured {
				elapsed = append(elapsed, time.Since(start).Nanoseconds())
			}
		}
		return nil
	}

	for i := 0; i < b.Warmup; i++ {
		if err := iter(false); err != nil {
			return res, fmt.Errorf("warmup: %w", err)
		}
	}
	for i := 0; i < b.Iterations; i++ {
		if err := iter(true); err != nil {
			return res, err
		}
	}

	// Write kinds: the checksum is the post-run sanity count(*) (§6) — rollbacks held /
	// every durable commit landed — and it must also match the locally expected count.
	if b.Kind != "query" {
		table := insertTable(b.SQL)
		n, err := eng.QueryInt("SELECT count(*) FROM " + table)
		if err != nil {
			return res, err
		}
		var expect int64
		switch b.Kind {
		case "write_rollback":
			ds, err := datasets.Find(b.Dataset)
			if err != nil {
				return res, err
			}
			for _, t := range ds.Table {
				if t.Name == table {
					expect = int64(t.Rows)
				}
			}
		case "write_durable":
			expect = int64(b.Warmup + b.Iterations)
		}
		if n != expect {
			return res, fmt.Errorf("post-run count(*) of %s: got %d, want %d", table, n, expect)
		}
		sum.Int(n)
		sum.EndRow()
	}

	slices.Sort(elapsed)
	for _, d := range elapsed {
		res.TotalNs += d
	}
	res.NsPerOp = res.TotalNs / int64(b.Iterations)
	res.MinNs = elapsed[0]
	res.P50Ns = elapsed[(len(elapsed)-1)/2]
	res.Checksum = sum.Hex()
	return res, nil
}

// insertTable extracts the target table from "INSERT INTO <table> ..." for the
// post-run count.
func insertTable(sql string) string {
	fields := strings.Fields(sql)
	for i, f := range fields {
		if strings.EqualFold(f, "INTO") && i+1 < len(fields) {
			name, _, _ := strings.Cut(fields[i+1], "(")
			return name
		}
	}
	panic("write bench SQL has no INSERT INTO table: " + sql)
}
