package jed

// A4 — variable-width columnar lanes + encoded-span predicates (spec/design/packed-leaf.md §11 Track
// A4). On a paged database a single-table aggregate or bare-column projection reads its touched
// variable-width columns in place: IS [NOT] NULL from the leaf's null directory, text/bytea =/<> as a
// byte compare against the stored span, array @>/<@/&& against the stored element bodies, and
// COUNT(col) from nullness alone; every other predicate (and every value a consumer needs) goes through
// the ordinary evaluator over resolved values. With the aggregate lane's ceiling attempt switched off
// (cost.md §6.1), the same session with a (never-reached) max_cost runs the row path — the oracle. For every query shape both
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
	// Under a ceiling the aggregate lane runs as an attempt (cost.md §6.1); the oracle is the row
	// path alone.
	s.engine.session.lane.ceilingOff = true
	rowRows, rowCost := varwidthRun(t, s, sql, params)
	s.engine.session.lane.ceilingOff = false
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

// ==== A5 and the ceiling attempt ====

// A5 — the lanes widened (spec/design/packed-leaf.md §11 Track A5; memory.md §5.4, §6.6.1) and the
// aggregate lane under a cost ceiling (cost.md §6.1). Decimal SUM/AVG fold on the columnar lane; a
// grouped fold on a spill-capable host keeps a bounded group table that hands over to the spill
// structures and leaves as a spool, sorted by the bounded-spill lane's ORDER BY stage; under a cost
// ceiling or lifetime budget the lane runs as an attempt the row path replaces unless it finishes
// cleanly under every ceiling. The oracle is the row path: the same session under a ceiling it never
// reaches, with the attempt switched off. Mirrors Rust (tests/varwidth_lanes.rs) and TS
// (tests/varwidth_lanes.test.ts).

// laneSession builds a fresh database from seed: file-backed and reopened (clean Packed leaves) when
// file is set, in memory otherwise.
func laneSession(t *testing.T, file string, seed []string) *Session {
	t.Helper()
	if file == "" {
		db, err := CreateDatabase(CreateOptions{})
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { db.Close() })
		s := db.Session(SessionOptions{})
		for _, sql := range seed {
			mustExec(t, s, sql)
		}
		return s
	}
	path := filepath.Join(t.TempDir(), file)
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	for _, sql := range seed {
		mustExec(t, s, sql)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	paged, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { paged.Close() })
	return paged.Session(SessionOptions{})
}

// laneRows renders an outcome's rows — in order for an ORDER BY query, sorted otherwise.
func laneRows(out outcome, sql string) string {
	rows := make([]string, len(out.Rows))
	for i, r := range out.Rows {
		cells := make([]string, len(r))
		for j, v := range r {
			cells[j] = v.Render()
		}
		rows[i] = strings.Join(cells, "\x1f")
	}
	if !strings.Contains(sql, "ORDER BY") {
		sort.Strings(rows)
	}
	return strings.Join(rows, "\n")
}

// laneOutcome is one query's rows and cost, or its error code.
func laneOutcome(s *Session, sql string) string {
	out, err := queryOutcome(s, sql, nil)
	if err != nil {
		if e, ok := err.(*EngineError); ok {
			return "error " + e.Code()
		}
		return "error " + err.Error()
	}
	return fmt.Sprintf("cost %d\n%s", out.Cost, laneRows(out, sql))
}

// laneCheck asserts the lane (unmetered) and the row path (a ceiling it never reaches, the attempt
// off) agree on rows and cost.
func laneCheck(t *testing.T, s *Session, sql string) {
	t.Helper()
	s.SetMaxCost(0)
	lane := laneOutcome(s, sql)
	s.SetMaxCost(1 << 60)
	s.engine.session.lane.ceilingOff = true
	row := laneOutcome(s, sql)
	s.engine.session.lane.ceilingOff = false
	s.SetMaxCost(0)
	if strings.HasPrefix(lane, "error") || lane != row {
		t.Fatalf("lane vs row path for %q:\n lane %s\n row  %s", sql, lane, row)
	}
}

// laneCheckUnderBudgets: under a query-memory budget the lane and the row path hold different
// buffers, so they may trip 54P05 at different budgets (memory.md §5.4; the thresholds themselves are
// pinned across cores by `mise run conformance:query_memory`). What must hold on every path and at
// every budget: a statement either fails 54P05 or returns exactly its unbudgeted rows and cost. Run
// unmetered (the lane), under a ceiling it never reaches (the lane as an attempt), and with the
// attempt off (the row path).
func laneCheckUnderBudgets(t *testing.T, s *Session, sql string) {
	t.Helper()
	s.SetMaxCost(0)
	want := laneOutcome(s, sql)
	if strings.HasPrefix(want, "error") {
		t.Fatalf("unbudgeted %q: %s", sql, want)
	}
	for _, budget := range []int64{64, 4096, 64 * 1024, 1024 * 1024, 64 * 1024 * 1024} {
		s.SetMaxQueryMemoryBytes(budget)
		for _, c := range []struct {
			maxCost int64
			off     bool
		}{{0, false}, {1 << 60, false}, {1 << 60, true}} {
			s.SetMaxCost(c.maxCost)
			s.engine.session.lane.ceilingOff = c.off
			got := laneOutcome(s, sql)
			s.engine.session.lane.ceilingOff = false
			if got != want && got != "error 54P05" {
				t.Fatalf("budget %d, max_cost %d, attempt off %v: %q: %s", budget, c.maxCost, c.off, sql, got)
			}
		}
		s.SetMaxCost(0)
		s.SetMaxQueryMemoryBytes(0)
	}
}

var decimalFoldSeed = []string{
	"CREATE TABLE dt (id i32 PRIMARY KEY, n i32, k i64, big i64, d numeric(12,2), wide numeric)",
	// Small money-shaped values, a NULL in every column, and a NULL group key.
	`INSERT INTO dt VALUES
		(-3, 1, NULL, 9223372036854775807, 0.01, 1),
		(-2, 1, 1, -9223372036854775808, -0.01, 0.000000000000000000000000000001),
		(-1, NULL, 2, NULL, NULL, NULL),
		(0, 2, 2, 0, 0.00, 0.0)`,
	`INSERT INTO dt SELECT g, g % 5, g % 3,
		CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693,
		((g * 7919) % 100000000)::numeric / 100 - 250000,
		CASE WHEN g % 11 = 0 THEN NULL
		     WHEN g % 7 = 0 THEN g * 123456789012345678901234567890.5 * 10000000000000000000000.0
		     WHEN g % 4 = 0 THEN -(g::numeric / 1000000007)
		     ELSE g::numeric * 1.000001 END
		FROM generate_series(1, 3000) g`,
}

var decimalFoldQueries = []string{
	"SELECT sum(big) FROM dt",
	"SELECT sum(d) FROM dt",
	"SELECT sum(wide) FROM dt",
	"SELECT avg(big), avg(d), avg(wide), avg(n) FROM dt",
	"SELECT sum(big), sum(d), sum(wide), count(*), min(d), max(wide) FROM dt",
	"SELECT sum(d) FROM dt WHERE n > 2",
	"SELECT avg(d) FROM dt WHERE d > 0",
	"SELECT sum(big), avg(wide) FROM dt WHERE wide IS NOT NULL AND big < 0",
	"SELECT sum(d) FROM dt WHERE id < 0",
	"SELECT n, sum(big), sum(d), avg(d) FROM dt GROUP BY n",
	"SELECT n, sum(wide), avg(big), count(d) FROM dt WHERE d <> 0 GROUP BY n",
	"SELECT k, sum(d) FROM dt GROUP BY k",
}

// The decimal folds — SUM(i64|decimal) and AVG over an integer or decimal — on the lane against the
// row path, rows and cost: sums that grow past 2^64 and 2^128, a NULL group key, on a file (clean
// leaves, then a writer's uncommitted ones) and in memory, and under query-memory budgets.
func TestDecimalFoldsMatchRowPath(t *testing.T) {
	t.Parallel()
	s := laneSession(t, "decimal_fold_lanes.jed", decimalFoldSeed)
	for _, q := range decimalFoldQueries {
		laneCheck(t, s, q)
		laneCheckUnderBudgets(t, s, q)
	}
	mustExec(t, s, "BEGIN")
	mustExec(t, s, "INSERT INTO dt VALUES (5000, 1, 1, 42, 123.45, 1e40)")
	mustExec(t, s, "UPDATE dt SET d = d + 1, big = big / 2 WHERE n = 3")
	for _, q := range decimalFoldQueries {
		laneCheck(t, s, q)
	}
	mustExec(t, s, "ROLLBACK")

	mem := laneSession(t, "", decimalFoldSeed)
	for _, q := range decimalFoldQueries {
		laneCheck(t, mem, q)
		laneCheckUnderBudgets(t, mem, q)
	}
}

// gtSeed is the `gt` table — narrow and wide decimals, text, floats, NULLs in every column.
var gtSeed = []string{
	"CREATE TABLE gt (id i32 PRIMARY KEY, n i32, k i64, big i64, d numeric(12,2), wide numeric, s text, f f64, m i16)",
	`INSERT INTO gt VALUES
		(-3, 1, NULL, 9223372036854775807, 0.01, 1, 'a', 1.5, 1),
		(-2, 1, 1, -9223372036854775808, -0.01, 0.000000000000000000000000000001, '', -2.25, -1),
		(-1, NULL, 2, NULL, NULL, NULL, NULL, NULL, NULL),
		(0, 2, 2, 0, 0.00, 0.0, 'é', 0, 0)`,
	`INSERT INTO gt SELECT g, g % 5, g % 3,
		CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693,
		((g * 7919) % 100000000)::numeric / 100 - 250000,
		CASE WHEN g % 11 = 0 THEN NULL
		     WHEN g % 7 = 0 THEN g * 123456789012345678901234567890.5 * 10000000000000000000000.0
		     WHEN g % 4 = 0 THEN -(g::numeric / 1000000007)
		     ELSE g::numeric * 1.000001 END,
		'v' || repeat('x', (g % 40)::i32) || (g % 97)::text,
		(g % 1000)::f64 / 8, (g % 300)::i16
		FROM generate_series(1, 3000) g`,
}

var groupedHandoverQueries = []string{
	// A few groups, every accumulator kind (fixed-size and size-changing), NULL keys.
	"SELECT n, count(*), count(d), sum(big), sum(d), avg(d), min(d), max(wide), min(s), max(s) FROM gt GROUP BY n",
	"SELECT k, sum(wide), avg(big), min(wide), max(big), count(s) FROM gt WHERE d <> 0 GROUP BY k",
	"SELECT n, sum(f), avg(f), sum(m) FROM gt GROUP BY n",
	// About one group per row.
	"SELECT big, sum(d), max(s), count(*) FROM gt GROUP BY big",
	"SELECT big, count(*), min(s) FROM gt WHERE n > 1 AND s <> 'v3' GROUP BY big",
	// Grouped by the primary key, which the lane does not take: the same answer all the same.
	"SELECT id, sum(big) FROM gt GROUP BY id",
	// No survivors, and a primary-key bound.
	"SELECT n, sum(d) FROM gt WHERE id < -100 GROUP BY n",
	"SELECT n, sum(d), max(s) FROM gt WHERE id BETWEEN 100 AND 900 GROUP BY n",
}

// Grouped folds with an ORDER BY over their groups — by the key, by an aggregate, by an expression of
// both — each a total order, so the row sequence is part of the comparison.
var orderedGroupQueries = []string{
	"SELECT n, count(*), sum(d) FROM gt GROUP BY n ORDER BY n",
	"SELECT k, avg(big), max(wide) FROM gt GROUP BY k ORDER BY k DESC NULLS FIRST",
	"SELECT n, sum(d), max(s) FROM gt GROUP BY n ORDER BY sum(d) DESC NULLS LAST, n",
	"SELECT big, count(*), min(s) FROM gt GROUP BY big ORDER BY min(s) DESC, big LIMIT 9 OFFSET 3",
	"SELECT big, sum(d) FROM gt WHERE n > 1 GROUP BY big ORDER BY big % 7, big",
	"SELECT big, sum(d) FROM gt GROUP BY big ORDER BY sum(d) + big % 3 DESC, big LIMIT 40",
	"SELECT n, sum(d) FROM gt WHERE id < -100 GROUP BY n ORDER BY n",
	"SELECT n, max(s) FROM gt GROUP BY n ORDER BY n LIMIT 0",
}

// Grouped aggregates on a file-backed database take the columnar lane while the group table fits
// work_mem, then hand it over to the spill structures and carry on (memory.md §6.6.1). Each budget
// below places the hand-over somewhere else — after the first row, after a few groups, part-way
// through a many-group scan, never — and at each the lane must agree with the row path on rows and
// cost.
func TestGroupedLaneHandsOverToSpill(t *testing.T) {
	t.Parallel()
	s := laneSession(t, "grouped_handover_lanes.jed", gtSeed)
	handovers := func() uint64 { return s.engine.session.lane.handovers }
	// work_mem of 1 hands over after the first survivor; the middle budgets after a handful of groups
	// and part-way through the many-group scans; the default never.
	for _, workMem := range []int{1, 400, 3_000, 40_000, 300_000, defaultWorkMem} {
		s.SetWorkMem(workMem)
		for _, q := range groupedHandoverQueries {
			before := handovers()
			laneCheck(t, s, q)
			// At the smallest budgets every query with a survivor hands over on its lane run.
			if workMem <= 400 && !strings.Contains(q, "id < -100") && !strings.Contains(q, "GROUP BY id") && handovers() == before {
				t.Fatalf("work_mem %d: no hand-over: %q", workMem, q)
			}
		}
	}
	// An ORDER BY over the groups: sorted by the bounded-spill lane's own stage, whether or not the
	// fold handed over.
	for _, workMem := range []int{1, 400, 3_000, 40_000, defaultWorkMem} {
		s.SetWorkMem(workMem)
		for _, q := range orderedGroupQueries {
			before := handovers()
			laneCheck(t, s, q)
			if workMem <= 400 && !strings.Contains(q, "id < -100") && handovers() == before {
				t.Fatalf("work_mem %d: no hand-over: %q", workMem, q)
			}
		}
	}
	// LIMIT/OFFSET over a handed-over result: the same window of the same emission order.
	s.SetWorkMem(3_000)
	for _, q := range []string{
		"SELECT big, sum(d) FROM gt GROUP BY big LIMIT 7",
		"SELECT big, count(*) FROM gt GROUP BY big LIMIT 5 OFFSET 2990",
		"SELECT big, count(*) FROM gt GROUP BY big OFFSET 5000",
		"SELECT n, max(s) FROM gt GROUP BY n LIMIT 0",
	} {
		// No ORDER BY: emission order is scan-order-of-first-appearance on both paths.
		s.SetMaxCost(0)
		lane, err := queryOutcome(s, q, nil)
		if err != nil {
			t.Fatalf("%q: %v", q, err)
		}
		s.SetMaxCost(1 << 60)
		s.engine.session.lane.ceilingOff = true
		row, err := queryOutcome(s, q, nil)
		s.engine.session.lane.ceilingOff = false
		s.SetMaxCost(0)
		if err != nil {
			t.Fatalf("%q: %v", q, err)
		}
		if lane.Cost != row.Cost || laneRows(lane, "ORDER BY") != laneRows(row, "ORDER BY") {
			t.Fatalf("window over a handed-over result: %q", q)
		}
	}
	// A writer's uncommitted (Decoded) leaves, mid hand-over.
	mustExec(t, s, "BEGIN")
	mustExec(t, s, "INSERT INTO gt VALUES (5000, 9, 9, 42, 123.45, 1e40, 'zz', 3.5, 7)")
	mustExec(t, s, "UPDATE gt SET d = d + 1, s = s || '!' WHERE n = 3")
	for _, workMem := range []int{1, 3_000, defaultWorkMem} {
		s.SetWorkMem(workMem)
		for _, q := range groupedHandoverQueries {
			laneCheck(t, s, q)
		}
	}
	mustExec(t, s, "ROLLBACK")
	// Under a query-memory budget: the same rows and cost, or 54P05.
	s.SetWorkMem(3_000)
	for _, q := range append(append([]string{}, groupedHandoverQueries...), orderedGroupQueries...) {
		laneCheckUnderBudgets(t, s, q)
	}
}

// The bounded table's measure of a group is the keyed state map's (memory.md §6.6.1): an
// accumulator's variable part tracks exactly the change in its packed size.
func TestLaneGroupSizeTracksThePackedState(t *testing.T) {
	t.Parallel()
	packed := func(a *acc) int64 { return memValueBytes(CompositeValue(blockingAccRow(a))) }
	dec := func(text string) Decimal {
		d, err := ParseDecimal(text)
		if err != nil {
			t.Fatal(err)
		}
		return d
	}
	wide := dec("-" + strings.Repeat("7", 52) + "." + strings.Repeat("7", 9))
	min := func(cur *Value) *acc {
		a := &acc{plan: planMin}
		if cur != nil {
			a.cur, a.hasCur = *cur, true
		}
		return a
	}
	val := func(v Value) *Value { return &v }
	// (fresh, folded) pairs: decimal sums over one and many digit groups, a MIN over fixed-width,
	// text, and decimal values, and the fixed-size accumulators.
	cases := [][2]*acc{
		{{plan: planSumDecimal}, {plan: planSumDecimal, sumDec: dec("12345"), seen: true}},
		{{plan: planSumDecimal}, {plan: planSumDecimal, sumDec: wide, seen: true}},
		{{plan: planAvg}, {plan: planAvg, sumDec: wide, count: 9}},
		{min(nil), min(val(IntValue(7)))},
		{min(nil), min(val(TextValue("ñandú and more")))},
		{min(nil), min(val(DecimalValue(wide)))},
		{{plan: planCount}, {plan: planCount, count: 41}},
		{{plan: planSumInt}, {plan: planSumInt, sumInt: 1<<63 - 1, seen: true}},
	}
	for i, c := range cases {
		tracked := accVariableBytes(c[1]) - accVariableBytes(c[0])
		if want := packed(c[1]) - packed(c[0]); tracked != want {
			t.Fatalf("case %d: tracked %d, packed %d", i, tracked, want)
		}
	}
	// One fresh group's base is ENTRY + key + its packed fresh state.
	specs := []aggSpec{{plan: planCountStar}, {plan: planSumDecimal}, {plan: planMin}}
	zone := ZoneRef{}
	if got, want := laneGroupBaseBytes(specs, zone), memMapEntryBytes([]Value{IntValue(5)}, freshGroupState(specs, 3, zone)); got != want {
		t.Fatalf("group base %d, state-map measure %d", got, want)
	}
}

// ---- The aggregate lane under a cost ceiling (spec/design/cost.md §6.1) ----------------------

// laneMeteredOutcome is one statement's whole observable outcome under the session's current
// ceilings: its rows and cost, or its error code and text, and what it added to the session's
// cumulative cost.
func laneMeteredOutcome(s *Session, sql string) (string, int64) {
	before := s.LifetimeCost()
	out, err := queryOutcome(s, sql, nil)
	spent := s.LifetimeCost() - before
	if err != nil {
		if e, ok := err.(*EngineError); ok {
			return fmt.Sprintf("%s: %v", e.Code(), err), spent
		}
		return "error: " + err.Error(), spent
	}
	return fmt.Sprintf("cost %d: %s", out.Cost, laneRows(out, sql)), spent
}

// laneMatchesRowPath runs the same statement with the lane's attempt on and off: every observable
// must match.
func laneMatchesRowPath(t *testing.T, s *Session, sql, what string) {
	t.Helper()
	tools := s.engine.session.lane
	tools.ceilingOff = false
	lane, laneSpent := laneMeteredOutcome(s, sql)
	tools.ceilingOff = true
	row, rowSpent := laneMeteredOutcome(s, sql)
	tools.ceilingOff = false
	if lane != row || laneSpent != rowSpent {
		t.Fatalf("%s: %q:\n attempt  (spent %d) %s\n row path (spent %d) %s", what, sql, laneSpent, lane, rowSpent, row)
	}
}

// laneRowPathSpend is what the row path alone spends on sql with a ceiling it never reaches (to an
// error, if it raises one): the top of the range of ceilings worth sweeping.
func laneRowPathSpend(s *Session, sql string) int64 {
	s.engine.session.lane.ceilingOff = true
	s.SetMaxCost(1 << 60)
	_, spent := laneMeteredOutcome(s, sql)
	s.SetMaxCost(0)
	s.engine.session.lane.ceilingOff = false
	return spent
}

// laneCeilings is every step-th ceiling from 1 to past top, and the boundary itself.
func laneCeilings(top int64, step int64) []int64 {
	var out []int64
	for l := int64(1); l <= top+2; l += step {
		out = append(out, l)
	}
	for l := top - 1; l <= top+2; l++ {
		if l > 0 {
			out = append(out, l)
		}
	}
	return out
}

// laneSweepMaxCost runs sql at every step-th max_cost from 1 to past its cost, and at the boundary.
func laneSweepMaxCost(t *testing.T, s *Session, sql string, step int64) {
	t.Helper()
	for _, limit := range laneCeilings(laneRowPathSpend(s, sql), step) {
		s.SetMaxCost(limit)
		laneMatchesRowPath(t, s, sql, fmt.Sprintf("max_cost %d", limit))
	}
	s.SetMaxCost(0)
}

// laneSweepLifetime runs sql under a lifetime budget with `left` units remaining, alone and beside a
// max_cost one either side of it (which ceiling's error wins is part of the outcome).
func laneSweepLifetime(t *testing.T, s *Session, sql string, step int64) {
	t.Helper()
	tools := s.engine.session.lane
	for _, left := range laneCeilings(laneRowPathSpend(s, sql), step) {
		for _, maxCost := range []int64{0, left - 1, left, left + 1} {
			if maxCost < 0 {
				continue
			}
			// Each run draws on the budget, so it is re-armed `left` above the running total.
			run := func(off bool) (string, int64) {
				tools.ceilingOff = off
				s.SetLifetimeMaxCost(s.LifetimeCost() + left)
				s.SetMaxCost(maxCost)
				out, spent := laneMeteredOutcome(s, sql)
				// The lifetime error's text carries the session's absolute totals, which differ between
				// the two runs; its code and the cost it left accrued are what must match.
				if strings.HasPrefix(out, "54P02") {
					out = "54P02"
				}
				s.SetLifetimeMaxCost(0)
				s.SetMaxCost(0)
				tools.ceilingOff = false
				return out, spent
			}
			lane, laneSpent := run(false)
			row, rowSpent := run(true)
			if lane != row || laneSpent != rowSpent {
				t.Fatalf("lifetime +%d, max_cost %d: %q:\n attempt  (spent %d) %s\n row path (spent %d) %s", left, maxCost, sql, laneSpent, lane, rowSpent, row)
			}
		}
	}
}

// ctSeed is a 300-row table small enough to try every ceiling: a NULL row, decimals of both signs,
// text.
var ctSeed = []string{
	"CREATE TABLE ct (id i32 PRIMARY KEY, n i32, big i64, d numeric(12,2), s text)",
	"INSERT INTO ct VALUES (0, NULL, NULL, NULL, NULL)",
	`INSERT INTO ct SELECT g, g % 5,
		CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693,
		((g * 7919) % 100000000)::numeric / 100 - 250000,
		'v' || repeat('x', (g % 40)::i32) || (g % 97)::text
		FROM generate_series(1, 300) g`,
}

// Lane-shaped statements whose filters are safe to discard: whole-table and grouped folds, ordered
// groups, a window, no survivors, and filters that raise part-way through the scan.
var ceilingQueries = []string{
	"SELECT count(*), sum(d), min(s) FROM ct",
	"SELECT sum(big), avg(d) FROM ct WHERE n > 1 AND d <> 0",
	"SELECT n, count(*), sum(d) FROM ct GROUP BY n",
	"SELECT n, sum(d), max(s) FROM ct WHERE big % 3 <> 1 GROUP BY n ORDER BY sum(d) DESC, n",
	"SELECT n, count(*) FROM ct GROUP BY n ORDER BY n LIMIT 2 OFFSET 1",
	"SELECT big, count(*), max(d) FROM ct WHERE n IN (1, 3) GROUP BY big",
	"SELECT n, sum(d) FROM ct WHERE id < -5 GROUP BY n",
	"SELECT count(*), sum(d) FROM ct WHERE 1000 / (id - 200) > 0",
	"SELECT n, sum(d) FROM ct WHERE CASE WHEN id > 250 THEN 1000 / (id - 260) ELSE 1 END <> 7 GROUP BY n",
	"SELECT * FROM (SELECT n, sum(d) AS t FROM ct GROUP BY n) g WHERE t IS NOT NULL",
	// An uncorrelated scalar subquery is folded to a constant before the lane runs.
	"SELECT count(*), sum(d) FROM ct WHERE n > (SELECT 1)",
}

func laneUnderACeilingMatchesRowPath(t *testing.T, file string) {
	s := laneSession(t, file, ctSeed)
	tools := s.engine.session.lane
	attempts := func() [2]uint64 { return [2]uint64{tools.attempts, tools.fallbacks} }
	// On a file: every ceiling for a whole-table fold, a grouped fold and a filter that raises; every
	// eleventh for the rest (the sweep always adds the boundary itself). Every eleventh in memory too,
	// where the lane and row path are the same pair.
	every := map[int]bool{0: true, 2: true, 7: true}
	for qi, q := range ceilingQueries {
		step := int64(11)
		if file != "" && every[qi] {
			step = 1
		}
		before := attempts()
		laneSweepMaxCost(t, s, q, step)
		after := attempts()
		// In memory an ordered grouping does not take the lane at all (packed-leaf.md §11 Track A5).
		if file == "" && strings.Contains(q, "ORDER BY") {
			if after != before {
				t.Fatalf("attempted in memory: %q", q)
			}
			continue
		}
		if after[0] == before[0] {
			t.Fatalf("the lane was never attempted: %q", q)
		}
		// A statement that completes was accepted at its high ceilings and rejected at its low ones.
		if after[1] == before[1] {
			t.Fatalf("the lane never fell back: %q", q)
		}
		if !strings.Contains(q, "1000 /") && after[0]-before[0] <= after[1]-before[1] {
			t.Fatalf("the lane was never accepted: %q", q)
		}
		laneSweepLifetime(t, s, q, 41)
	}
	// The grouped fold handing over to the spill structures part-way (file only).
	if file != "" {
		for _, workMem := range []int{1, 400, 3_000} {
			s.SetWorkMem(workMem)
			for _, q := range ceilingQueries {
				if strings.Contains(q, "GROUP BY") {
					laneSweepMaxCost(t, s, q, 47)
				}
			}
		}
		s.SetWorkMem(defaultWorkMem)
	}
	// Not safe to discard — entropy, a function call, a correlated subquery, a regex — so never
	// attempted.
	for _, q := range []string{
		"SELECT count(*), sum(d) FROM ct WHERE uuidv4() IS NOT NULL",
		"SELECT n, sum(d) FROM ct WHERE abs(n) > 1 GROUP BY n",
		"SELECT count(*) FROM ct WHERE n > (SELECT count(*) FROM ct c WHERE c.id = ct.id)",
		"SELECT count(*) FROM ct WHERE s ~ '^vx+1'",
	} {
		before := attempts()
		laneSweepMaxCost(t, s, q, 97)
		if attempts() != before {
			t.Fatalf("attempted though not safe to discard: %q", q)
		}
	}
	// With a query-memory budget too (one nothing here reaches), the attempt is still made.
	s.SetMaxQueryMemoryBytes(64 * 1024 * 1024)
	before := attempts()
	for _, q := range ceilingQueries {
		laneSweepMaxCost(t, s, q, 97)
	}
	if attempts()[0] == before[0] {
		t.Fatal("no attempt under a query-memory budget")
	}
	s.SetMaxQueryMemoryBytes(0)
	// A writer's uncommitted leaves, at a ceiling the statement fits under (an abort would end the
	// transaction).
	mustExec(t, s, "BEGIN")
	mustExec(t, s, "INSERT INTO ct VALUES (900, 9, 42, 123.45, 'zz')")
	mustExec(t, s, "UPDATE ct SET d = d + 1, s = s || '!' WHERE n = 3")
	for _, q := range ceilingQueries {
		if strings.Contains(q, "1000 /") {
			continue
		}
		top := laneRowPathSpend(s, q)
		s.SetMaxCost(top + 1)
		laneMatchesRowPath(t, s, q, "uncommitted writer")
		s.SetMaxCost(0)
	}
	mustExec(t, s, "ROLLBACK")
}

func TestLaneUnderACeilingMatchesRowPathOnAFile(t *testing.T) {
	t.Parallel()
	laneUnderACeilingMatchesRowPath(t, "ceiling_lanes.jed")
}

func TestLaneUnderACeilingMatchesRowPathInMemory(t *testing.T) {
	t.Parallel()
	laneUnderACeilingMatchesRowPath(t, "")
}

// A sampled sweep over the 3,000-row table: many groups, a mid-scan hand-over, wide decimals.
func TestLaneUnderACeilingMatchesRowPathOnALargerTable(t *testing.T) {
	t.Parallel()
	s := laneSession(t, "ceiling_lanes_large.jed", gtSeed)
	for _, workMem := range []int{3_000, defaultWorkMem} {
		s.SetWorkMem(workMem)
		for _, q := range []string{
			"SELECT count(*), sum(wide), avg(big), max(s) FROM gt WHERE d <> 0",
			"SELECT big, sum(d), max(s), count(*) FROM gt GROUP BY big",
			"SELECT n, sum(d), max(s) FROM gt GROUP BY n ORDER BY sum(d) DESC NULLS LAST, n",
			"SELECT big, count(*) FROM gt WHERE n > 1 GROUP BY big ORDER BY big % 7, big LIMIT 9",
		} {
			// A dozen ceilings across the statement's cost, plus its boundary.
			laneSweepMaxCost(t, s, q, laneRowPathSpend(s, q)/12+1)
		}
	}
}

// An engine with no pager has unpacked stores, which the columnar walk declines: its aggregates take
// the row-based vectorized fold (foldAggBatch / groupByIntKey). The decimal folds must match the row
// path there too, on rows and cost.
func TestDecimalFoldsOnTheRowBasedKernelMatchRowPath(t *testing.T) {
	t.Parallel()
	db := newEngine()
	for _, sql := range decimalFoldSeed {
		if _, err := queryOutcome(db, sql, nil); err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
	}
	if st := db.lkpStore("dt"); st == nil || st.paging != nil {
		t.Fatal("expected an unpacked store")
	}
	run := func(sql string) string {
		out, err := queryOutcome(db, sql, nil)
		if err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
		return fmt.Sprintf("cost %d\n%s", out.Cost, laneRows(out, sql))
	}
	for _, q := range decimalFoldQueries {
		db.SetMaxCost(0)
		lane := run(q)
		db.SetMaxCost(1 << 60)
		db.laneTools().ceilingOff = true
		row := run(q)
		db.laneTools().ceilingOff = false
		attempt := run(q)
		db.SetMaxCost(0)
		if lane != row || attempt != row {
			t.Fatalf("%q:\n lane    %s\n attempt %s\n row     %s", q, lane, attempt, row)
		}
	}
}

// The file-backed top-k heap admits a touched numeric(p,s) column, budgeting its largest digit-group
// payload on top of the fixed-width estimate: K × (8 + 40 × columns + Σ 4 × ⌈p/4⌉) must fit work_mem
// (spill.md §4.1). An unbounded numeric or a text column still takes the external sorter.
func TestTopKAdmitsABoundedDecimal(t *testing.T) {
	t.Parallel()
	s := laneSession(t, "top_k_decimal.jed", gtSeed)
	db := s.engine
	// gt: id, n, k, big, d numeric(12,2), wide numeric, s text, f, m — nine columns.
	mask := func(cols ...int) *selectPlan {
		m := make([]bool, 9)
		for _, c := range cols {
			m[c] = true
		}
		return &selectPlan{rels: []planRel{{tableName: "gt"}}, relMasks: [][]bool{m}}
	}
	fixed, decimal, unbounded, text := mask(0, 3), mask(0, 4), mask(0, 5), mask(0, 6)
	// 8 + 40 × 9 = 368 per row; the decimal(12,2) adds 4 × 3 = 12.
	for _, c := range []struct {
		plan    *selectPlan
		workMem int
		k       int64
		want    bool
	}{
		{fixed, 368 * 10, 10, true},
		{fixed, 368*10 - 1, 10, false},
		{decimal, 380 * 10, 10, true},
		{decimal, 380*10 - 1, 10, false},
		{decimal, 380 * 10, 11, false},
		{unbounded, defaultWorkMem, 1, false},
		{text, defaultWorkMem, 1, false},
		{text, 0, 1 << 40, true}, // work_mem 0 never spills: always the heap
	} {
		s.SetWorkMem(c.workMem)
		if got := db.streamingTopKFits(c.plan, c.k); got != c.want {
			t.Fatalf("work_mem %d, k %d, mask %v: fits = %v", c.workMem, c.k, c.plan.relMasks[0], got)
		}
	}
	s.SetWorkMem(defaultWorkMem)
	// And the answers agree whichever structure sorts: every window is that slice of the full sort.
	for _, key := range []string{"d DESC, id", "d NULLS FIRST, id DESC", "n, d DESC NULLS LAST, id"} {
		full, err := queryOutcome(s, "SELECT id, d, big, n FROM gt ORDER BY "+key, nil)
		if err != nil {
			t.Fatal(err)
		}
		all := strings.Split(laneRows(full, "ORDER BY"), "\n")
		for _, w := range [][2]int{{10, 0}, {7, 5}, {1, 3003}, {5000, 0}, {0, 0}} {
			sql := fmt.Sprintf("SELECT id, d, big, n FROM gt ORDER BY %s LIMIT %d OFFSET %d", key, w[0], w[1])
			out, err := queryOutcome(s, sql, nil)
			if err != nil {
				t.Fatalf("%q: %v", sql, err)
			}
			lo, hi := min(w[1], len(all)), min(w[1]+w[0], len(all))
			want := strings.Join(all[lo:hi], "\n")
			if got := laneRows(out, sql); got != want {
				t.Fatalf("%q: window differs from the full sort", sql)
			}
		}
	}
}
