package main

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestTwoPhaseCommitOrdering(t *testing.T) {
	for _, mode := range []string{"fullsync-fullsync", "barrier-fullsync"} {
		t.Run(mode, func(t *testing.T) {
			var events []string
			err := commit(mode,
				func() error { events = append(events, "body"); return nil },
				func() error { events = append(events, "meta"); return nil },
				func(kind flushKind) error { events = append(events, string(kind)); return nil })
			first := "fullsync"
			if mode == "barrier-fullsync" {
				first = "barrier"
			}
			if err != nil || !slices.Equal(events, []string{"body", first, "meta", "fullsync"}) {
				t.Fatalf("events=%v, error=%v", events, err)
			}
		})
	}
}

func TestUnsupportedBarrierUsesFullsync(t *testing.T) {
	for _, unsupported := range []error{syscall.ENOTSUP, syscall.EINVAL, syscall.ENOSYS} {
		var events []string
		s := &syncer{counts: make(map[flushKind]int), elapsed: make(map[flushKind]time.Duration),
			call: func(kind flushKind) error {
				events = append(events, string(kind))
				if kind == barrierFlush {
					return unsupported
				}
				return nil
			}}
		err := commit("barrier-fullsync",
			func() error { events = append(events, "body"); return nil },
			func() error { events = append(events, "meta"); return nil }, s.flush)
		if err != nil || !slices.Equal(events, []string{"body", "barrier", "fullsync", "meta", "fullsync"}) || s.fallbacks != 1 || s.counts[fullFlush] != 2 {
			t.Fatalf("events=%v fallbacks=%d counts=%v error=%v", events, s.fallbacks, s.counts, err)
		}
	}
}

func TestBarrierIOFailureDoesNotPublishMeta(t *testing.T) {
	var published bool
	s := &syncer{counts: make(map[flushKind]int), elapsed: make(map[flushKind]time.Duration),
		call: func(flushKind) error { return syscall.EIO }}
	err := commit("barrier-fullsync", func() error { return nil },
		func() error { published = true; return nil }, s.flush)
	if !errors.Is(err, syscall.EIO) || published || s.fallbacks != 0 {
		t.Fatalf("error=%v meta=%v fallbacks=%d", err, published, s.fallbacks)
	}
}

func TestFinalFullsyncNeverWeakensOrHidesFailure(t *testing.T) {
	s := &syncer{counts: make(map[flushKind]int), elapsed: make(map[flushKind]time.Duration),
		call: func(kind flushKind) error {
			if kind == fullFlush {
				return syscall.ENOTSUP
			}
			return nil
		}}
	err := commit("barrier-fullsync", func() error { return nil }, func() error { return nil }, s.flush)
	if !errors.Is(err, syscall.ENOTSUP) || s.fallbacks != 0 || s.counts[fileFlush] != 0 || s.counts[dataFlush] != 0 {
		t.Fatalf("error=%v counts=%v fallbacks=%d", err, s.counts, s.fallbacks)
	}
}

func TestNonDarwinRejectedBeforeFilesystemChanges(t *testing.T) {
	if supportedPlatform {
		t.Skip("negative platform test")
	}
	path := filepath.Join(t.TempDir(), "must-not-be-created")
	err := run([]string{"--dir", path, "--iterations", "1"}, &bytes.Buffer{})
	if err == nil || !strings.Contains(err.Error(), "only runs on macOS") {
		t.Fatalf("error=%v", err)
	}
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("stat=%v", err)
	}
}
