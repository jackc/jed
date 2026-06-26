// Command mutate is the Go core's mutation-testing harness (spec/design/mutation-testing.md).
//
// It injects deliberate bugs into the Go core's source — a flipped comparison, an off-by-one
// boundary, a dropped guard, a swapped connective — and checks whether the conformance corpus
// (CLAUDE.md §7) catches each one. A mutant the corpus still passes is a SURVIVOR: a hole in
// the tests, located precisely. The mutation score (killed / (killed+survived)) quantifies how
// much of the targeted logic the corpus actually exercises, answering "are we only testing the
// obvious?" with a map instead of a guess (.scratch/testing-ideas.md §1.2).
//
// This deliberately lives OUTSIDE `rake ci`, like benchmarks and stress: it is a slow analysis
// tool, not a merge gate. It is reproducible — enumeration is deterministic and sampling is
// seeded — so `rake mutation` yields the same mutant set every run.
//
// Usage (from anywhere in the repo, or via `rake mutation`):
//
//	go run ./cmd/mutate [flags]
//	  -files     comma list of target files, relative to impl/go (default: the core logic files)
//	  -mutators  comma list of operators, or "all" (default: all)
//	  -n         max mutants to run, 0 = all (default 300)
//	  -seed      sample seed for reproducible selection (default 1)
//	  -workers   parallel workspaces (default ~NumCPU/2, capped)
//	  -timeout   per-mutant seconds, 0 = auto from baseline (default 0)
//	  -list      only enumerate + print mutation points, do not build/run
//	  -json      write a JSONL result line per mutant to this path
//	  -v         stream each mutant verdict as it completes
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// defaultTargets are the "executor / evaluator / comparator" files the design brief names as the
// mutation target (.scratch/testing-ideas.md §5): the executor, the operator/expression evaluator,
// the value layer + comparator, the exact-decimal arithmetic + rounding, and key encoding.
var defaultTargets = []string{
	"executor.go",
	"operators.go",
	"value.go",
	"decimal.go",
	"encoding.go",
}

func main() { os.Exit(run()) }

func run() int {
	files := flag.String("files", strings.Join(defaultTargets, ","), "target files (relative to impl/go), comma-separated")
	mutatorSpec := flag.String("mutators", "all", "mutation operators, comma-separated, or \"all\"")
	n := flag.Int("n", 300, "max mutants to run (0 = all)")
	seed := flag.Int64("seed", 1, "sample seed for reproducible mutant selection")
	workers := flag.Int("workers", defaultWorkers(), "parallel workspaces")
	timeoutSec := flag.Float64("timeout", 0, "per-mutant timeout in seconds (0 = auto from baseline)")
	list := flag.Bool("list", false, "only enumerate and print mutation points; do not build/run")
	jsonPath := flag.String("json", "", "write a JSONL result line per mutant to this path")
	verbose := flag.Bool("v", false, "stream each mutant verdict as it completes")
	repoFlag := flag.String("repo", "", "repo root (default: auto-detected by walking up)")
	flag.Parse()

	root := *repoFlag
	if root == "" {
		var err error
		if root, err = findRepoRoot(); err != nil {
			fmt.Fprintln(os.Stderr, "mutate:", err)
			return 2
		}
	}
	realGoDir := filepath.Join(root, "impl", "go")
	realSpec := filepath.Join(root, "spec")

	enabled, err := parseMutators(*mutatorSpec)
	if err != nil {
		fmt.Fprintln(os.Stderr, "mutate:", err)
		return 2
	}

	// Load pristine originals and enumerate every mutation point.
	originals := map[string][]byte{}
	var all []Mutation
	for _, rel := range splitList(*files) {
		src, err := os.ReadFile(filepath.Join(realGoDir, rel))
		if err != nil {
			fmt.Fprintf(os.Stderr, "mutate: read %s: %v\n", rel, err)
			return 2
		}
		originals[rel] = src
		muts, err := enumerate(rel, src, enabled)
		if err != nil {
			fmt.Fprintln(os.Stderr, "mutate:", err)
			return 2
		}
		all = append(all, muts...)
	}

	if len(all) == 0 {
		fmt.Fprintln(os.Stderr, "mutate: no mutation points found")
		return 2
	}

	// Seeded sample. Shuffle a copy, take the first n, then re-sort for stable reporting.
	selected := all
	if *n > 0 && *n < len(all) {
		selected = make([]Mutation, len(all))
		copy(selected, all)
		r := rand.New(rand.NewSource(*seed))
		r.Shuffle(len(selected), func(i, j int) { selected[i], selected[j] = selected[j], selected[i] })
		selected = selected[:*n]
		sort.SliceStable(selected, func(i, j int) bool {
			if selected[i].RelFile != selected[j].RelFile {
				return selected[i].RelFile < selected[j].RelFile
			}
			if selected[i].ByteOff != selected[j].ByteOff {
				return selected[i].ByteOff < selected[j].ByteOff
			}
			return selected[i].Desc < selected[j].Desc
		})
	}

	mutatorNames := make([]string, 0, len(enabled))
	for _, m := range allMutators {
		if enabled[m] {
			mutatorNames = append(mutatorNames, m)
		}
	}

	fmt.Printf("mutation testing — Go core\n")
	fmt.Printf("  target files: %s\n", strings.Join(splitList(*files), ", "))
	fmt.Printf("  mutators:     %s\n", strings.Join(mutatorNames, ", "))
	fmt.Printf("  enumerated %d mutation points", len(all))
	if len(selected) < len(all) {
		fmt.Printf("; sampling %d (seed %d)", len(selected), *seed)
	}
	fmt.Println()

	if *list {
		for _, m := range selected {
			fmt.Printf("  %s\n", m.ID())
		}
		return 0
	}

	// Baseline: a pristine workspace must produce a green corpus, or mutation testing is
	// meaningless. Reuse this workspace as worker 0.
	fmt.Printf("  building baseline workspace…\n")
	ws0, err := newWorkspace(realGoDir, realSpec)
	if err != nil {
		fmt.Fprintln(os.Stderr, "mutate:", err)
		return 2
	}
	defer ws0.cleanup()

	baseTimeout := 120 * time.Second
	bstart := time.Now()
	if out, err := ws0.build(); err != nil {
		fmt.Fprintf(os.Stderr, "mutate: baseline build failed:\n%s\n", truncate(out, 1500))
		return 2
	}
	_, status, _ := ws0.runCorpus(baseTimeout)
	baseElapsed := time.Since(bstart)
	if status != StatusSurvived {
		fmt.Fprintf(os.Stderr, "mutate: baseline corpus is not green (%s); cannot mutation-test a red suite\n", status)
		return 2
	}

	timeout := time.Duration(*timeoutSec * float64(time.Second))
	if timeout <= 0 {
		// Generous headroom over baseline so only a genuine hang trips the deadline.
		timeout = 15 * baseElapsed
		if timeout < 10*time.Second {
			timeout = 10 * time.Second
		}
	}
	fmt.Printf("  baseline green in %s; per-mutant timeout %s; workers %d\n\n",
		baseElapsed.Round(time.Millisecond), timeout.Round(time.Millisecond), *workers)

	results := runAll(selected, originals, realGoDir, realSpec, ws0, *workers, timeout, *verbose)
	return report(results, len(all), os.Stdout, *jsonPath)
}

// runAll dispatches the selected mutants across worker workspaces and returns results in input
// order. ws0 (the baseline workspace) is reused as worker 0's; the rest are built lazily.
func runAll(muts []Mutation, originals map[string][]byte, realGoDir, realSpec string, ws0 *Workspace, workers int, timeout time.Duration, verbose bool) []Result {
	if workers < 1 {
		workers = 1
	}
	results := make([]Result, len(muts))
	jobs := make(chan int)
	var done int64
	var wg sync.WaitGroup

	worker := func(ws *Workspace, own bool) {
		defer wg.Done()
		if own {
			defer ws.cleanup()
		}
		for i := range jobs {
			results[i] = ws.run(muts[i], originals, timeout)
			n := atomic.AddInt64(&done, 1)
			if verbose {
				r := results[i]
				extra := ""
				if r.KillingTest != "" {
					extra = "  (caught by " + r.KillingTest + ")"
				}
				fmt.Printf("  [%d/%d] %-9s %s%s\n", n, len(muts), r.Status, r.Mut.ID(), extra)
			} else {
				fmt.Printf("\r  running… %d/%d", n, len(muts))
			}
		}
	}

	wg.Add(1)
	go worker(ws0, false)
	for w := 1; w < workers; w++ {
		ws, err := newWorkspace(realGoDir, realSpec)
		if err != nil {
			fmt.Fprintf(os.Stderr, "\nmutate: worker %d setup failed: %v (continuing with fewer)\n", w, err)
			break
		}
		wg.Add(1)
		go worker(ws, true)
	}

	for i := range muts {
		jobs <- i
	}
	close(jobs)
	wg.Wait()
	if !verbose {
		fmt.Printf("\r%-40s\r", "") // clear the progress line
	}
	return results
}

// report prints the human summary, the actionable survivor list, and a per-file score table,
// and optionally writes a JSONL result file. Returns the process exit code: survivors or harness
// errors are a non-zero "you have work to do" signal; an all-killed run exits 0.
func report(results []Result, enumerated int, out *os.File, jsonPath string) int {
	var killed, survived, invalid, timeout, errored int
	perFile := map[string]*[2]int{} // [killed+timeout, survived]
	perMutator := map[string]*[2]int{}
	var survivors []Result
	var errors []Result

	for _, r := range results {
		pf := perFile[r.Mut.RelFile]
		if pf == nil {
			pf = &[2]int{}
			perFile[r.Mut.RelFile] = pf
		}
		pm := perMutator[r.Mut.Mutator]
		if pm == nil {
			pm = &[2]int{}
			perMutator[r.Mut.Mutator] = pm
		}
		switch r.Status {
		case StatusKilled:
			killed++
			pf[0]++
			pm[0]++
		case StatusTimeout:
			timeout++
			pf[0]++
			pm[0]++
		case StatusSurvived:
			survived++
			pf[1]++
			pm[1]++
			survivors = append(survivors, r)
		case StatusInvalid:
			invalid++
		case StatusError:
			errored++
			errors = append(errors, r)
		}
	}

	scored := killed + timeout + survived
	fmt.Fprintf(out, "results (%d mutants run, %d enumerated):\n", len(results), enumerated)
	fmt.Fprintf(out, "  killed    %4d\n", killed)
	if timeout > 0 {
		fmt.Fprintf(out, "  timeout   %4d  (counted as killed)\n", timeout)
	}
	fmt.Fprintf(out, "  survived  %4d\n", survived)
	fmt.Fprintf(out, "  invalid   %4d  (did not compile — excluded from score)\n", invalid)
	if errored > 0 {
		fmt.Fprintf(out, "  error     %4d  (harness failure)\n", errored)
	}
	fmt.Fprintf(out, "  -----------------\n")
	if scored > 0 {
		fmt.Fprintf(out, "  mutation score: %d/%d = %.1f%%   (killed / (killed+survived))\n",
			killed+timeout, scored, 100*float64(killed+timeout)/float64(scored))
	}

	if len(survivors) > 0 {
		fmt.Fprintf(out, "\nsurviving mutants (untested logic — the corpus did not catch these):\n")
		for _, r := range survivors {
			fmt.Fprintf(out, "  %s\n", r.Mut.ID())
		}
	}

	fmt.Fprintf(out, "\nper-file score:\n")
	for _, rel := range sortedKeys(perFile) {
		c := perFile[rel]
		fmt.Fprintf(out, "  %-16s %s\n", rel, scoreStr(c[0], c[1]))
	}
	fmt.Fprintf(out, "per-mutator score:\n")
	for _, name := range allMutators {
		c, ok := perMutator[name]
		if !ok {
			continue
		}
		fmt.Fprintf(out, "  %-16s %s\n", name, scoreStr(c[0], c[1]))
	}

	if len(errors) > 0 {
		fmt.Fprintf(out, "\nharness errors:\n")
		for _, r := range errors {
			fmt.Fprintf(out, "  %s\n    %s\n", r.Mut.ID(), r.Detail)
		}
	}

	if jsonPath != "" {
		if err := writeJSONL(jsonPath, results); err != nil {
			fmt.Fprintf(os.Stderr, "mutate: write json: %v\n", err)
		} else {
			fmt.Fprintf(out, "\nwrote %d result lines to %s\n", len(results), jsonPath)
		}
	}

	if errored > 0 {
		return 2
	}
	if survived > 0 {
		return 1
	}
	return 0
}

func scoreStr(killed, survived int) string {
	tot := killed + survived
	if tot == 0 {
		return "—"
	}
	return fmt.Sprintf("%d/%d  %.1f%%", killed, tot, 100*float64(killed)/float64(tot))
}

func writeJSONL(path string, results []Result) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	enc := json.NewEncoder(f)
	for _, r := range results {
		row := map[string]any{
			"file":    r.Mut.RelFile,
			"line":    r.Mut.Line,
			"col":     r.Mut.Col,
			"mutator": r.Mut.Mutator,
			"desc":    r.Mut.Desc,
			"status":  r.Status,
			"elapsed": r.Elapsed.Seconds(),
		}
		if r.KillingTest != "" {
			row["killing_test"] = r.KillingTest
		}
		if r.Detail != "" {
			row["detail"] = r.Detail
		}
		if err := enc.Encode(row); err != nil {
			return err
		}
	}
	return nil
}

// findRepoRoot walks up from cwd to the dir containing spec/conformance/suites — the same anchor
// the conformance harness uses, so the tool runs from anywhere in the tree.
func findRepoRoot() (string, error) {
	wd, err := os.Getwd()
	if err != nil {
		return "", err
	}
	dir := wd
	for {
		if fileExists(filepath.Join(dir, "spec", "conformance", "suites")) &&
			fileExists(filepath.Join(dir, "impl", "go", "go.mod")) {
			return dir, nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", fmt.Errorf("could not find repo root (spec/conformance/suites + impl/go) from %s", wd)
		}
		dir = parent
	}
}

func splitList(s string) []string {
	var out []string
	for _, p := range strings.Split(s, ",") {
		if p = strings.TrimSpace(p); p != "" {
			out = append(out, p)
		}
	}
	return out
}

func sortedKeys(m map[string]*[2]int) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func defaultWorkers() int {
	w := runtime.NumCPU() / 2
	if w < 2 {
		w = 2
	}
	if w > 8 {
		w = 8
	}
	return w
}
