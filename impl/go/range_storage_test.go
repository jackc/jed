package jed

// Range storage (spec/design/ranges.md, R2–R4) — the divergences + introspection the oracle corpus
// cannot express (CLAUDE.md §10): the deliberate 0A000 narrowings PostgreSQL does NOT share that
// remain after range became a key (a range DEFAULT and INSERT…SELECT into a range column — PG accepts
// the DEFAULT outright), the jed-canonical i32range spelling (PG reports int4range), the cross-element
// comparison code (jed's uniform 42804 where PG reports 42883), and the whole-image store/load
// round-trip of a range column (the byte layout is pinned cross-core by range_table.jed; this is the
// behavioral check). A range PRIMARY KEY / ordered index / UNIQUE / FK now WORK (range-PK slice, R4 —
// PG also allows them via its range btree opclass), so they live oracle-clean in types/range.test; the
// byte-exact key encoding is pinned by range_pk_table.jed + range_key_test.go (encoding.md §2.11). The
// agreeing behavior — render, canonicalization, IS NULL, the range_cmp total order (=/</ORDER BY/
// DISTINCT), 22000/22P02/22003/42704 — lives in types/range.test (oracle-clean), not here. Mirrors
// impl/rust/tests/range_storage.rs.

import (
	"reflect"
	"strings"
	"testing"
)

// errRange executes sql expecting an error and returns its SQLSTATE code.
func errRange(t *testing.T, db *Engine, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("%s: expected an error", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("%s: expected an *EngineError, got %T", sql, err)
	}
	return ee.Code()
}

// TestRangeImageRoundtrip: a range column survives a whole-image serialize + reload (ToImage →
// LoadEngine), exercising encodeRangeBody / readRangeBody (the empty range, infinite bounds, a NULL
// range, the canonical [) storage). The on-disk byte layout is pinned cross-core by range_table.jed;
// this is the behavioral round-trip.
func TestRangeImageRoundtrip(t *testing.T) {
	db := NewEngine()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)', '[10,20)')")
	run(t, db, "INSERT INTO t VALUES (2, '[1,5]', NULL)") // canonical [1,6)
	run(t, db, "INSERT INTO t VALUES (3, 'empty', '(,100)')")
	run(t, db, "INSERT INTO t VALUES (4, '(,)', '(5,)')") // canonical [6,)
	run(t, db, "INSERT INTO t VALUES (5, NULL, '[1,1]')") // canonical [1,2)

	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize image: %v", err)
	}
	loaded, err := LoadEngine(image)
	if err != nil {
		t.Fatalf("load image: %v", err)
	}
	got := queryRendered(t, loaded, "SELECT id, r, br FROM t ORDER BY id")
	want := [][]string{
		{"1", "[1,5)", "[10,20)"},
		{"2", "[1,6)", "NULL"},
		{"3", "empty", "(,100)"},
		{"4", "(,)", "[6,)"},
		{"5", "NULL", "[1,2)"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows differ\n  got:  %v\n  want: %v", got, want)
	}
}

// TestRangeCanonicalNameAndAliases: the jed-canonical name is i32range (PG reports int4range), and
// int4range/int8range are accepted as aliases (the i/f-prefix rename — CLAUDE.md §4). The PG alias
// declares a column whose stored value renders identically to the canonical spelling, and the
// canonical name (not the PG int4range) appears in a jed message.
func TestRangeCanonicalNameAndAliases(t *testing.T) {
	// The PG alias is accepted on the column; the value renders the same as the canonical spelling.
	db := NewEngine()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r int4range)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)')")
	got := queryRendered(t, db, "SELECT r FROM t")
	if want := [][]string{{"[1,5)"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("rows differ\n  got:  %v\n  want: %v", got, want)
	}

	// A range PRIMARY KEY now WORKS even when declared with the PG alias, and the value behaves as a
	// key (the stored row renders the canonical spelling). Range is keyable since R4 (encoding.md §2.11).
	db2 := NewEngine()
	run(t, db2, "CREATE TABLE k (r int4range PRIMARY KEY, n i32)")
	run(t, db2, "INSERT INTO k VALUES ('[1,5)', 1)")
	if got := errRange(t, db2, "INSERT INTO k VALUES ('[1,4]', 2)"); got != "23505" {
		// [1,4] canonicalizes to [1,5) — the same key, so a duplicate
		t.Errorf("canonical-collision duplicate key: got %s, want 23505", got)
	}

	// A still-rejected path reports the canonical i32range even when declared with the alias: GIN needs
	// an array/jsonb opclass, so GIN over a plain range column is 42704 and names the canonical element
	// type (PG agrees a range has no gin opclass but reports int4range — the naming divergence, so this
	// stays a per-core test).
	db3 := NewEngine()
	run(t, db3, "CREATE TABLE u (id i32 PRIMARY KEY, r int4range)")
	_, err := Execute(db3, "CREATE INDEX ON u USING gin (r)")
	if err == nil {
		t.Fatal("a gin index over a plain range column should be rejected")
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected an *EngineError, got %T", err)
	}
	if !strings.Contains(ee.Message, "i32range") {
		t.Errorf("message should name i32range: %q", ee.Message)
	}
}

// TestRangeNarrowingsAre0A000: the staged 0A000 narrowings PostgreSQL does NOT share that REMAIN after
// range became a key (R4): a range DEFAULT and INSERT…SELECT into a range column (PG accepts a range
// DEFAULT outright — spec/design/ranges.md §8). A range PRIMARY KEY / ordered index / UNIQUE now work
// (oracle-clean, types/range.test) — PG also allows them via its range btree opclass. These remaining
// cases are jed-stricter, so they cannot live in the oracle-clean corpus.
func TestRangeNarrowingsAre0A000(t *testing.T) {
	db := NewEngine()
	if got := errRange(t, db, "CREATE TABLE b (id i32 PRIMARY KEY, r i32range DEFAULT '[1,5)')"); got != "0A000" {
		t.Errorf("range DEFAULT: got %s, want 0A000", got)
	}
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)")
	// A range ordered (btree) index now WORKS (the range-bounds key, encoding.md §2.11) — a positive
	// check that the former 0A000 narrowing is lifted.
	run(t, db, "CREATE INDEX ri ON t (r)")
	// INSERT … SELECT into a range column is deferred (the VALUES + literal path is the input).
	run(t, db, "CREATE TABLE src (id i32 PRIMARY KEY, r i32range)")
	run(t, db, "INSERT INTO src VALUES (1, '[1,5)')")
	if got := errRange(t, db, "INSERT INTO t SELECT id, r FROM src"); got != "0A000" {
		t.Errorf("INSERT ... SELECT into range column: got %s, want 0A000", got)
	}
}

// TestRangeUpdateDeferrals: updating a range COLUMN works (ranges.md §4, oracle-clean in
// types/range.test) but three sub-cases stay 0A000 — PG supports them, so they are jed-stricter and
// cannot live in the oracle corpus: a $N parameter into a range column, the ON CONFLICT DO UPDATE
// conflict-action path, and a composite column (a separate slice). The happy-path forms (literal /
// cast / constructor / set-op / NULL / re-key) and the 42804 type errors live in types/range.test.
func TestRangeUpdateDeferrals(t *testing.T) {
	db := NewEngine()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)')")
	// A bound parameter into a range column is deferred (INSERT's param-to-container path is special).
	if got := errRange(t, db, "UPDATE t SET r = $1 WHERE id = 1"); got != "0A000" {
		t.Errorf("$N into range column: got %s, want 0A000", got)
	}
	// The ON CONFLICT DO UPDATE conflict-action path does not yet update a container column.
	if got := errRange(t, db, "INSERT INTO t VALUES (1, '[2,6)') ON CONFLICT (id) DO UPDATE SET r = '[9,10)'"); got != "0A000" {
		t.Errorf("ON CONFLICT DO UPDATE range: got %s, want 0A000", got)
	}
	// A composite column UPDATE is a separate slice (anonymous-record → named-composite coercion).
	run(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	run(t, db, "CREATE TABLE p (id i32 PRIMARY KEY, a addr)")
	run(t, db, "INSERT INTO p VALUES (1, ROW('x', 5))")
	if got := errRange(t, db, "UPDATE p SET a = ROW('y', 9) WHERE id = 1"); got != "0A000" {
		t.Errorf("composite column UPDATE: got %s, want 0A000", got)
	}
}

// TestRangeCrossElementComparisonIs42804: range comparison (R3) is restricted to the SAME element
// type (spec/design/ranges.md §6): a range is comparable only to a range over an equal element, never
// to a different-element range or to a bare scalar. jed reports its uniform comparison-mismatch code
// 42804; PostgreSQL reports 42883 ("operator does not exist") — a deliberate divergence, so this
// cannot live in the oracle corpus. The agreeing same-element comparison (=/</ORDER BY) is covered by
// types/range.test.
func TestRangeCrossElementComparisonIs42804(t *testing.T) {
	db := NewEngine()
	// A range over i32 vs a range over i64 — different element types, no implicit cross-range cast.
	if got := errRange(t, db, "SELECT '[1,5)'::i32range = '[1,5)'::i64range"); got != "42804" {
		t.Errorf("i32range = i64range: got %s, want 42804", got)
	}
	if got := errRange(t, db, "SELECT '[1,5)'::i32range < '[1,5)'::i64range"); got != "42804" {
		t.Errorf("i32range < i64range: got %s, want 42804", got)
	}
	// A range vs a bare scalar of its own element type is still a 42804 (a range is not its element).
	if got := errRange(t, db, "SELECT '[1,5)'::i32range = 5"); got != "42804" {
		t.Errorf("i32range = i32 scalar: got %s, want 42804", got)
	}
}

// TestRangeCompositeFieldIs0A000: a range-typed composite field is deferred (0A000) — only range
// *columns* are storable this slice. The type name IS known, so it is 0A000, not the 42704 an unknown
// type would give.
func TestRangeCompositeFieldIs0A000(t *testing.T) {
	db := NewEngine()
	if got := errRange(t, db, "CREATE TYPE rec AS (lo i32, span i32range)"); got != "0A000" {
		t.Errorf("range composite field: got %s, want 0A000", got)
	}
}

// TestRangeConstructorDivergences: the range CONSTRUCTORS (RF2) under jed's own spellings +
// assignment-style bound coercion — the two places jed diverges from PG's strict function-argument
// matching (spec/design/range-functions.md §2), which the oracle corpus (PG-clean) cannot express. The
// agreeing constructor behavior — default `[)`, explicit bounds, NULL→infinite, canonicalize/empty,
// 22000/42601/22003 — lives in expr/range_constructors.test. Mirrors range_constructor_divergences in
// impl/rust/tests/range_storage.rs.
func TestRangeConstructorDivergences(t *testing.T) {
	db := NewEngine()
	// (1) jed ACCEPTS the i/f-prefix spellings i32range/i64range as constructor names (PG ships only
	// int4range/int8range). The result is identical to the PG-spelled alias.
	if got := queryRendered(t, db, "SELECT i32range(1, 5)"); !reflect.DeepEqual(got, [][]string{{"[1,5)"}}) {
		t.Errorf("i32range(1, 5) = %v, want [[1,5)]", got)
	}
	if got := queryRendered(t, db, "SELECT i64range(100, 200, '[]')"); !reflect.DeepEqual(got, [][]string{{"[100,201)"}}) {
		t.Errorf("i64range(100, 200, '[]') = %v, want [[100,201)]", got)
	}
	// (2) jed accepts a WIDER integer for a narrower range and range-checks at eval — PG rejects the
	// int4range(bigint, …) overload outright (42883). A value that fits is built; one that overflows the
	// element domain is 22003 (the same assignment range-check INSERT applies).
	if got := queryRendered(t, db, "SELECT int4range(1::i64, 5::i64)"); !reflect.DeepEqual(got, [][]string{{"[1,5)"}}) {
		t.Errorf("int4range(1::i64, 5::i64) = %v, want [[1,5)]", got)
	}
	if got := errRange(t, db, "SELECT int4range(3000000000::i64, 4000000000::i64)"); got != "22003" {
		t.Errorf("int4range(overflow) = %s, want 22003", got)
	}
	// (3) Conversely jed is STRICTER on the unknown-literal corner: a string literal is NOT a valid
	// integer/decimal bound (no unknown→number coercion), so it is 42883 — where PG coerces '1' to
	// integer. (A string DOES adapt to a temporal element, exercised in the corpus.)
	if got := errRange(t, db, "SELECT int4range('1', 5)"); got != "42883" {
		t.Errorf("int4range('1', 5) = %s, want 42883", got)
	}
	if got := errRange(t, db, "SELECT numrange('1', 2)"); got != "42883" {
		t.Errorf("numrange('1', 2) = %s, want 42883", got)
	}
	// Arity: only the 2-arg and 3-arg forms exist; anything else is no overload.
	if got := errRange(t, db, "SELECT int4range(1)"); got != "42883" {
		t.Errorf("int4range(1) = %s, want 42883", got)
	}
	if got := errRange(t, db, "SELECT int4range(1, 2, '[]', 3)"); got != "42883" {
		t.Errorf("int4range(1, 2, '[]', 3) = %s, want 42883", got)
	}
}

// TestRangeOperatorDivergences: the range BOOLEAN operators (RF3) — the error cases the oracle corpus
// (which only carries value-producing rows) cannot express, plus the one real divergence
// (spec/design/range-functions.md §3). The agreeing value behavior of all eight operators lives in
// expr/range_operators.test. Mirrors range_operator_divergences in impl/rust/tests/range_storage.rs.
func TestRangeOperatorDivergences(t *testing.T) {
	db := NewEngine()
	// THE divergence: jed has no integer bit-shift, so the `<<` / `>>` tokens are RANGE-only. An
	// integer `<<` / `>>` is "operator does not exist" (42883) — PostgreSQL would compute a bit shift
	// (5 << 2 = 20). A documented divergence (jed owns its surface), so it cannot live in the corpus.
	if got := errRange(t, db, "SELECT 5 << 2"); got != "42883" {
		t.Errorf("5 << 2 = %s, want 42883", got)
	}
	if got := errRange(t, db, "SELECT 5 >> 2"); got != "42883" {
		t.Errorf("5 >> 2 = %s, want 42883", got)
	}
	// A range operator pairs only with a range over the SAME element type (this AGREES with PG's
	// "operator does not exist" 42883, but an error row is awkward in the value-oriented corpus).
	if got := errRange(t, db, "SELECT '[1,5)'::int4range @> '[1,5)'::int8range"); got != "42883" {
		t.Errorf("int4range @> int8range = %s, want 42883", got)
	}
	if got := errRange(t, db, "SELECT '[1,5)'::int4range && '[1,5)'::int8range"); got != "42883" {
		t.Errorf("int4range && int8range = %s, want 42883", got)
	}
	// The positional operators have no element overload — `range << element` is 42883 (only @>/<@ take
	// an element). And `-|-` on non-ranges is 42883 (it is range-only, like PG).
	if got := errRange(t, db, "SELECT '[1,5)'::int4range << 5"); got != "42883" {
		t.Errorf("int4range << 5 = %s, want 42883", got)
	}
	if got := errRange(t, db, "SELECT 1 -|- 2"); got != "42883" {
		t.Errorf("1 -|- 2 = %s, want 42883", got)
	}
	// `-|-` lexes greedily and is NOT confused with `-` then a comment / minus: this is the adjacency
	// operator over two ranges (true here), proving the token won the `--` race.
	if got := queryRendered(t, db, "SELECT '[1,5)'::int4range -|- '[5,9)'::int4range"); !reflect.DeepEqual(got, [][]string{{"true"}}) {
		t.Errorf("int4range -|- int4range = %v, want [[true]]", got)
	}
}
