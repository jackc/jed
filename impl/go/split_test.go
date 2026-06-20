package jed

// SplitStatements boundary correctness (spec/design/session.md §4.1): a `;` inside a string
// literal, dollar-quoted string, or line/block comment is not a statement boundary; empty spans are
// skipped. Per-core unit tested — the splitter adds no SQL semantics, so it is not in the shared
// corpus (CLAUDE.md §10). Mirrors impl/rust/src/split.rs tests.

import (
	"reflect"
	"testing"
)

func splitTexts(sql string) []string {
	var out []string
	for s := range SplitStatements(sql) {
		out = append(out, s.Text)
	}
	return out
}

func splitPairs(sql string) []StatementSpan {
	var out []StatementSpan
	for s := range SplitStatements(sql) {
		out = append(out, s)
	}
	return out
}

func TestSplitBasicAndOffsets(t *testing.T) {
	got := splitPairs("SELECT 1; SELECT 2")
	want := []StatementSpan{{Text: "SELECT 1", Offset: 0}, {Text: "SELECT 2", Offset: 10}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %+v, want %+v", got, want)
	}
}

func TestSplitEmptySpansSkipped(t *testing.T) {
	cases := map[string][]string{
		"SELECT 1;":           {"SELECT 1"},
		";;; SELECT 1 ;;;":    {"SELECT 1"},
		"":                    nil,
		"   \n\t  ":           nil,
		";":                   nil,
		"-- just a comment\n": nil,
		"/* block only */":    nil,
	}
	for sql, want := range cases {
		if got := splitTexts(sql); !reflect.DeepEqual(got, want) {
			t.Fatalf("%q: got %v, want %v", sql, got, want)
		}
	}
}

func TestSplitSemicolonInStringIsNotABoundary(t *testing.T) {
	if got := splitTexts("INSERT INTO t VALUES ('a;b'); SELECT 1"); !reflect.DeepEqual(
		got, []string{"INSERT INTO t VALUES ('a;b')", "SELECT 1"},
	) {
		t.Fatalf("got %v", got)
	}
	if got := splitTexts("SELECT 'it''s; ok'"); !reflect.DeepEqual(got, []string{"SELECT 'it''s; ok'"}) {
		t.Fatalf("got %v", got)
	}
}

func TestSplitSemicolonInCommentIsNotABoundary(t *testing.T) {
	if got := splitTexts("SELECT 1 -- a; b\n; SELECT 2"); !reflect.DeepEqual(
		got, []string{"SELECT 1", "SELECT 2"},
	) {
		t.Fatalf("line comment: got %v", got)
	}
	if got := splitTexts("SELECT /* a; b */ 1; SELECT 2"); !reflect.DeepEqual(
		got, []string{"SELECT /* a; b */ 1", "SELECT 2"},
	) {
		t.Fatalf("block comment: got %v", got)
	}
	if got := splitTexts("SELECT /* /* ; */ */ 1"); !reflect.DeepEqual(
		got, []string{"SELECT /* /* ; */ */ 1"},
	) {
		t.Fatalf("nested block comment: got %v", got)
	}
}

func TestSplitDollarQuoteSemicolonIsNotABoundary(t *testing.T) {
	if got := splitTexts("SELECT $$a;b$$; SELECT 2"); !reflect.DeepEqual(
		got, []string{"SELECT $$a;b$$", "SELECT 2"},
	) {
		t.Fatalf("empty tag: got %v", got)
	}
	if got := splitTexts("SELECT $tag$a;$$;b$tag$; SELECT 2"); !reflect.DeepEqual(
		got, []string{"SELECT $tag$a;$$;b$tag$", "SELECT 2"},
	) {
		t.Fatalf("named tag: got %v", got)
	}
	// `$1` is a bind parameter, not a dollar-quote — the `;` after it splits.
	if got := splitTexts("SELECT $1; SELECT 2"); !reflect.DeepEqual(got, []string{"SELECT $1", "SELECT 2"}) {
		t.Fatalf("bind param: got %v", got)
	}
}

func TestSplitTrailingWhitespaceAndInteriorComment(t *testing.T) {
	got := splitPairs("  SELECT 1  ;  SELECT /* x */ 2  ")
	if got[0] != (StatementSpan{Text: "SELECT 1", Offset: 2}) {
		t.Fatalf("first: got %+v", got[0])
	}
	if got[1] != (StatementSpan{Text: "SELECT /* x */ 2", Offset: 15}) {
		t.Fatalf("second: got %+v", got[1])
	}
}

func TestSplitNoTrailingSemicolonStillYieldsLast(t *testing.T) {
	if got := splitTexts("SELECT 1; SELECT 2; SELECT 3"); !reflect.DeepEqual(
		got, []string{"SELECT 1", "SELECT 2", "SELECT 3"},
	) {
		t.Fatalf("got %v", got)
	}
}
