package jed

// L2/L3 — defer inline values at fault (spec/design/lazy-record.md §12). On the demand-paged path
// every variable-length / structured present value (text/bytea/decimal/json/jsonb/composite/
// array/range) is loaded as a deferred Unfetched (Form 0x00) — a zero-copy slice of the shared page
// block (form (a), L3) — instead of being eagerly decoded; the scan layer resolves exactly the
// query's touched columns, an untouched one is dropped still deferred. The reshape is cost-,
// result-, and byte-neutral (§8) regardless of representation (form (a)/(b)), so a paged file and a
// fully-resident in-memory database must observe identical rows and identical cost for every query
// shape — that mode-identity is the leak-catcher (an unresolved deferral escapes the scan layer as
// a loud poison panic, never silent NULL). Mirrors impl/rust/tests/lazy_inline_values.rs and
// impl/ts/tests/lazy_inline_values.test.ts.

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

// inlineSeed builds the schema + rows exercising every deferrable type alongside a join partner
// and a secondary index. The default page size keeps every value inline-plain, so on a paged
// reopen each lands as an inline-deferred Unfetched — the L2 case (nothing spills).
func inlineSeed(t *testing.T, db dbHandle) {
	t.Helper()
	mustExec(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	mustExec(t, db, "CREATE TABLE t ("+
		"id i32 PRIMARY KEY, name text, data bytea, amount decimal(12,2), "+
		"doc jsonb, tags i32[], home addr, span i32range)")
	mustExec(t, db, "CREATE INDEX t_name ON t (name)")
	mustExec(t, db, "INSERT INTO t VALUES "+
		"(1, 'alice', '\\xdeadbeef', 100.50, '{\"k\": 1, \"tag\": \"x\"}', ARRAY[10, 20, 30], ROW('Main St', 90210), '[1,5)'), "+
		"(2, 'bob', '\\xcafe', 2.25, '{\"k\": 2}', ARRAY[1, NULL, 3], ROW('Oak Ave', 12345), '[10,20]'), "+
		"(3, 'carol', NULL, NULL, NULL, NULL, ROW('Elm', NULL), 'empty'), "+
		"(4, 'dave', '\\x00ff', 9999.99, '{\"k\": 4, \"nested\": {\"a\": [1,2,3]}}', '{}', ROW(NULL, 7), '(,9)')")
	mustExec(t, db, "CREATE TABLE u (id i32 PRIMARY KEY, t_id i32, note text)")
	mustExec(t, db, "INSERT INTO u VALUES (1, 1, 'first'), (2, 1, 'again'), (3, 3, 'lonely'), (4, 99, 'orphan')")
}

// rowsSorted runs sql and returns its rows rendered to strings and sorted — an order-insensitive
// multiset compare (a query without ORDER BY has unspecified order; sorting both sides is sound).
func rowsSorted(t *testing.T, db dbHandle, sql string) []string {
	t.Helper()
	rows := queryRows(t, db, sql)
	out := make([]string, len(rows))
	for i, r := range rows {
		cells := make([]string, len(r))
		for j, v := range r {
			cells[j] = v.Render()
		}
		out[i] = strings.Join(cells, "\x1f")
	}
	sort.Strings(out)
	return out
}

// TestLazyInlineValuesMatchResidentAcrossQueryShapes is the broad leak-catcher: a battery of query
// shapes touching the deferred columns through every read path (projection, filter, sort, DISTINCT,
// aggregate, join, subquery, correlated, index, window, CTE, container element/field access). For
// each, a paged reopen and an in-memory seed must agree on both rows and cost.
func TestLazyInlineValuesMatchResidentAcrossQueryShapes(t *testing.T) {
	path := filepath.Join(t.TempDir(), "l2_shapes.jed")
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	inlineSeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	mem := memDB().Session(SessionOptions{})
	inlineSeed(t, mem)
	paged, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()

	queries := []string{
		"SELECT * FROM t",
		"SELECT id FROM t",
		"SELECT name FROM t",
		"SELECT data FROM t",
		"SELECT amount FROM t",
		"SELECT doc FROM t",
		"SELECT tags FROM t",
		"SELECT home FROM t",
		"SELECT span FROM t",
		"SELECT id FROM t WHERE name = 'bob'",
		"SELECT id FROM t WHERE amount > 100",
		"SELECT id FROM t WHERE data = '\\xcafe'",
		"SELECT id FROM t WHERE name IS NULL",
		"SELECT id FROM t WHERE data IS NULL",
		"SELECT tags[1] FROM t",
		"SELECT (home).zip FROM t",
		"SELECT (home).street FROM t",
		"SELECT doc->>'k' FROM t",
		"SELECT id FROM t WHERE (doc->>'k') = '2'",
		"SELECT id FROM t WHERE lower(span) = 1",
		"SELECT name FROM t ORDER BY name",
		"SELECT id, name FROM t ORDER BY name DESC",
		"SELECT name, amount FROM t ORDER BY id",
		"SELECT DISTINCT name FROM t",
		"SELECT count(*), max(name), min(amount) FROM t",
		"SELECT amount, count(*) FROM t GROUP BY amount",
		"SELECT name FROM t GROUP BY name HAVING count(*) = 1",
		"SELECT name FROM t WHERE name = 'carol'",
		"SELECT id, name FROM t WHERE name > 'bob' ORDER BY name",
		"SELECT t.name, u.note FROM t JOIN u ON u.t_id = t.id",
		"SELECT t.name FROM t JOIN u ON u.t_id = t.id WHERE u.note = 'first'",
		"SELECT name FROM t WHERE id IN (SELECT t_id FROM u WHERE note = 'lonely')",
		"SELECT name FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)",
		"SELECT name FROM t WHERE id = (SELECT min(t_id) FROM u)",
		"SELECT name, row_number() OVER (ORDER BY id) FROM t",
		"SELECT name, count(*) OVER () FROM t",
		"WITH c AS (SELECT id, name FROM t) SELECT name FROM c WHERE id = 1",
		"WITH c AS (SELECT name, amount FROM t WHERE amount IS NOT NULL) SELECT name FROM c ORDER BY amount",
	}
	for _, sql := range queries {
		if want, got := rowsSorted(t, mem, sql), rowsSorted(t, paged, sql); !eqStrings(want, got) {
			t.Fatalf("rows differ (paged vs resident) for %q:\n want %v\n  got %v", sql, want, got)
		}
		if want, got := costOf(t, mem, sql), costOf(t, paged, sql); want != got {
			t.Fatalf("cost differs (paged vs resident) for %q: want %d got %d", sql, want, got)
		}
	}
}

// TestLazyInlineMutationsPreserveUntouchedValues: an UPDATE that touches only some columns must
// re-store the untouched deferred ones losslessly (the dirty leaf's other rows resolve at commit;
// the rewritten row's remaining references resolve as part of the rewrite — large-values.md §14,
// generalized to inline values). Applying the identical sequence to a resident database must reach
// the identical final state.
func TestLazyInlineMutationsPreserveUntouchedValues(t *testing.T) {
	path := filepath.Join(t.TempDir(), "l2_mutations.jed")
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	inlineSeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	mem := memDB().Session(SessionOptions{})
	inlineSeed(t, mem)

	mutations := []string{
		"UPDATE t SET amount = amount + 1 WHERE id = 1",
		"UPDATE t SET name = 'robert' WHERE id = 2",
		"UPDATE t SET tags = ARRAY[7, 8] WHERE id = 4",
		"DELETE FROM t WHERE id = 3",
		"INSERT INTO t VALUES (5, 'erin', '\\xab', 1.00, '{\"k\":5}', ARRAY[9], ROW('New', 1), '[2,3)')",
		"UPDATE u SET note = 'edited' WHERE t_id = 1",
	}
	for _, m := range mutations {
		mustExec(t, mem, m)
	}
	paged, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, m := range mutations {
		mustExec(t, paged, m)
	}
	if err := paged.Close(); err != nil {
		t.Fatal(err)
	}

	paged, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()
	for _, sql := range []string{
		"SELECT * FROM t",
		"SELECT id, name, amount, doc, tags, home, span, data FROM t ORDER BY id",
		"SELECT * FROM u",
	} {
		if want, got := rowsSorted(t, mem, sql), rowsSorted(t, paged, sql); !eqStrings(want, got) {
			t.Fatalf("final state differs for %q:\n want %v\n  got %v", sql, want, got)
		}
	}
}

// TestLazyUntouchedCorruptInlineBodyDefersError proves read-on-touch for inline values
// (lazy-record.md §8): with an inline text body corrupted to non-UTF-8 on disk (length prefix +
// page checksum kept valid), the skip-walk that finds the body's span still advances correctly — so
// open and untouching queries succeed — while touching the column runs the real decode and surfaces
// XX001. The inline analogue of TestLazyChainsAreReadOnlyWhenTouched.
func TestLazyUntouchedCorruptInlineBodyDefersError(t *testing.T) {
	path := filepath.Join(t.TempDir(), "l2_corrupt.jed")
	marker := "Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq" // 32 chars, no overlap with catalog text
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)")
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s', 42), (2, 'clean', 7)", marker))
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Corrupt the first content byte of the marker body to 0xFF (an invalid UTF-8 lead byte),
	// leaving the length prefix intact so the skip-walk advances identically, then repair the page
	// CRC so the corruption is checksum-valid (isolating the failure to decode time).
	ps := int(DefaultPageSize)
	bytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	at := -1
	needle := []byte(marker)
	for i := 0; i+len(needle) <= len(bytes); i++ {
		if string(bytes[i:i+len(needle)]) == marker {
			at = i
			break
		}
	}
	if at < 0 {
		t.Fatal("marker text body not found in the file")
	}
	pageIdx := at / ps
	if bytes[pageIdx*ps] != pageLeaf {
		t.Fatalf("marker not in a leaf page (page_type %d)", bytes[pageIdx*ps])
	}
	bytes[at] = 0xFF
	page := bytes[pageIdx*ps : (pageIdx+1)*ps]
	binary.BigEndian.PutUint32(page[12:16], pageCRC(page))
	if err := os.WriteFile(path, bytes, 0o644); err != nil {
		t.Fatal(err)
	}

	db, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if rows := queryRows(t, db, "SELECT id FROM t"); len(rows) != 2 {
		t.Fatalf("untouching SELECT id: got %d rows, want 2", len(rows))
	}
	if rows := queryRows(t, db, "SELECT id, n FROM t WHERE n = 42"); len(rows) != 1 || rows[0][0].Render() != "1" {
		t.Fatal("untouching filter on a fixed-width column must succeed")
	}
	if rows := queryRows(t, db, "SELECT body FROM t WHERE id = 2"); rows[0][0].Render() != "clean" {
		t.Fatal("the clean row's body must resolve")
	}
	for _, sql := range []string{"SELECT body FROM t WHERE id = 1", "SELECT * FROM t ORDER BY id"} {
		_, err := db.Execute(sql, nil)
		if err == nil {
			t.Fatalf("touching the corrupted body must fail: %q", sql)
		}
		if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
			t.Fatalf("%q: want XX001, got %v", sql, err)
		}
	}
}

// TestLazyUntouchedDeferredColumnRidesSpillingSort: an unreferenced deferred column riding a
// spilling sort (spill.md §4) must round-trip opaquely through the spill run file (the spill codec's
// inline pass-through, tag 21). Under a tiny work_mem the sort spills many runs; the result must
// still equal the in-memory sort.
func TestLazyUntouchedDeferredColumnRidesSpillingSort(t *testing.T) {
	path := filepath.Join(t.TempDir(), "l2_spill.jed")
	mem := memDB().Session(SessionOptions{})
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range []dbHandle{mem, db} {
		mustExec(t, e, "CREATE TABLE t (id i32 PRIMARY KEY, k i32, label text, doc jsonb)")
	}
	for id := 0; id < 200; id++ {
		k := (id * 48271) % 100
		row := fmt.Sprintf("INSERT INTO t VALUES (%d, %d, 'label-%d-xxxxxxxxxx', '{\"id\": %d}')", id, k, id, id)
		mustExec(t, mem, row)
		mustExec(t, db, row)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	paged, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()
	paged.SetWorkMem(128) // ~2-3 rows per run → dozens of spilled runs + a deep k-way merge

	for _, sql := range []string{
		"SELECT id FROM t ORDER BY k, id",
		"SELECT id, k FROM t ORDER BY k DESC, id DESC",
		"SELECT id FROM t ORDER BY k, id LIMIT 13 OFFSET 9",
		"SELECT label FROM t ORDER BY k, id LIMIT 5",
	} {
		if want, got := rowsSorted(t, mem, sql), rowsSorted(t, paged, sql); !eqStrings(want, got) {
			t.Fatalf("spilling sort with deferred carried columns differs for %q", sql)
		}
		if want, got := costOf(t, mem, sql), costOf(t, paged, sql); want != got {
			t.Fatalf("cost differs for %q: want %d got %d", sql, want, got)
		}
	}
}

// TestLazyInlineFaultedLeafSharesPageBlock — L3, form (a) zero-copy block-shared deferral
// (spec/design/lazy-record.md §5a/§12). A faulted leaf's deferred inline values are SLICES of the
// one shared page block, never per-value copies (form (b), L2). This is the resident-memory
// dividend (§9): resident leaf bytes track ≈ pageSize, not the sum of the decoded values. The
// property is invisible to results and cost (§8), so it is asserted white-box: a Go sub-slice's cap
// reaches the end of its backing array, so cap(view) > len(view), whereas a make()+copy body has
// cap == len. Mirrors the Rust faulted_leaf_shares_one_block_across_deferred_values and the TS
// equivalent.
func TestLazyInlineFaultedLeafSharesPageBlock(t *testing.T) {
	i32 := scalarColType(scalarInt32)
	// Variable-length / structured columns so every present value defers (§6); the i32 column stays
	// eagerly decoded (deferring a fixed-width scalar buys nothing).
	colTypes := []colType{
		i32,
		scalarColType(scalarText),
		scalarColType(scalarBytea),
		scalarColType(scalarDecimal),
		{Elem: &i32}, // i32[]
	}
	const ps = 8192 // large page → every value stays inline-plain (no spill)
	capacity := ps - pageHeader

	rows := make([]storedRow, 3)
	for i := range rows {
		rows[i] = storedRow{
			IntValue(int64(i)),
			TextValue(fmt.Sprintf("name-%d-padding-padding", i)),
			ByteaValue([]byte{byte(i), byte(i), byte(i), byte(i)}),
			DecimalValue(decimalFromDigitsScale(false, "12345", 2)),
			arrayValueOf(oneDimArray([]Value{IntValue(int64(i)), IntValue(int64(i + 1))})),
		}
	}

	// Encode the records into one PAX leaf page payload (everything inline at this page size).
	takeSeq := uint32(100)
	take := func() uint32 { takeSeq++; return takeSeq }
	var ovf []overflowPageOut
	keys := make([][]byte, len(rows))
	for i := range rows {
		key := make([]byte, 4)
		binary.BigEndian.PutUint32(key, uint32(i))
		keys[i] = key
	}
	payload := encodeLeafPAX(colTypes, keys, rows, capacity, take, &ovf)
	if len(ovf) != 0 {
		t.Fatalf("values must stay inline (no overflow) for the form-(a) case, got %d overflow pages", len(ovf))
	}
	block := makePage(ps, pageLeaf, uint32(len(rows)), 0, payload)

	// Fault the leaf → Packed form (packed-leaf.md §5): the block + PAX directories are retained and
	// NO value is decoded (the decoded row vector is empty), so rows are reconstructed on demand.
	// Reconstruction produces the same inline-deferred Unfetched (form (a)) the eager fault used to.
	node, err := decodeLeafNode(block, 2, colTypes, nil)
	if err != nil {
		t.Fatalf("decodeLeafNode: %v", err)
	}
	if node.packed == nil {
		t.Fatalf("a faulted leaf is Packed (packed-leaf.md §5)")
	}
	if len(node.vals) != 0 {
		t.Fatalf("a Packed leaf holds no decoded row vector (resident ≈ pageSize, §9); got %d", len(node.vals))
	}

	deferred := 0
	for ri := 0; ri < len(rows); ri++ {
		row, err := node.rowAt(ri)
		if err != nil {
			t.Fatalf("rowAt %d: %v", ri, err)
		}
		for ci, v := range row {
			if v.Kind != ValUnfetched || v.unfetched().Form != 0x00 {
				continue
			}
			deferred++
			comp := v.unfetched().Comp
			// Form (a): the body is a SLICE of the page block, so its cap reaches the page's end
			// (zero-fill tail) and exceeds its len. A form-(b) copy (make+copy) has cap == len.
			if cap(comp) <= len(comp) {
				t.Fatalf("row %d col %d: deferred body is a copy (cap %d == len %d), not a block view (form (a))",
					ri, ci, cap(comp), len(comp))
			}
			// It still resolves to exactly the eager value (form (a) is decode-neutral).
			got, err := resolveUnfetched(colTypes[ci], v.unfetched(), func(uint32) ([]byte, error) {
				return nil, fmt.Errorf("inline values read no overflow pages")
			})
			if err != nil {
				t.Fatalf("row %d col %d: resolve: %v", ri, ci, err)
			}
			if !bytes.Equal(encodeValue(colTypes[ci], got), encodeValue(colTypes[ci], rows[ri][ci])) {
				t.Fatalf("row %d col %d: resolved value differs from the eager value", ri, ci)
			}
		}
	}
	// 3 rows × 4 deferrable columns (text/bytea/decimal/array) = 12 deferred values; the i32 column
	// stays eager (§6).
	if deferred != 12 {
		t.Fatalf("expected 12 deferred values, got %d", deferred)
	}
}

// TestPackedLeafTouchedColumnsAndSelfResolution — the touched-column path (packed-leaf.md §4/§6,
// the PAX dividend): colAt reconstructs ONLY the requested column of a Packed leaf,
// byte-identically to the whole-row reconstruction — plus the B4 demand-fault backstop
// (bplus-reshape.md §5): a deferred value carries its own resolution handles, so
// resolveUnfetchedSelf reconstructs it with NO caller-supplied type or pager — the path the
// evaluator's column access takes when the static touched set missed. Mirrors the Rust
// packed_leaf_reconstructs_only_touched_columns.
func TestPackedLeafTouchedColumnsAndSelfResolution(t *testing.T) {
	colTypes := []colType{
		scalarColType(scalarInt32),
		scalarColType(scalarText),
		scalarColType(scalarInt64),
	}
	const ps = 8192
	capacity := ps - pageHeader
	rows := make([]storedRow, 4)
	for i := range rows {
		rows[i] = storedRow{IntValue(int64(i)), TextValue(fmt.Sprintf("row-%d", i)), IntValue(int64(i) * 1000)}
	}
	takeSeq := uint32(100)
	take := func() uint32 { takeSeq++; return takeSeq }
	var ovf []overflowPageOut
	keys := make([][]byte, len(rows))
	for i := range rows {
		key := make([]byte, 4)
		binary.BigEndian.PutUint32(key, uint32(i))
		keys[i] = key
	}
	payload := encodeLeafPAX(colTypes, keys, rows, capacity, take, &ovf)
	if len(ovf) != 0 {
		t.Fatalf("values must stay inline, got %d overflow pages", len(ovf))
	}
	block := makePage(ps, pageLeaf, uint32(len(rows)), 0, payload)
	node, err := decodeLeafNode(block, 2, colTypes, nil)
	if err != nil {
		t.Fatalf("decodeLeafNode: %v", err)
	}

	// resolve a (possibly-deferred) value to its comparable eager bytes.
	resolve := func(v Value, c int) []byte {
		if v.Kind == ValUnfetched {
			got, err := resolveUnfetched(colTypes[c], v.unfetched(), func(uint32) ([]byte, error) {
				return nil, fmt.Errorf("inline values read no overflow pages")
			})
			if err != nil {
				t.Fatalf("resolve: %v", err)
			}
			v = got
		}
		return encodeValue(colTypes[c], v)
	}

	for i := range rows {
		whole, err := node.rowAt(i)
		if err != nil {
			t.Fatalf("rowAt %d: %v", i, err)
		}
		// colAt(c) equals the whole row's column c.
		for c := range colTypes {
			one, err := node.colAt(i, c)
			if err != nil {
				t.Fatalf("colAt(%d,%d): %v", i, c, err)
			}
			if !bytes.Equal(resolve(one, c), resolve(whole[c], c)) {
				t.Fatalf("row %d col %d: colAt differs from whole row", i, c)
			}
		}
		// The B4 demand-fault backstop: a deferred value carries its own resolution handles, so
		// resolveUnfetchedSelf reconstructs it with NO caller-supplied type or pager.
		for c := range colTypes {
			if whole[c].Kind != ValUnfetched {
				continue
			}
			got, err := resolveUnfetchedSelf(whole[c].unfetched())
			if err != nil {
				t.Fatalf("row %d col %d: self-resolve: %v", i, c, err)
			}
			if !bytes.Equal(encodeValue(colTypes[c], got), resolve(whole[c], c)) {
				t.Fatalf("row %d col %d: self-resolution differs from context resolution", i, c)
			}
		}
	}
}
