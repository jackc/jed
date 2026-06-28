package jed

// SQL comments are lexer whitespace (spec/design/grammar.md §33): `--` line comments
// run to end of line (and ALWAYS start outside a string, even abutting a token —
// `1--2` is `1`); `/* */` block comments NEST per PG / the SQL standard; an
// unterminated block is 42601; comment openers inside a string literal are text.
//
// mustCreate / wantErr are shared helpers from create_table_test.go (same package).

import "testing"

func commentsSetup(t *testing.T) *Engine {
	t.Helper()
	db := NewEngine()
	mustCreate(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, s text)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, 10, '--x /*y*/')")
	return db
}

// oneValue runs a query expected to produce exactly one value; returns it rendered.
func oneValue(t *testing.T, db *Engine, sql string) string {
	t.Helper()
	out := mustCreate(t, db, sql)
	if out.Kind != OutcomeQuery || len(out.Rows) != 1 || len(out.Rows[0]) != 1 {
		t.Fatalf("Execute(%q): expected a 1x1 query result, got %+v", sql, out)
	}
	return out.Rows[0][0].Render()
}

func TestLineCommentsRunToEndOfLine(t *testing.T) {
	db := commentsSetup(t)
	// Trailing comment; the statement continues on the next line.
	if got := oneValue(t, db, "SELECT v -- trailing\nFROM t WHERE id = 1"); got != "10" {
		t.Errorf("trailing comment: got %q", got)
	}
	// Leading comment line.
	if got := oneValue(t, db, "-- leading\nSELECT v FROM t WHERE id = 1"); got != "10" {
		t.Errorf("leading comment: got %q", got)
	}
	// A comment at the very end of input (no newline) is fine.
	if got := oneValue(t, db, "SELECT v FROM t WHERE id = 1 -- done"); got != "10" {
		t.Errorf("comment at EOF: got %q", got)
	}
}

func TestTwoHyphensStartACommentEvenAbuttingAToken(t *testing.T) {
	db := commentsSetup(t)
	// `v--1` is `v` then a comment (PG) — NOT `v - (-1)`.
	if got := oneValue(t, db, "SELECT v--1\nFROM t WHERE id = 1"); got != "10" {
		t.Errorf("abutting comment: got %q", got)
	}
	// Separated operators still mean double negation.
	if got := oneValue(t, db, "SELECT v - -1 FROM t WHERE id = 1"); got != "11" {
		t.Errorf("v - -1: got %q", got)
	}
}

func TestBlockCommentsSeparateTokensAndNest(t *testing.T) {
	db := commentsSetup(t)
	// A block comment is a token separator.
	if got := oneValue(t, db, "SELECT/*c*/v/*c*/FROM t WHERE id = 1"); got != "10" {
		t.Errorf("separator: got %q", got)
	}
	// Blocks nest: the comment ends only when the depth returns to zero.
	if got := oneValue(t, db, "SELECT /* a /* b */ still comment */ v FROM t WHERE id = 1"); got != "10" {
		t.Errorf("nested: got %q", got)
	}
	// A quote inside a block comment is ordinary comment text.
	if got := oneValue(t, db, "SELECT /* it's fine */ v FROM t WHERE id = 1"); got != "10" {
		t.Errorf("quote in comment: got %q", got)
	}
}

func TestCommentOpenersInsideAStringAreText(t *testing.T) {
	db := commentsSetup(t)
	if got := oneValue(t, db, "SELECT s FROM t WHERE id = 1"); got != "--x /*y*/" {
		t.Errorf("string content: got %q", got)
	}
}

func TestUnterminatedBlockCommentIs42601(t *testing.T) {
	db := commentsSetup(t)
	for _, sql := range []string{
		"SELECT v FROM t /* unterminated",
		"SELECT v FROM t /* outer /* inner */ still open",
		"SELECT v FROM t /*/", // the close cannot overlap the open
	} {
		wantErr(t, db, sql, "42601")
	}
}

func TestStrayCloseIsNotCommentSyntax(t *testing.T) {
	db := commentsSetup(t)
	// `*/` with no opener lexes as `*` `/` and fails at parse.
	wantErr(t, db, "SELECT v */ 1 FROM t", "42601")
}

func TestCommentOnlyInputIsNoStatement(t *testing.T) {
	db := commentsSetup(t)
	for _, sql := range []string{"-- nothing here", "/* nothing here */", "  /* a */ -- b"} {
		wantErr(t, db, sql, "42601")
	}
}
