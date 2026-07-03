// stress is the Go runner for Layer 3 of the concurrency contract
// (spec/design/concurrency-testing.md §6): the parallelism-stress format. Unlike the Layer 1/2
// `# format: concurrency` schedules (an explicit total order, run inside the conformance harness),
// a `stress/*.stress.toml` file has NO order — writers and readers run concurrently and
// correctness is checked by INVARIANTS, not a transcript. It is bench-family (outside `rake ci`):
// timing-nondeterministic, but its answers are still checked (the confluent final state + a
// cross-core answer checksum). This binary lives in the bench module so it can reuse the shared
// splitmix64 PRNG and the FNV-1a answer checksum (benchmarks.md §6) with no new dependency.
//
// Two execution modes drive the SAME worker definitions:
//   - threaded   (Go's native mode): one goroutine per worker over the shared handle; writers
//     contend on the single-writer gate for real, readers pin real snapshots. Run under `-race`
//     (via `rake stress`) this exercises the actual concurrent code paths. A watchdog flags a
//     deadlock as a timeout.
//   - sequential (`--sequential`): the seeded interleaver (§6) — the same algorithm the
//     single-thread TS core uses. Deterministic given the file's seed; never truly blocks (a
//     writer is scheduled to acquire the gate only while it is free). Useful for a deterministic
//     cross-check and for debugging without the race detector.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	jed "github.com/jackc/jed/impl/go"

	"jed-bench/internal/bench"

	"github.com/BurntSushi/toml"
)

// deadlockTimeout bounds a threaded run: the balance workload finishes in well under a second, so a
// minute with no completion means a worker is wedged (the §6 deadlock check).
const deadlockTimeout = 60 * time.Second

// --- the stress file format (concurrency-testing.md §6) --------------------------------------

type stressFile struct {
	Meta   stressMeta     `toml:"meta"`
	Setup  stressSetup    `toml:"setup"`
	Worker []stressWorker `toml:"worker"`
	Final  *stressFinal   `toml:"final"`
}

type stressMeta struct {
	Name        string `toml:"name"`
	Description string `toml:"description"`
	Parallel    string `toml:"parallel"` // "optional" → sequential fallback on single-thread cores; "required" → skip there
	Seed        uint64 `toml:"seed"`
}

type stressSetup struct {
	SQL []string `toml:"sql"`
}

type stressWorker struct {
	Kind            string `toml:"kind"` // "writer" | "reader"
	Count           int    `toml:"count"`
	Iterations      int    `toml:"iterations"`
	Op              string `toml:"op"`               // writer: BEGIN; … ; COMMIT;
	InvariantQuery  string `toml:"invariant_query"`  // reader
	InvariantExpect string `toml:"invariant_expect"` // reader: the rendered scalar it must return
}

type stressFinal struct {
	Query             string    `toml:"query"`
	Expect            [][]int64 `toml:"expect"` // confluent final rows (omit for invariant-only)
	CrossCoreChecksum bool      `toml:"cross_core_checksum"`
}

// result is one JSONL line — one per stress file. `rake stress` aggregates these across cores.
type result struct {
	Schema          int    `json:"schema"`
	Name            string `json:"name"`
	Lang            string `json:"lang"`
	Mode            string `json:"mode"`   // "threaded" | "sequential" | "skipped"
	Status          string `json:"status"` // "pass" | "fail" | "skip"
	InvariantChecks int64  `json:"invariant_checks"`
	Writers         int    `json:"writers"`
	WriterIters     int    `json:"writer_iters"`
	FinalOK         bool   `json:"final_ok"`
	Checksum        string `json:"checksum"`
	CrossCore       bool   `json:"cross_core_checksum"`
	DurationMs      int64  `json:"duration_ms"`
	Error           string `json:"error,omitempty"`
}

// parseOp splits a writer's `op` into the executable statements: bare BEGIN/COMMIT/ROLLBACK are
// transaction MARKERS, mapped onto the handle's open/commit (§6), so they are dropped here.
// memDB builds a fresh in-memory database, unwrapping the infallible in-memory CreateDatabase
// (spec/design/api.md §2.1.1 — the in-memory create cannot fail).
func memDB() *jed.Database {
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		panic("in-memory CreateDatabase is infallible: " + err.Error())
	}
	return db
}

func parseOp(op string) []string {
	var stmts []string
	for _, part := range strings.Split(op, ";") {
		s := strings.TrimSpace(part)
		if s == "" {
			continue
		}
		switch strings.ToUpper(s) {
		case "BEGIN", "COMMIT", "ROLLBACK":
			continue
		}
		stmts = append(stmts, s)
	}
	return stmts
}

// queryScalar runs a single-column, single-row query on a read handle and renders the scalar to its
// canonical string (Value.Render — the same deterministic rendering the conformance harness uses, so
// e.g. the decimal `sum(bigint)` result `1000` renders identically across cores). The invariant is a
// string comparison against `invariant_expect`, not folded into the cross-core checksum.
func queryScalar(r *jed.Session, sql string) (string, error) {
	rows, err := r.Query(sql, nil)
	if err != nil {
		return "", err
	}
	if !rows.Next() {
		return "", fmt.Errorf("no row from %q", sql)
	}
	return rows.Row()[0].Render(), nil
}

// --- setup + the final check (shared by both modes) ------------------------------------------

// setup runs the file's setup SQL as one durable write transaction (committed version 1).
func setup(db *jed.Database, f *stressFile) error {
	w := db.WriteSession()
	for _, s := range f.Setup.SQL {
		if _, err := w.Execute(s, nil); err != nil {
			_ = w.Rollback()
			return fmt.Errorf("setup %q: %w", s, err)
		}
	}
	return w.Commit()
}

// checkFinal runs `[final].query` against the final committed snapshot, folds the rows into the
// answer checksum, and compares them to `[final].expect` (when present — a confluent workload).
func checkFinal(db *jed.Database, fin *stressFinal) (checksum string, ok bool, err error) {
	if fin == nil {
		return "", true, nil
	}
	r := db.ReadSession()
	defer r.Close()
	rows, err := r.Query(fin.Query, nil)
	if err != nil {
		return "", false, err
	}
	sum := bench.NewChecksum()
	var got [][]int64
	for rows.Next() {
		ints := make([]int64, 0, len(rows.Row()))
		for _, v := range rows.Row() {
			switch v.Kind {
			case jed.ValInt:
				sum.Int(v.Int)
				ints = append(ints, v.Int)
			case jed.ValNull:
				sum.Null()
			default:
				return "", false, fmt.Errorf("stress final query must return integer columns, got kind %d", v.Kind)
			}
		}
		sum.EndRow()
		got = append(got, ints)
	}
	return sum.Hex(), finalEqual(got, fin.Expect), nil
}

// finalEqual compares the observed final rows to the pinned expectation. An empty expectation means
// the workload is not confluent (invariant-only) — the exact-rows check is skipped (ok by default).
func finalEqual(got, want [][]int64) bool {
	if len(want) == 0 {
		return true
	}
	if len(got) != len(want) {
		return false
	}
	for i := range got {
		if len(got[i]) != len(want[i]) {
			return false
		}
		for j := range got[i] {
			if got[i][j] != want[i][j] {
				return false
			}
		}
	}
	return true
}

// --- threaded mode (Go's native mode; real goroutines, race-detector coverage) ---------------

// runWriter runs one writer worker: `iterations` transactions, each taking the gate (db.WriteSession()
// blocks while another writer holds it — the real contention path), running `op`, and committing.
func runWriter(db *jed.Database, stmts []string, iterations int) error {
	for i := 0; i < iterations; i++ {
		w := db.WriteSession()
		for _, s := range stmts {
			if _, err := w.Execute(s, nil); err != nil {
				_ = w.Rollback()
				return fmt.Errorf("writer exec %q: %w", s, err)
			}
		}
		if err := w.Commit(); err != nil {
			return fmt.Errorf("writer commit: %w", err)
		}
	}
	return nil
}

// runReader runs one reader worker: `iterations` snapshots, each asserting the invariant.
func runReader(db *jed.Database, query, expect string, iterations int, checks *int64) error {
	for i := 0; i < iterations; i++ {
		r := db.ReadSession()
		got, err := queryScalar(r, query)
		r.Close()
		if err != nil {
			return err
		}
		atomic.AddInt64(checks, 1)
		if got != expect {
			return fmt.Errorf("invariant %q: got %s, want %s", query, got, expect)
		}
	}
	return nil
}

// runThreaded spawns one goroutine per worker, all concurrent, and waits with a deadlock watchdog.
func runThreaded(db *jed.Database, f *stressFile) (int64, error) {
	var (
		wg     sync.WaitGroup
		checks int64
		errMu  sync.Mutex
		firstE error
	)
	fail := func(e error) {
		errMu.Lock()
		if firstE == nil {
			firstE = e
		}
		errMu.Unlock()
	}
	for _, wk := range f.Worker {
		for c := 0; c < wk.Count; c++ {
			wg.Add(1)
			go func(wk stressWorker) {
				defer wg.Done()
				switch wk.Kind {
				case "writer":
					if e := runWriter(db, parseOp(wk.Op), wk.Iterations); e != nil {
						fail(e)
					}
				case "reader":
					if e := runReader(db, wk.InvariantQuery, wk.InvariantExpect, wk.Iterations, &checks); e != nil {
						fail(e)
					}
				default:
					fail(fmt.Errorf("unknown worker kind %q", wk.Kind))
				}
			}(wk)
		}
	}
	done := make(chan struct{})
	go func() { wg.Wait(); close(done) }()
	select {
	case <-done:
	case <-time.After(deadlockTimeout):
		return atomic.LoadInt64(&checks), fmt.Errorf("deadlock: workers did not finish within %s", deadlockTimeout)
	}
	return checks, firstE
}

// --- seeded-sequential mode (the §6 interleaver; the same algorithm TS uses) ------------------

// seqWorker is one worker modeled as a program of atomic ops over the shared handle (writer:
// acquire · exec… · commit; reader: open · check · close), advanced one op at a time by the
// interleaver. iter counts completed iterations; op is the cursor within the current iteration.
type seqWorker struct {
	kind       string
	stmts      []string // writer
	query      string   // reader
	expect     string   // reader
	iterations int
	iter       int
	op         int
	wh         *jed.Session
	rh         *jed.Session
}

// done reports whether the worker has run all its iterations.
func (w *seqWorker) done() bool { return w.iter >= w.iterations }

// runnable reports whether the worker's next op can run now. The only gated op is a writer's
// acquire (op 0), which needs the single-writer gate free; every other op (the worker already
// holds its handle, or it is a reader) is always runnable — so the gate holder always makes
// progress and the interleaver never deadlocks.
func (w *seqWorker) runnable(gateFree bool) bool {
	if w.done() {
		return false
	}
	if w.kind == "writer" && w.op == 0 {
		return gateFree
	}
	return true
}

// runSequential walks the workers through the seeded interleaver: at each step the splitmix64
// stream picks one runnable worker (fixed index order) and advances it one op. Deterministic given
// the seed; reproduces the logical interleavings without ever truly blocking.
func runSequential(db *jed.Database, f *stressFile) (int64, error) {
	var workers []*seqWorker
	for _, wk := range f.Worker {
		for c := 0; c < wk.Count; c++ {
			switch wk.Kind {
			case "writer":
				workers = append(workers, &seqWorker{kind: "writer", stmts: parseOp(wk.Op), iterations: wk.Iterations})
			case "reader":
				workers = append(workers, &seqWorker{kind: "reader", query: wk.InvariantQuery, expect: wk.InvariantExpect, iterations: wk.Iterations})
			default:
				return 0, fmt.Errorf("unknown worker kind %q", wk.Kind)
			}
		}
	}
	prng := bench.NewPrng(f.Meta.Seed)
	gateHolder := -1
	var checks int64
	for {
		var runnable []int
		for i, w := range workers {
			if w.runnable(gateHolder == -1) {
				runnable = append(runnable, i)
			}
		}
		if len(runnable) == 0 {
			break // all workers done (the gate holder is always runnable, so this means none remain)
		}
		idx := runnable[int(prng.Next()%uint64(len(runnable)))]
		if err := stepSeq(db, workers[idx], idx, &gateHolder, &checks); err != nil {
			return checks, err
		}
	}
	return checks, nil
}

// stepSeq advances one worker by one atomic op.
func stepSeq(db *jed.Database, w *seqWorker, idx int, gateHolder *int, checks *int64) error {
	switch w.kind {
	case "writer":
		switch {
		case w.op == 0: // acquire (the gate is free — guaranteed by runnable)
			w.wh = db.WriteSession()
			*gateHolder = idx
			w.op++
		case w.op <= len(w.stmts): // exec stmt[op-1]
			if _, err := w.wh.Execute(w.stmts[w.op-1], nil); err != nil {
				_ = w.wh.Rollback()
				return fmt.Errorf("writer exec %q: %w", w.stmts[w.op-1], err)
			}
			w.op++
		default: // commit
			if err := w.wh.Commit(); err != nil {
				return fmt.Errorf("writer commit: %w", err)
			}
			w.wh = nil
			*gateHolder = -1
			w.op = 0
			w.iter++
		}
	case "reader":
		switch w.op {
		case 0: // open a snapshot
			w.rh = db.ReadSession()
			w.op++
		case 1: // assert the invariant
			got, err := queryScalar(w.rh, w.query)
			if err != nil {
				w.rh.Close()
				return err
			}
			*checks++
			if got != w.expect {
				w.rh.Close()
				return fmt.Errorf("invariant %q: got %s, want %s", w.query, got, w.expect)
			}
			w.op++
		default: // close (advance the watermark)
			w.rh.Close()
			w.rh = nil
			w.op = 0
			w.iter++
		}
	}
	return nil
}

// --- driving one file ------------------------------------------------------------------------

// lang and canThread describe this binary's core. Go can run real threads, so its native mode is
// threaded; a single-thread core (TS) sets canThread=false and runs the seeded interleaver.
const (
	lang      = "go"
	canThread = true
)

// runFile runs one stress file and returns its result line. forceSequential makes a threading core
// run the deterministic interleaver instead (for a cross-check / debugging without `-race`).
func runFile(path string, forceSequential bool) result {
	start := time.Now()
	res := result{Schema: 1, Lang: lang}
	var f stressFile
	if _, err := toml.DecodeFile(path, &f); err != nil {
		res.Status, res.Error = "fail", fmt.Sprintf("parse %s: %v", filepath.Base(path), err)
		return res
	}
	res.Name = f.Meta.Name
	if f.Final != nil {
		res.CrossCore = f.Final.CrossCoreChecksum
	}
	for _, wk := range f.Worker {
		if wk.Kind == "writer" {
			res.Writers += wk.Count
			res.WriterIters += wk.Count * wk.Iterations
		}
	}

	sequential := forceSequential || !canThread
	// A `parallel = "required"` file cannot run in the sequential interleaver — skip it there.
	if sequential && f.Meta.Parallel == "required" {
		res.Mode, res.Status = "skipped", "skip"
		return res
	}
	res.Mode = map[bool]string{true: "sequential", false: "threaded"}[sequential]

	db := memDB()
	if err := setup(db, &f); err != nil {
		res.Status, res.Error = "fail", err.Error()
		return res
	}

	var checks int64
	var err error
	if sequential {
		checks, err = runSequential(db, &f)
	} else {
		checks, err = runThreaded(db, &f)
	}
	res.InvariantChecks = checks
	res.DurationMs = time.Since(start).Milliseconds()
	if err != nil {
		res.Status, res.Error = "fail", err.Error()
		return res
	}

	checksum, finalOK, ferr := checkFinal(db, f.Final)
	if ferr != nil {
		res.Status, res.Error = "fail", ferr.Error()
		return res
	}
	res.Checksum, res.FinalOK = checksum, finalOK
	if !finalOK {
		res.Status, res.Error = "fail", "final state did not match [final].expect"
		return res
	}
	res.Status = "pass"
	return res
}

func main() {
	args := os.Args[1:]
	forceSequential := false
	var positional []string
	for _, a := range args {
		switch a {
		case "--sequential":
			forceSequential = true
		default:
			positional = append(positional, a)
		}
	}
	if len(positional) < 2 {
		fmt.Fprintln(os.Stderr, "usage: stress <stress_dir> <out_path> [name_filter] [--sequential]")
		os.Exit(2)
	}
	stressDir, outPath := positional[0], positional[1]
	filter := ""
	if len(positional) > 2 {
		filter = positional[2]
	}

	entries, err := os.ReadDir(stressDir)
	if err != nil {
		fmt.Fprintf(os.Stderr, "read %s: %v\n", stressDir, err)
		os.Exit(1)
	}
	var files []string
	for _, e := range entries {
		if !e.IsDir() && strings.HasSuffix(e.Name(), ".stress.toml") {
			if filter == "" || strings.Contains(e.Name(), filter) {
				files = append(files, filepath.Join(stressDir, e.Name()))
			}
		}
	}
	sort.Strings(files)

	out, err := os.Create(outPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "create %s: %v\n", outPath, err)
		os.Exit(1)
	}
	defer out.Close()
	enc := json.NewEncoder(out)

	exit := 0
	for _, file := range files {
		res := runFile(file, forceSequential)
		if err := enc.Encode(res); err != nil {
			fmt.Fprintf(os.Stderr, "write result: %v\n", err)
			os.Exit(1)
		}
		switch res.Status {
		case "pass":
			fmt.Fprintf(os.Stderr, "  PASS  %-36s %-10s checks=%d checksum=%s (%dms)\n", res.Name, res.Mode, res.InvariantChecks, res.Checksum, res.DurationMs)
		case "skip":
			fmt.Fprintf(os.Stderr, "  SKIP  %-36s %s\n", res.Name, res.Mode)
		default:
			exit = 1
			fmt.Fprintf(os.Stderr, "  FAIL  %-36s %s: %s\n", res.Name, res.Mode, res.Error)
		}
	}
	os.Exit(exit)
}
