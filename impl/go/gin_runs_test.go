package jed

// GIN posting-run vectors (spec/encoding/gin_posting_runs.toml, spec/design/gin.md §4.5): the run
// codec, the build's greedy packing, and run maintenance over an in-memory index store must be
// byte-exact against the shared vectors — the run bytes are the GIN entry tree's records, a
// byte-level contract no corpus entry can observe (CLAUDE.md §8/§10).

import (
	"encoding/hex"
	"slices"
	"testing"
)

func ginRunHexes(t *testing.T, hs []string) [][]byte {
	t.Helper()
	out := make([][]byte, len(hs))
	for i, h := range hs {
		b, err := hex.DecodeString(h)
		if err != nil {
			t.Fatal(err)
		}
		out[i] = b
	}
	return out
}

func TestGinPostingRunVectors(t *testing.T) {
	path := specPath(t, "encoding/gin_posting_runs.toml")
	checked := 0
	for _, r := range readTomlTables(t, path, "run") {
		term := ginRunHexes(t, []string{r.str("term")})[0]
		keys := ginRunHexes(t, r.strs("keys"))
		run := ginEncodeRun(term, keys)
		if hex.EncodeToString(run) != r.str("run") {
			t.Fatalf("run %v: %x want %s", r.strs("keys"), run, r.str("run"))
		}
		got, err := ginDecodeRun(len(term), run)
		if err != nil || !slices.EqualFunc(got, keys, func(a, b []byte) bool { return string(a) == string(b) }) {
			t.Fatalf("decode %s: %x %v", r.str("run"), got, err)
		}
		checked++
	}
	for _, r := range readTomlTables(t, path, "pack") {
		term := ginRunHexes(t, []string{r.str("term")})[0]
		var got []string
		for _, run := range ginPackRuns(term, ginRunHexes(t, r.strs("keys"))) {
			got = append(got, hex.EncodeToString(run))
		}
		if !slices.Equal(got, r.strs("runs")) {
			t.Fatalf("pack: %v want %v", got, r.strs("runs"))
		}
		checked++
	}
	for _, r := range readTomlTables(t, path, "history") {
		term := ginRunHexes(t, []string{r.str("term")})[0]
		store := newTableStore(pageCap{c: 4080}, nil)
		for _, op := range r.strs("ops") {
			key := ginRunHexes(t, []string{op[1:]})[0]
			var err error
			if op[0] == '+' {
				_, err = ginInsert(store, term, key)
			} else {
				_, err = ginRemove(store, term, key)
			}
			if err != nil {
				t.Fatal(err)
			}
		}
		es, err := store.EntriesInKeyOrder()
		if err != nil {
			t.Fatal(err)
		}
		var got []string
		for _, e := range es {
			got = append(got, hex.EncodeToString(e.Key))
		}
		if !slices.Equal(got, r.strs("runs")) {
			t.Fatalf("history %s: %v want %v", r.str("name"), got, r.strs("runs"))
		}
		checked++
	}
	if checked < 10 {
		t.Fatalf("only %d vectors", checked)
	}
}
