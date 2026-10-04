// A standalone syscall experiment. It does not open or modify jed database files.
package main

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math/rand/v2"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"syscall"
	"time"
)

type flushKind string

const (
	dataFlush    flushKind = "fdatasync"
	fileFlush    flushKind = "fsync"
	fullFlush    flushKind = "fullsync"
	barrierFlush flushKind = "barrier"
)

var modes = []string{"fullsync-fullsync", "barrier-fullsync", "fullsync", "fsync", "fdatasync"}

type syncer struct {
	call      func(flushKind) error
	counts    map[flushKind]int
	elapsed   map[flushKind]time.Duration
	fallbacks int
}

func (s *syncer) flush(kind flushKind) error {
	start := time.Now()
	err := s.call(kind)
	s.counts[kind]++
	s.elapsed[kind] += time.Since(start)
	if kind == barrierFlush && (errors.Is(err, syscall.ENOTSUP) || errors.Is(err, syscall.EINVAL) || errors.Is(err, syscall.ENOSYS)) {
		s.fallbacks++
		return s.flush(fullFlush)
	}
	return err
}

// The two-phase modes differ only in the body barrier. Single-phase modes are
// timing controls: fullsync has durable completion, but none can preserve the
// existing COW root-publication protocol if power fails before it returns.
func commit(mode string, writeBody, writeMeta func() error, flush func(flushKind) error) error {
	if err := writeBody(); err != nil {
		return err
	}
	switch mode {
	case "fullsync-fullsync":
		if err := flush(fullFlush); err != nil {
			return err
		}
	case "barrier-fullsync":
		if err := flush(barrierFlush); err != nil {
			return err
		}
	}
	if err := writeMeta(); err != nil {
		return err
	}
	switch mode {
	case "fdatasync":
		return flush(dataFlush)
	case "fsync":
		return flush(fileFlush)
	default:
		return flush(fullFlush)
	}
}

type result struct {
	OS               string                `json:"os"`
	Arch             string                `json:"arch"`
	GoVersion        string                `json:"go_version"`
	Mode             string                `json:"mode"`
	Semantics        string                `json:"semantics"`
	Directory        string                `json:"directory"`
	Iterations       int                   `json:"iterations"`
	Warmup           int                   `json:"warmup"`
	BodyPages        int                   `json:"body_pages"`
	Seed             uint64                `json:"seed"`
	BytesWritten     int64                 `json:"bytes_written"`
	MeanMicroseconds float64               `json:"mean_us"`
	P50Microseconds  float64               `json:"p50_us"`
	P95Microseconds  float64               `json:"p95_us"`
	P99Microseconds  float64               `json:"p99_us"`
	Calls            map[flushKind]int     `json:"sync_calls"`
	CallMicroseconds map[flushKind]float64 `json:"sync_total_us"`
	BarrierFallbacks int                   `json:"barrier_fallbacks"`
}

func semantics(mode string) string {
	switch mode {
	case "fullsync-fullsync":
		return "two durable barriers; current COW protocol baseline"
	case "barrier-fullsync":
		return "ordered body before meta, durable completion; unsupported barrier falls back to fullsync"
	case "fullsync":
		return "UNSAFE COW timing control: durable completion, no body-before-meta crash ordering"
	default:
		return "UNSAFE timing control: no guaranteed drive-cache durability or crash ordering"
	}
}

func run(args []string, out io.Writer) error {
	flags := flag.NewFlagSet("macos-sync-probe", flag.ContinueOnError)
	flags.SetOutput(out)
	dir := flags.String("dir", ".", "existing directory on the filesystem to measure (files created with exclusive temporary names and removed)")
	iterations := flags.Int("iterations", 200, "measured commits per mode")
	warmup := flags.Int("warmup", 20, "untimed commits per mode")
	pages := flags.Int("body-pages", 1, "4096-byte body pages per commit (1..256); every mode also writes one 4096-byte meta page")
	seed := flags.Uint64("seed", 1, "reproducible page-content seed")
	mode := flags.String("mode", "all", "all, or a comma-separated list: "+strings.Join(modes, ","))
	flags.Usage = func() {
		fmt.Fprintln(out, "macOS-only preallocated I/O experiment; JSONL output. Not a database or a power-cut test.")
		fmt.Fprintln(out, "fullsync-fullsync: body -> F_FULLFSYNC -> meta -> F_FULLFSYNC (baseline).")
		fmt.Fprintln(out, "barrier-fullsync: body -> F_BARRIERFSYNC -> meta -> F_FULLFSYNC (candidate).")
		fmt.Fprintln(out, "fullsync: body -> meta -> F_FULLFSYNC; UNSAFE COW ordering timing control.")
		fmt.Fprintln(out, "fsync/fdatasync: body -> meta -> selected call; UNSAFE durability/ordering timing controls.")
		fmt.Fprintln(out, "Barrier unsupported errors fall back to F_FULLFSYNC; final fullsync never falls back to weaker calls.")
		flags.PrintDefaults()
	}
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || *iterations < 1 || *iterations > 1_000_000 || *warmup < 0 || *warmup > 1_000_000 || *pages < 1 || *pages > 256 {
		return errors.New("invalid arguments: iterations 1..1000000, warmup 0..1000000, body-pages 1..256, no positional arguments")
	}
	selected := strings.Split(*mode, ",")
	if *mode == "all" {
		selected = modes
	}
	for _, name := range selected {
		if !slices.Contains(modes, name) {
			return fmt.Errorf("unknown mode %q", name)
		}
	}
	if !supportedPlatform {
		return errors.New("this probe only runs on macOS; cross-compilation does not validate macOS behavior")
	}
	abs, err := filepath.Abs(*dir)
	if err != nil {
		return err
	}
	for _, name := range selected {
		row, err := measure(abs, name, *iterations, *warmup, *pages, *seed)
		if err != nil {
			return fmt.Errorf("%s: %w", name, err)
		}
		if err := json.NewEncoder(out).Encode(row); err != nil {
			return err
		}
	}
	return nil
}

func writeAt(file *os.File, bytes []byte, offset int64) error {
	n, err := file.WriteAt(bytes, offset)
	if err == nil && n != len(bytes) {
		err = io.ErrShortWrite
	}
	return err
}

func measure(dir, mode string, iterations, warmup, pages int, seed uint64) (result, error) {
	row := result{OS: runtime.GOOS, Arch: runtime.GOARCH, GoVersion: runtime.Version(), Mode: mode,
		Semantics: semantics(mode), Directory: dir, Iterations: iterations, Warmup: warmup, BodyPages: pages, Seed: seed}
	file, err := os.CreateTemp(dir, ".jed-sync-probe-*")
	if err != nil {
		return row, err
	}
	defer os.Remove(file.Name())
	defer file.Close()
	// Real writes, not sparse truncate: reserve eight MiB ahead of timing. This
	// tests in-region overwrites, the same condition as jed's steady-state commit.
	zeros := make([]byte, 1<<20)
	for offset := int64(0); offset < 8<<20; offset += int64(len(zeros)) {
		if err := writeAt(file, zeros, offset); err != nil {
			return row, err
		}
	}
	if err := platformFlush(file, fullFlush); err != nil {
		return row, fmt.Errorf("durable preallocation: %w", err)
	}
	rng := rand.New(rand.NewPCG(seed, seed^0x9e3779b97f4a7c15))
	body, meta := make([]byte, pages*4096), make([]byte, 4096)
	for _, bytes := range [][]byte{body, meta} {
		for i := 0; i < len(bytes); i += 8 {
			binary.LittleEndian.PutUint64(bytes[i:], rng.Uint64())
		}
	}
	s := &syncer{call: func(kind flushKind) error { return platformFlush(file, kind) },
		counts: make(map[flushKind]int), elapsed: make(map[flushKind]time.Duration)}
	times := make([]time.Duration, 0, iterations)
	var total time.Duration
	for n := 0; n < warmup+iterations; n++ {
		if n == warmup {
			s.counts = make(map[flushKind]int)
			s.elapsed = make(map[flushKind]time.Duration)
			s.fallbacks = 0
		}
		binary.LittleEndian.PutUint64(body, uint64(n))
		binary.LittleEndian.PutUint64(meta, uint64(n))
		// Alternate two disjoint body extents and two meta pages, keeping the
		// preallocated file size fixed. No claim of real recovery is made here.
		bodyOffset := int64(8192 + (n%2)*len(body))
		metaOffset := int64((n % 2) * len(meta))
		start := time.Now()
		err := commit(mode, func() error { return writeAt(file, body, bodyOffset) },
			func() error { return writeAt(file, meta, metaOffset) }, s.flush)
		elapsed := time.Since(start)
		if err != nil {
			return row, err
		}
		if n >= warmup {
			times = append(times, elapsed)
			total += elapsed
		}
	}
	// Check the final bytes via a fresh descriptor. This verifies I/O/content,
	// not persistence across a power failure (the OS cache remains available).
	reader, err := os.Open(file.Name())
	if err != nil {
		return row, err
	}
	defer reader.Close()
	last := warmup + iterations - 1
	for _, entry := range []struct {
		offset int64
		want   []byte
	}{
		{int64(8192 + (last%2)*len(body)), body}, {int64((last % 2) * len(meta)), meta},
	} {
		got := make([]byte, len(entry.want))
		if _, err := reader.ReadAt(got, entry.offset); err != nil {
			return row, err
		}
		if !slices.Equal(got, entry.want) {
			return row, errors.New("fresh-descriptor content mismatch")
		}
	}
	slices.Sort(times)
	percentile := func(p int) float64 { return float64(times[(len(times)*p+99)/100-1]) / float64(time.Microsecond) }
	row.MeanMicroseconds = float64(total) / float64(iterations) / float64(time.Microsecond)
	row.P50Microseconds, row.P95Microseconds, row.P99Microseconds = percentile(50), percentile(95), percentile(99)
	row.BytesWritten = int64(iterations) * int64(len(body)+len(meta))
	row.Calls, row.BarrierFallbacks = s.counts, s.fallbacks
	row.CallMicroseconds = make(map[flushKind]float64)
	for kind, elapsed := range s.elapsed {
		row.CallMicroseconds[kind] = float64(elapsed) / float64(time.Microsecond)
	}
	return row, nil
}

func main() {
	if err := run(os.Args[1:], os.Stdout); err != nil && !errors.Is(err, flag.ErrHelp) {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
