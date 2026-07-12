package bench

import (
	"errors"
	"fmt"
	"os"
	"slices"
	"strings"
	"sync"
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

// ConcurrentEngine is the OPTIONAL capability a driver implements to run the
// concurrent_read kind (spec/design/benchmarks.md §8.1): NewReaderPool opens `n`
// independent readers over the same committed data — for jed, one Database and `n` reader
// Sessions (the slice-7 convergence, session.md §2.4/§10). A driver that does not
// implement it (the Ruby gem, the wasm wrap) makes the runner SKIP the bench.
type ConcurrentEngine interface {
	NewReaderPool(n int) (ReaderPool, error)
}

// ReaderPool is a set of independent readers, one per concurrency slot. Reader(i) returns
// the i-th reader (0-based), each safe to drive from its own goroutine.
type ReaderPool interface {
	Reader(i int) Reader
	Close() error
}

// Reader runs one query at a time (re-parsing the SQL — the host session API has no
// prepared-statement form; benchmarks.md §8.1), folding rows into sum (nil during warmup).
type Reader interface {
	Query(sql string, args []any, sum *Checksum) (int, error)
}

// errSkip is the sentinel runOne returns when a bench does not apply to this driver (a
// concurrent_read on a driver without ConcurrentEngine). Run drops it and continues.
var errSkip = errors.New("bench skipped for this driver")

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
	Readers     int    `json:"readers"` // concurrent_read: reader count; 0 for the other kinds
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
		if errors.Is(err, errSkip) {
			continue
		}
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

	if b.Kind == "concurrent_read" {
		ce, ok := eng.(ConcurrentEngine)
		if !ok {
			fmt.Fprintf(os.Stderr, "  skip: %s/%s/%s has no concurrent_read support\n", cfg.Engine, cfg.Lang, cfg.Variant)
			return res, errSkip
		}
		return runConcurrent(cfg, b, ce, res)
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
		table := writeTable(b.SQL)
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

// runConcurrent runs a concurrent_read bench (spec/design/benchmarks.md §8.1): it opens
// `readers` independent reader Sessions over one shared Database and drives the point
// lookup `iterations` times, split into `readers` contiguous param blocks (one per
// reader), in parallel. The metric is THROUGHPUT — total_ns is the wall clock of the
// concurrent measured phase, so ns_per_op = wall/iterations falls as readers scale the
// §3 lock-free read path. The answer checksum is partition-folded (each reader hashes its
// block in order; blocks combine in reader-index order), so it is identical regardless of
// goroutine scheduling, and matches the Rust/TS cores running the identical partition.
func runConcurrent(cfg Config, b *Bench, ce ConcurrentEngine, res Result) (Result, error) {
	res.Readers = b.Readers
	sql := b.SQLFor(cfg.Engine)

	stream := NewParamStream(b)
	warm := make([][]any, b.Warmup)
	for i := range warm {
		warm[i] = stream.Next()
	}
	meas := make([][]any, b.Iterations)
	for i := range meas {
		meas[i] = stream.Next()
	}
	warmBlocks := partition(warm, b.Readers)
	measBlocks := partition(meas, b.Readers)

	pool, err := ce.NewReaderPool(b.Readers)
	if err != nil {
		return res, err
	}
	defer pool.Close()

	// pass runs every reader's block concurrently, one goroutine per reader; sums[r] is
	// nil during warmup. Returns the first error any reader hit.
	pass := func(blocks [][][]any, sums []*Checksum, elapsed [][]int64, rows []int64) error {
		errs := make([]error, b.Readers)
		var wg sync.WaitGroup
		for r := 0; r < b.Readers; r++ {
			wg.Add(1)
			go func(r int) {
				defer wg.Done()
				rd := pool.Reader(r)
				for _, args := range blocks[r] {
					var s *Checksum
					if sums != nil {
						s = sums[r]
					}
					var t0 time.Time
					if elapsed != nil {
						t0 = time.Now()
					}
					n, err := rd.Query(sql, args, s)
					if err != nil {
						errs[r] = err
						return
					}
					if elapsed != nil {
						elapsed[r] = append(elapsed[r], time.Since(t0).Nanoseconds())
						rows[r] += int64(n)
						if b.ExpectRowsPerIter > 0 && n != b.ExpectRowsPerIter {
							errs[r] = fmt.Errorf("expected %d rows per iteration, got %d", b.ExpectRowsPerIter, n)
							return
						}
					}
				}
			}(r)
		}
		wg.Wait()
		return errors.Join(errs...)
	}

	// Pass 1 — warmup, untimed: populate the shared buffer pool before measuring.
	if err := pass(warmBlocks, nil, nil, nil); err != nil {
		return res, fmt.Errorf("warmup: %w", err)
	}

	// Pass 2 — measured, timed by wall clock.
	sums := make([]*Checksum, b.Readers)
	for r := range sums {
		sums[r] = NewChecksum()
	}
	elapsed := make([][]int64, b.Readers)
	rows := make([]int64, b.Readers)
	start := time.Now()
	if err := pass(measBlocks, sums, elapsed, rows); err != nil {
		return res, err
	}
	wall := time.Since(start).Nanoseconds()

	combined := NewChecksum()
	var all []int64
	var rowsTotal int64
	for r := 0; r < b.Readers; r++ {
		combined.Text(sums[r].Hex())
		all = append(all, elapsed[r]...)
		rowsTotal += rows[r]
	}
	slices.Sort(all)
	res.TotalNs = wall
	res.NsPerOp = wall / int64(b.Iterations)
	res.MinNs = all[0]
	res.P50Ns = all[(len(all)-1)/2]
	res.RowsTotal = rowsTotal
	res.Checksum = combined.Hex()
	return res, nil
}

// partition tiles items into n contiguous blocks (the first len%n blocks get one extra),
// the deterministic per-reader split the concurrent checksum folds over.
func partition[T any](items []T, n int) [][]T {
	blocks := make([][]T, n)
	base, extra := len(items)/n, len(items)%n
	idx := 0
	for r := 0; r < n; r++ {
		size := base
		if r < extra {
			size++
		}
		blocks[r] = items[idx : idx+size]
		idx += size
	}
	return blocks
}

// writeTable extracts the target table of a write statement — the word after INTO
// (INSERT), UPDATE, or FROM (DELETE) — for the post-run count.
func writeTable(sql string) string {
	fields := strings.Fields(sql)
	for i, f := range fields {
		if (strings.EqualFold(f, "INTO") || strings.EqualFold(f, "UPDATE") || strings.EqualFold(f, "FROM")) && i+1 < len(fields) {
			name, _, _ := strings.Cut(fields[i+1], "(")
			return name
		}
	}
	panic("write bench SQL has no INSERT / UPDATE / DELETE target table: " + sql)
}
