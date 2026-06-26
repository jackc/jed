// The runner: turn one Mutation into a verdict.
//
// Each mutant is applied in an isolated WORKSPACE — a temp copy of impl/go plus a symlink
// to the real spec/ — so the live working tree is never touched and workers do not collide.
// A mutant is applied by overwriting one file, rebuilding the conformance binary, and running
// the corpus. The corpus's own exit code is the oracle (CLAUDE.md §7): exit 0 means every test
// still passed (the mutant SURVIVED — untested logic); non-zero means a test caught the bug
// (KILLED). A mutant that will not compile is INVALID (stillborn, excluded from the score); one
// that hangs is a TIMEOUT (counted as killed — the corpus's timeout caught it).
package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

type Status string

const (
	StatusKilled   Status = "killed"
	StatusSurvived Status = "survived"
	StatusInvalid  Status = "invalid" // did not compile — not scored
	StatusTimeout  Status = "timeout" // hung past the deadline — counted as killed
	StatusError    Status = "error"   // harness failure (e.g. cannot launch go) — aborts the run
)

// Result is the verdict for one mutant.
type Result struct {
	Mut         Mutation
	Status      Status
	Oracle      string // what caught a KILLED mutant: "corpus", "unit", or "timeout"
	KillingTest string // first failing corpus file, for a corpus-KILLED mutant
	Detail      string // truncated build/run output (INVALID/ERROR diagnostics)
	Elapsed     time.Duration
}

// Workspace is one isolated copy of impl/go a worker mutates in place.
type Workspace struct {
	root  string // temp dir: <root>/spec -> real spec, <root>/impl/go = copy
	goDir string // <root>/impl/go
	bin   string // built conformance binary path
}

// newWorkspace builds an isolated workspace: copy the Go module, symlink the real spec/ so the
// conformance harness's walk-up finds the corpus, collation, and tz fixtures unchanged.
func newWorkspace(realGoDir, realSpec string) (*Workspace, error) {
	root, err := os.MkdirTemp("", "jed-mutate-")
	if err != nil {
		return nil, err
	}
	goDir := filepath.Join(root, "impl", "go")
	if err := os.MkdirAll(filepath.Dir(goDir), 0o755); err != nil {
		return nil, err
	}
	if err := copyTree(realGoDir, goDir); err != nil {
		return nil, fmt.Errorf("copy module: %w", err)
	}
	if err := os.Symlink(realSpec, filepath.Join(root, "spec")); err != nil {
		return nil, fmt.Errorf("symlink spec: %w", err)
	}
	return &Workspace{root: root, goDir: goDir, bin: filepath.Join(goDir, "mutate_conf_bin")}, nil
}

func (w *Workspace) cleanup() { _ = os.RemoveAll(w.root) }

// fileIn returns the absolute path of a target file inside this workspace.
func (w *Workspace) fileIn(relFile string) string { return filepath.Join(w.goDir, relFile) }

// run applies a single mutant, builds, runs the oracle(s), and restores the file to pristine.
// originals maps relFile -> pristine bytes (shared, read-only). unitRegex, when non-empty, adds
// the per-core unit subset `go test ./ -run <regex>` as a second kill oracle, consulted only when
// the corpus survives — so byte-level logic the corpus deliberately does not pin (key encoding,
// catalog bytes) is still scored against the fixture/unit layer that DOES pin it (CLAUDE.md §10).
func (w *Workspace) run(m Mutation, originals map[string][]byte, timeout time.Duration, unitRegex string) Result {
	start := time.Now()
	res := Result{Mut: m}

	orig := originals[m.RelFile]
	target := w.fileIn(m.RelFile)
	if err := os.WriteFile(target, m.apply(orig), 0o644); err != nil {
		res.Status, res.Detail = StatusError, err.Error()
		return res
	}
	// Always restore, so the workspace is pristine for the next (possibly different) file.
	defer func() { _ = os.WriteFile(target, orig, 0o644) }()

	// Build. A compile failure is a stillborn (INVALID) mutant, not a kill.
	if out, err := w.build(); err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) {
			res.Status, res.Detail = StatusInvalid, truncate(out, 600)
		} else {
			res.Status, res.Detail = StatusError, fmt.Sprintf("build: %v", err)
		}
		res.Elapsed = time.Since(start)
		return res
	}

	// Run the corpus under a deadline.
	out, status, killing := w.runCorpus(timeout)
	res.Status, res.KillingTest = status, killing
	switch status {
	case StatusKilled:
		res.Oracle = "corpus"
	case StatusTimeout:
		res.Oracle = "timeout"
	case StatusError:
		res.Detail = truncate(out, 600)
	case StatusSurvived:
		// Second oracle: the per-core unit subset. A unit-kill means the logic IS tested, just
		// not by the (cross-core) corpus — so it is not a corpus gap, but it is not a survivor.
		if unitRegex != "" {
			if uout, killedByUnit := w.runUnit(unitRegex, timeout); killedByUnit {
				res.Status, res.Oracle = StatusKilled, "unit"
				res.KillingTest = firstFailingTest(uout)
			}
		}
	}
	res.Elapsed = time.Since(start)
	return res
}

// runUnit runs the per-core unit subset `go test ./ -run <regex>` in the workspace and reports
// whether it failed (i.e. caught the mutant). A non-ExitError (e.g. a timeout) is treated as a
// kill too — the mutant made the unit suite misbehave.
func (w *Workspace) runUnit(regex string, timeout time.Duration) (out string, killed bool) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, goBinary(), "test", "./", "-run", regex, "-count=1")
	cmd.Dir = w.goDir
	var buf bytes.Buffer
	cmd.Stdout, cmd.Stderr = &buf, &buf
	err := cmd.Run()
	return buf.String(), err != nil
}

func (w *Workspace) build() (string, error) {
	cmd := exec.Command(goBinary(), "build", "-o", w.bin, "./cmd/conformance")
	cmd.Dir = w.goDir
	var buf bytes.Buffer
	cmd.Stdout, cmd.Stderr = &buf, &buf
	err := cmd.Run()
	return buf.String(), err
}

func (w *Workspace) runCorpus(timeout time.Duration) (out string, status Status, killing string) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, w.bin)
	cmd.Dir = w.goDir
	var buf bytes.Buffer
	cmd.Stdout, cmd.Stderr = &buf, &buf
	err := cmd.Run()
	text := buf.String()

	if ctx.Err() == context.DeadlineExceeded {
		return text, StatusTimeout, ""
	}
	if err == nil {
		return text, StatusSurvived, ""
	}
	var exit *exec.ExitError
	if errors.As(err, &exit) {
		return text, StatusKilled, firstFailing(text)
	}
	return text, StatusError, ""
}

// firstFailing extracts the first failing corpus file from the harness output, so a KILLED
// mutant reports *which* test caught it (the actionable half of a kill).
func firstFailing(out string) string {
	for _, line := range strings.Split(out, "\n") {
		if rest, ok := strings.CutPrefix(line, "FAIL "); ok {
			// "FAIL <rel>: <err>" or "FAIL <rel>"
			rel, _, _ := strings.Cut(rest, ":")
			return strings.TrimSpace(rel)
		}
	}
	return ""
}

// firstFailingTest extracts the first failing Go test from `go test` output (a `--- FAIL: Name`
// line), so a unit-killed mutant reports which test caught it.
func firstFailingTest(out string) string {
	for _, line := range strings.Split(out, "\n") {
		if rest, ok := strings.CutPrefix(strings.TrimSpace(line), "--- FAIL: "); ok {
			name, _, _ := strings.Cut(rest, " ")
			return strings.TrimSpace(name)
		}
	}
	return ""
}

// copyTree recursively copies regular files and directories from src to dst. Symlinks and the
// stale build artifact are skipped; impl/go has no symlinks of its own.
func copyTree(src, dst string) error {
	return filepath.Walk(src, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		out := filepath.Join(dst, rel)
		switch {
		case info.IsDir():
			return os.MkdirAll(out, 0o755)
		case info.Mode()&os.ModeSymlink != 0:
			return nil // skip symlinks
		case info.Name() == "mutate_conf_bin":
			return nil // skip a stale build artifact
		default:
			return copyFile(path, out, info.Mode().Perm())
		}
	})
}

func copyFile(src, dst string, perm os.FileMode) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(dst, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, perm)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

// goBinary locates the go toolchain: PATH first (the mise/rake environment puts it there),
// then GOROOT/bin/go as a fallback.
func goBinary() string {
	if p, err := exec.LookPath("go"); err == nil {
		return p
	}
	if root := runtime.GOROOT(); root != "" {
		if p := filepath.Join(root, "bin", "go"); fileExists(p) {
			return p
		}
	}
	return "go"
}

func fileExists(p string) bool {
	_, err := os.Stat(p)
	return err == nil
}

func truncate(s string, n int) string {
	s = strings.TrimSpace(s)
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
