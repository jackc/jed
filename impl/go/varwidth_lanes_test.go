package jed

// A4 — variable-width columnar lanes + encoded-span predicates (spec/design/packed-leaf.md §11 Track
// A4). On a paged database a single-table aggregate or bare-column projection reads its touched
// variable-width columns in place: IS [NOT] NULL from the leaf's null directory, text/bytea =/<> as a
// byte compare against the stored span, array @>/<@/&& against the stored element bodies, and
// COUNT(col) from nullness alone; every other predicate (and every value a consumer needs) goes through
// the ordinary evaluator over resolved values. The lanes run only on the unmetered lane, so the same
// session with a (never-reached) max_cost runs the row path — the oracle. For every query shape both
// must agree on rows AND cost, over NULLs, empty values, embedded 0x00 bytes, multibyte text, NULL
// array elements, compressed and out-of-line values (which fall back to the evaluator), and a writer's
// uncommitted (Decoded) leaves. Mirrors Rust (tests/varwidth_lanes.rs) and TS
// (tests/varwidth_lanes.test.ts).

import (
	"fmt"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

var varwidthSeed = []string{
	"CREATE TABLE vt (id i32 PRIMARY KEY, n i32, s text, b bytea, tags text[], nums i32[], d decimal)",
	`INSERT INTO vt VALUES
		(1, 1, 'a', '\x00'::bytea, '{a,b}', '{1,2}', 1.5),
		(2, 2, '', '\x'::bytea, '{}', '{}', NULL),
		(3, 3, NULL, NULL, NULL, NULL, 2.25),
		(4, 4, 'é', '\x6100'::bytea, '{é,NULL}', '{NULL,3}', 10),
		(5, 5, 'ñandú', '\x0000'::bytea, '{NULL}', '{-1}', -3.125),
		(6, 6, 'a', '\x61'::bytea, '{b,a,a}', '{3,3}', 0),
		(7, 7, 'ab', '\x6162'::bytea, '{{a,b},{c,NULL}}', '{{1,2},{3,4}}', 7)`,
	// A compressed value set (large-values.md §13): a highly compressible text, bytea, and text array —
	// each falls back to the evaluator. Row 9 (incompressible, out of line) is built by the test.
	"INSERT INTO vt VALUES (8, 8, repeat('x', 30000), decode(repeat('ab', 9000), 'hex'), ARRAY[repeat('a', 9000), 'a'], '{5}', 1)",
	"INSERT INTO vt SELECT g, g % 7, 'v' || (g % 13)::text, NULL, ARRAY['t' || (g % 5)::text], ARRAY[(g % 4)::i32], NULL FROM generate_series(10, 400) g",
}

var varwidthQueries = []string{
	// Nullness atoms, COUNT(col).
	"SELECT count(*) FROM vt WHERE s IS NULL",
	"SELECT count(*) FROM vt WHERE s IS NOT NULL",
	"SELECT count(*) FROM vt WHERE tags IS NULL",
	"SELECT count(*) FROM vt WHERE d IS NOT NULL",
	"SELECT count(s), count(b), count(tags), count(nums), count(d) FROM vt",
	// Text / bytea equality atoms (either operand order, empty, multibyte, embedded zero bytes).
	"SELECT count(*) FROM vt WHERE s = 'a'",
	"SELECT count(*) FROM vt WHERE 'a' = s",
	"SELECT count(*) FROM vt WHERE s <> 'a'",
	"SELECT count(*) FROM vt WHERE s = ''",
	"SELECT count(*) FROM vt WHERE s = 'é'",
	"SELECT count(*) FROM vt WHERE s = 'ñandú'",
	"SELECT count(*) FROM vt WHERE s = 'v3'",
	"SELECT count(*) FROM vt WHERE s <> 'v3'",
	`SELECT count(*) FROM vt WHERE b = '\x00'::bytea`,
	`SELECT count(*) FROM vt WHERE b = '\x'::bytea`,
	`SELECT count(*) FROM vt WHERE b = '\x0000'::bytea`,
	`SELECT count(*) FROM vt WHERE b <> '\x6100'::bytea`,
	// Array atoms (constant on either side; NULL elements; empty; multidimensional; ints).
	"SELECT count(*) FROM vt WHERE tags @> '{a}'::text[]",
	"SELECT count(*) FROM vt WHERE tags @> ARRAY['a']",
	"SELECT count(*) FROM vt WHERE tags @> ARRAY['a', 'b']",
	"SELECT count(*) FROM vt WHERE tags @> '{}'::text[]",
	"SELECT count(*) FROM vt WHERE tags @> ARRAY[NULL]::text[]",
	"SELECT count(*) FROM vt WHERE tags <@ '{a,b,é}'::text[]",
	"SELECT count(*) FROM vt WHERE tags && '{b,é,t1}'::text[]",
	"SELECT count(*) FROM vt WHERE '{a}'::text[] <@ tags",
	"SELECT count(*) FROM vt WHERE '{a,b,c}'::text[] @> tags",
	"SELECT count(*) FROM vt WHERE tags @> '{c}'::text[]",
	"SELECT count(*) FROM vt WHERE nums @> '{3}'::i32[]",
	"SELECT count(*) FROM vt WHERE nums && '{1,-1}'::i32[]",
	"SELECT count(*) FROM vt WHERE nums <@ '{1,2,3}'::i32[]",
	// Mixed: atoms beside generic predicates, OR / NOT (generic), short-circuit order.
	"SELECT count(*) FROM vt WHERE s = 'a' AND n > 1",
	"SELECT count(*) FROM vt WHERE n > 1 AND s = 'a' AND tags @> '{a}'::text[]",
	"SELECT count(*) FROM vt WHERE s IS NULL OR n = 2",
	"SELECT count(*) FROM vt WHERE NOT (s = 'a')",
	"SELECT count(*) FROM vt WHERE s LIKE 'v1%' AND b IS NULL",
	"SELECT count(*) FROM vt WHERE length(s) > 100",
	// Aggregates that resolve values.
	"SELECT max(s), min(s) FROM vt",
	"SELECT max(s) FROM vt WHERE tags @> '{t2}'::text[]",
	"SELECT min(b), max(d) FROM vt",
	"SELECT n, count(tags) FROM vt GROUP BY n",
	"SELECT n, max(s) FROM vt WHERE s <> 'v1' GROUP BY n",
	// Projection lane: survivors only, variable-width projected columns, filter-only columns.
	"SELECT id, s FROM vt",
	"SELECT id, s, tags FROM vt WHERE s = 'a'",
	"SELECT tags, b FROM vt WHERE n = 4",
	"SELECT id FROM vt WHERE tags && '{a,é}'::text[]",
	"SELECT s, s FROM vt WHERE s IS NOT NULL AND id < 12",
	"SELECT id, s FROM vt WHERE length(s) > 100",
	"SELECT id, d FROM vt WHERE d IS NOT NULL",
	"SELECT id FROM vt WHERE id >= 5 AND s = 'v5'",
}

// varwidthRun returns one query's rows (rendered, sorted — no query here has an ORDER BY) and cost.
func varwidthRun(t *testing.T, s *Session, sql string, params []Value) (string, int64) {
	t.Helper()
	out, err := queryOutcome(s, sql, params)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	rows := make([]string, len(out.Rows))
	for i, r := range out.Rows {
		cells := make([]string, len(r))
		for j, v := range r {
			cells[j] = v.Render()
		}
		rows[i] = strings.Join(cells, "\x1f")
	}
	sort.Strings(rows)
	return strings.Join(rows, "\n"), out.Cost
}

// varwidthCheck asserts the lane (unmetered) and the row path (a ceiling it never reaches) agree on
// rows and cost.
func varwidthCheck(t *testing.T, s *Session, sql string, params []Value) {
	t.Helper()
	s.SetMaxCost(0)
	laneRows, laneCost := varwidthRun(t, s, sql, params)
	s.SetMaxCost(1 << 60)
	rowRows, rowCost := varwidthRun(t, s, sql, params)
	s.SetMaxCost(0)
	if laneRows != rowRows || laneCost != rowCost {
		t.Fatalf("lane vs row path for %q:\n lane cost %d rows\n%s\n row cost %d rows\n%s", sql, laneCost, laneRows, rowCost, rowRows)
	}
}

func varwidthCheckAll(t *testing.T, s *Session) {
	t.Helper()
	for _, q := range varwidthQueries {
		varwidthCheck(t, s, q, nil)
	}
	// A bind parameter is a row-independent constant too.
	varwidthCheck(t, s, "SELECT count(*) FROM vt WHERE s = $1", []Value{TextValue("v4")})
	varwidthCheck(t, s, "SELECT id FROM vt WHERE s <> $1 AND n = 3", []Value{TextValue("v3")})
}

func TestVarwidthLanesMatchRowPath(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "varwidth_lanes.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	seed := db.Session(SessionOptions{})
	for _, sql := range varwidthSeed {
		mustExec(t, seed, sql)
	}
	// Incompressible text and a large text array, stored out of line (large-values.md §12).
	x := uint64(0x9E3779B97F4A7C15)
	noise := func(n int) string {
		var sb strings.Builder
		for range n {
			x ^= x << 13
			x ^= x >> 7
			x ^= x << 17
			sb.WriteByte(byte('a' + x%26))
		}
		return sb.String()
	}
	long := noise(30000)
	elems := make([]string, 700)
	for i := range elems {
		elems[i] = noise(40)
	}
	mustExec(t, seed, fmt.Sprintf("INSERT INTO vt VALUES (9, 9, '%s', NULL, '{%s}', NULL, NULL)", long, strings.Join(elems, ",")))
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	paged, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()
	s := paged.Session(SessionOptions{})
	// Clean (Packed) leaves faulted from the file.
	varwidthCheckAll(t, s)
	// A writer's uncommitted changes live in Decoded leaves: the atoms fall back to the evaluator.
	mustExec(t, s, "BEGIN")
	mustExec(t, s, `INSERT INTO vt VALUES (1000, 1, 'a', '\x00'::bytea, '{a}', '{3}', 1)`)
	mustExec(t, s, "UPDATE vt SET s = 'é' WHERE id = 6")
	varwidthCheckAll(t, s)
	mustExec(t, s, "ROLLBACK")
}

func TestLen16PayloadRequiresAnExactBody(t *testing.T) {
	t.Parallel()
	cases := []struct {
		body []byte
		want []byte
		ok   bool
	}{
		{[]byte{0, 0}, []byte{}, true},
		{[]byte{0, 2, 0, 'a'}, []byte{0, 'a'}, true},
		{[]byte{0, 2, 0}, nil, false},        // short
		{[]byte{0, 1, 'a', 'b'}, nil, false}, // trailing byte
		{[]byte{0}, nil, false},
	}
	for _, c := range cases {
		got, ok := len16Payload(c.body)
		if ok != c.ok || (ok && string(got) != string(c.want)) {
			t.Fatalf("len16Payload(%v) = %v, %v", c.body, got, ok)
		}
	}
}

func varwidthTextArray(elems []*string) []byte {
	vals := make([]Value, len(elems))
	for i, e := range elems {
		if e == nil {
			vals[i] = NullValue()
		} else {
			vals[i] = TextValue(*e)
		}
	}
	ty := colType{Elem: &colType{Scalar: scalarText}}
	return encodeValue(ty, ArrayValue(vals))[1:]
}

func TestArrayElementSpans(t *testing.T) {
	t.Parallel()
	str := func(s string) *string { return &s }
	var spans [][2]int
	body := varwidthTextArray([]*string{str("a"), nil, str("é"), str("")})
	anyNull, ok := arrayElementSpans(body, elemCodec{text: true}, &spans)
	if !ok || !anyNull || len(spans) != 3 {
		t.Fatalf("well-formed body: anyNull=%v ok=%v spans=%v", anyNull, ok, spans)
	}
	want := []string{"\x00\x01a", "\x00\x02é", "\x00\x00"}
	for i, s := range spans {
		if string(body[s[0]:s[1]]) != want[i] {
			t.Fatalf("element %d = %q want %q", i, body[s[0]:s[1]], want[i])
		}
	}
	if anyNull, ok := arrayElementSpans(varwidthTextArray(nil), elemCodec{text: true}, &spans); !ok || anyNull || len(spans) != 0 {
		t.Fatalf("empty array: anyNull=%v ok=%v spans=%v", anyNull, ok, spans)
	}
	// Truncated, trailing bytes, a reserved flag bit, ndim over the maximum.
	ab := varwidthTextArray([]*string{str("ab"), str("c")})
	bad := [][]byte{ab[:len(ab)-1], append(append([]byte{}, ab...), 0)}
	flags := append([]byte{}, ab...)
	flags[1] = 0x02
	ndim := append([]byte{}, ab...)
	ndim[0] = 7
	bad = append(bad, flags, ndim)
	for i, b := range bad {
		if _, ok := arrayElementSpans(b, elemCodec{text: true}, &spans); ok {
			t.Fatalf("malformed body %d accepted", i)
		}
	}
	// An element that is not valid UTF-8 leaves the decoder to report it; as bytea it is well-formed.
	inv := varwidthTextArray([]*string{str("ab")})
	inv[len(inv)-1] = 0xFF
	if _, ok := arrayElementSpans(inv, elemCodec{text: true}, &spans); ok {
		t.Fatal("invalid UTF-8 text element accepted")
	}
	if _, ok := arrayElementSpans(inv, elemCodec{}, &spans); !ok {
		t.Fatal("bytea element rejected")
	}
}
