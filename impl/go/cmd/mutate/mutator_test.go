package main

import (
	"go/parser"
	"go/token"
	"strings"
	"testing"
)

// sample exercises every mutation operator exactly enough to pin its count and shape.
const sample = `package sample

func f(a, b int) bool {
	if a < b && a == 0 {
		x := 0
		x++
		_ = x
		return true
	}
	return false
}
`

func TestEnumerateCounts(t *testing.T) {
	all, _ := parseMutators("all")
	muts, err := enumerate("sample.go", []byte(sample), all)
	if err != nil {
		t.Fatalf("enumerate: %v", err)
	}

	got := map[string]int{}
	for _, m := range muts {
		got[m.Mutator]++
	}
	want := map[string]int{
		"comparison": 3, // `<` -> {<=, >=}; `==` -> {!=}
		"logic":      1, // `&&` -> `||`
		"bool":       2, // true->false, false->true
		"offbyone":   4, // two `0` literals, each -> {(0+1),(0-1)}
		"incdec":     1, // `x++` -> `x--`
		"condneg":    1, // negate the if-condition
		// arith: none in the sample
	}
	for k, w := range want {
		if got[k] != w {
			t.Errorf("mutator %q: got %d, want %d", k, got[k], w)
		}
	}
	if got["arith"] != 0 {
		t.Errorf("arith: got %d, want 0", got["arith"])
	}
}

// TestApplyCompiles verifies every mutant splices into valid (re-parseable) source at the right
// spot — the property the runner depends on to distinguish a stillborn INVALID from a real KILL.
func TestApplyCompiles(t *testing.T) {
	all, _ := parseMutators("all")
	src := []byte(sample)
	muts, err := enumerate("sample.go", src, all)
	if err != nil {
		t.Fatalf("enumerate: %v", err)
	}
	for _, m := range muts {
		out := m.apply(src)
		// The original text at the mutation site must be exactly what we recorded, and the
		// replacement must land where we said.
		if string(src[m.ByteOff:m.ByteEnd]) != m.Orig {
			t.Errorf("%s: recorded Orig %q != source %q", m.ID(), m.Orig, src[m.ByteOff:m.ByteEnd])
		}
		if !strings.HasPrefix(string(out[m.ByteOff:]), m.Repl) {
			t.Errorf("%s: replacement not at recorded offset", m.ID())
		}
		// All of these mutants are type-preserving / syntactically valid, so they re-parse.
		if _, err := parser.ParseFile(token.NewFileSet(), "m.go", out, parser.SkipObjectResolution); err != nil {
			t.Errorf("%s: mutant does not parse: %v", m.ID(), err)
		}
	}
}

func TestEnumerateDeterministic(t *testing.T) {
	all, _ := parseMutators("all")
	a, _ := enumerate("sample.go", []byte(sample), all)
	b, _ := enumerate("sample.go", []byte(sample), all)
	if len(a) != len(b) {
		t.Fatalf("non-deterministic length: %d vs %d", len(a), len(b))
	}
	for i := range a {
		if a[i].ID() != b[i].ID() || a[i].ByteOff != b[i].ByteOff || a[i].Repl != b[i].Repl {
			t.Fatalf("non-deterministic at %d: %q vs %q", i, a[i].ID(), b[i].ID())
		}
	}
}

func TestMutatorFilter(t *testing.T) {
	only, err := parseMutators("comparison")
	if err != nil {
		t.Fatal(err)
	}
	muts, _ := enumerate("sample.go", []byte(sample), only)
	for _, m := range muts {
		if m.Mutator != "comparison" {
			t.Errorf("filter leaked %q", m.Mutator)
		}
	}
	if len(muts) != 3 {
		t.Errorf("comparison-only: got %d, want 3", len(muts))
	}
	if _, err := parseMutators("bogus"); err == nil {
		t.Error("expected error for unknown mutator")
	}
}
