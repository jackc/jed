package main

// The stepped-THREADED mode of the concurrency schedule runner (spec/design/concurrency-testing.md
// §4.3): run every `# format: concurrency` suite file with one goroutine per session, the schedule
// order enforced by a turn token. The point is `go test -race ./cmd/conformance` — real
// concurrent-path coverage of the Database implementation (the actual atomics, the writer gate, the
// live-reader registry under multiple goroutines) that the single-goroutine sequential walk in the
// binary cannot give. The asserted result is identical to the sequential mode (the schedule is
// timing-free, §2), so a divergence — or a race the detector flags — is a genuine concurrency bug.
//
// This is what pulls concurrency back inside the §2 differential net: the same shared corpus file
// the sequential runner verifies is re-run here against the real concurrent code paths.

import (
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	jed "github.com/jackc/jed/impl/go"
)

func TestConcurrencySchedulesThreaded(t *testing.T) {
	suites := suitesDir()
	var files []string
	_ = filepath.WalkDir(suites, func(path string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() && strings.HasSuffix(path, ".test") {
			files = append(files, path)
		}
		return nil
	})
	sort.Strings(files)

	supported := map[string]bool{}
	for _, c := range jed.SupportedCapabilities {
		supported[c] = true
	}

	ran := 0
	for _, file := range files {
		data, err := os.ReadFile(file)
		if err != nil {
			t.Fatalf("read %s: %v", file, err)
		}
		text := string(data)
		if !isConcurrencyFormat(text) {
			continue
		}
		// Honor the same capability gate as the binary — skip a file needing a cap this core lacks.
		missing := false
		for _, c := range parseRequires(text) {
			if !supported[c] {
				missing = true
				break
			}
		}
		if missing {
			continue
		}
		steps, err := parseSchedule(text)
		if err != nil {
			t.Fatalf("parse %s: %v", filepath.Base(file), err)
		}
		if err := runScheduleThreaded(steps); err != nil {
			t.Fatalf("threaded %s: %v", filepath.Base(file), err)
		}
		ran++
	}
	if ran == 0 {
		t.Fatal("no runnable concurrency files found")
	}
}

// TestThreadedTeardownWithBlockedWriter covers the Layer 2 teardown path the suite `.test` files
// never reach (they always end every session): a schedule left with a live holder AND a queued
// (blocked) writer must tear down without hanging, reporting BOTH as still open. Tearing down the
// holder releases the gate, so the parked writer's Write() returns and its goroutine can be joined
// (§5). The timeout turns a teardown deadlock into a failure instead of a hung test run.
func TestThreadedTeardownWithBlockedWriter(t *testing.T) {
	steps := []cStep{
		{kind: "open", sid: "w1", mode: "write"},
		{kind: "open", sid: "w2", mode: "write", blocks: true},
	}
	done := make(chan error, 1)
	go func() { done <- runScheduleThreaded(steps) }()
	select {
	case err := <-done:
		if err == nil || !strings.Contains(err.Error(), "w1") || !strings.Contains(err.Error(), "w2") {
			t.Fatalf("want a leftover error naming w1 and w2, got %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("runScheduleThreaded hung tearing down a blocked writer")
	}
}
