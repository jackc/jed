package jed

// A1 — touched-column scan wiring (packed-leaf.md §4; the PAX read-path dividend). A file-backed
// scan reconstructs only the query's touched columns (relMasks), leaving untouched columns NULL on
// the Packed leaf, instead of decoding the whole row. This is byte/result/cost-neutral IFF the mask
// is a complete superset of every column any consumer reads — an invariant that was already
// load-bearing for deferred VARIABLE-LENGTH values (an untouched unfetched value poisons if read,
// lazy_inline_values_test.go) but is NEWLY load-bearing for FIXED-WIDTH columns (previously always
// decoded, so a mask gap was harmless). This battery actively exercises that: a WIDE ALL-FIXED-WIDTH
// table, and a spread of query shapes each touching a different column subset, where a paged reopen
// (masked reconstruction) and a fully-resident in-memory database (whole rows) must agree on both
// rows and cost. A mask gap surfaces as a divergence here, never a silent wrong answer.

import (
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

// wideFixedSeed builds a wide all-fixed-width table (i16/i32/i64, several nullable) plus a join
// partner. Every column is fixed-width, so on a paged reopen the leaf is Packed with no deferred
// values — the case rowAtMasked skips whole-column decodes that rowAt would have done.
func wideFixedSeed(t *testing.T, db dbHandle) {
	t.Helper()
	mustExec(t, db, "CREATE TABLE w ("+
		"id i32 PRIMARY KEY, c0 i16, c1 i32, c2 i64, c3 i32, c4 i16, c5 i64, c6 i32, c7 i32)")
	mustExec(t, db, "INSERT INTO w VALUES "+
		"(1, 10, 100, 1000, 7, 3, 500, 42, 9), "+
		"(2, 20, 100, 2000, 7, NULL, 600, 43, 8), "+
		"(3, 10, 300, 3000, 8, 5, NULL, 44, 7), "+
		"(4, 20, 100, 4000, 8, 6, 800, NULL, 6), "+
		"(5, 10, 500, 5000, 9, NULL, 900, 46, 5)")
	mustExec(t, db, "CREATE INDEX w_c3 ON w (c3)")
	mustExec(t, db, "CREATE TABLE w2 (id i32 PRIMARY KEY, k i32, note i32)")
	mustExec(t, db, "INSERT INTO w2 VALUES (1, 7, 71), (2, 8, 82), (3, 7, 73), (5, 9, 95)")
}

func TestMaskedWideFixedWidthMatchesResident(t *testing.T) {
	path := filepath.Join(t.TempDir(), "masked_wide_fixed.jed")
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	wideFixedSeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	mem := NewDatabase().Session(SessionOptions{})
	wideFixedSeed(t, mem)
	paged, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()

	// Each query touches a different column subset. If masked reconstruction wrongly NULLed a needed
	// column, the paged rows/cost would diverge from the resident whole-row path.
	queries := []string{
		// Whole-row and single/multi-column projections (the projection-scan feed).
		"SELECT * FROM w",
		"SELECT c0 FROM w",
		"SELECT c3, c7 FROM w",
		"SELECT id, c5 FROM w",
		// WHERE on one column, project another (touched set spans filter + projection).
		"SELECT c1 FROM w WHERE c0 > 15",
		"SELECT id FROM w WHERE c7 < 8",
		"SELECT c6 FROM w WHERE c4 IS NULL",
		"SELECT c2 FROM w WHERE c5 IS NOT NULL",
		// Vectorized aggregates (the columnar-gather fast-path feed, A2) touching one operand column.
		// Filter-free shapes take the A2 columnar path on the paged database (paging != nil) and the
		// row path on the resident one — they must agree on rows AND cost.
		"SELECT count(*) FROM w",
		"SELECT sum(c2) FROM w",            // SUM(i64) is not a vectorized kernel — takes the row path both ways
		"SELECT sum(c1) FROM w",            // SUM(i32) → planSumInt columnar
		"SELECT sum(c3), count(c6) FROM w", // multi-spec whole-table columnar (planSumInt + planCount)
		"SELECT count(c4) FROM w",          // COUNT over a nullable operand — the columnar null lane
		"SELECT min(c5), max(c6) FROM w",
		"SELECT sum(c0) FROM w WHERE c1 = 100", // filtered → row path (columnar declines on plan.filter)
		// Single-integer-key GROUP BY (touched: the key + the operand).
		"SELECT c0, sum(c2) FROM w GROUP BY c0",            // SUM(i64) key case → row path
		"SELECT c0, sum(c1) FROM w GROUP BY c0",            // SUM(i32) → columnar group-by
		"SELECT c0, sum(c1), count(c4) FROM w GROUP BY c0", // grouped multi-spec, nullable operand
		"SELECT c3, count(*) FROM w GROUP BY c3",
		// ORDER BY satisfied by the PK scan (top-N streaming) and by a sort (non-PK).
		"SELECT c1 FROM w ORDER BY id",
		"SELECT c1 FROM w ORDER BY id LIMIT 3",
		"SELECT c6 FROM w ORDER BY c6 DESC",
		"SELECT id, c0 FROM w ORDER BY c0, id",
		// DISTINCT.
		"SELECT DISTINCT c0 FROM w",
		"SELECT DISTINCT c3, c0 FROM w",
		// PK point + range bounds (the RangeScanWithUnits masked feed).
		"SELECT c4 FROM w WHERE id = 2",
		"SELECT c2, c6 FROM w WHERE id >= 3",
		// Secondary-index bound (indexBoundRows — whole-row, must still agree).
		"SELECT c0 FROM w WHERE c3 = 7",
		// Join (each rel materialized under its own mask).
		"SELECT w.c0, w2.note FROM w JOIN w2 ON w2.id = w.id",
		"SELECT w.c1 FROM w JOIN w2 ON w2.k = w.c3 WHERE w2.note > 72",
		// Subquery / IN (the inner and outer each touch distinct columns).
		"SELECT c0 FROM w WHERE id IN (SELECT id FROM w2 WHERE k = 7)",
		"SELECT c7 FROM w WHERE EXISTS (SELECT 1 FROM w2 WHERE w2.id = w.id AND w2.note > 80)",
	}
	for _, sql := range queries {
		if want, got := rowsSorted(t, mem, sql), rowsSorted(t, paged, sql); !eqStrings(want, got) {
			t.Fatalf("rows differ (paged-masked vs resident) for %q:\n want %v\n  got %v", sql, want, got)
		}
		if want, got := costOf(t, mem, sql), costOf(t, paged, sql); want != got {
			t.Fatalf("cost differs (paged-masked vs resident) for %q: want %d got %d", sql, want, got)
		}
	}
}

// TestMaskedColumnarMultiLevelMatchesResident forces a MULTI-LEVEL B-tree (enough rows that the tree
// splits past a single leaf into a root interior node carrying separator entries) so the A2 columnar
// gather's interior-separator path — a B-tree stores records in interior nodes too, gathered alongside
// the leaves — is exercised against the in-memory row oracle. The single-leaf wideFixedSeed table above
// never builds an interior node, so its columnar walk only visits leaves. Both databases use the
// DEFAULT page size so their tree shapes (hence the page_read node counts) are identical; the depth
// comes from the row count, not a shrunk page. Filter-free aggregates take the A2 columnar path on the
// paged (file-backed) database and the row path on the resident one; both the result rows AND the
// deterministic cost must agree. A few filtered / grouped-with-WHERE shapes exercise the row path over
// the same multi-level tree for good measure.
func TestMaskedColumnarMultiLevelMatchesResident(t *testing.T) {
	seed := func(t *testing.T, db dbHandle) {
		t.Helper()
		mustExec(t, db, "CREATE TABLE m (id i32 PRIMARY KEY, k i32, a i32, b i16)")
		const rows = 5000
		const chunk = 1000
		for start := 0; start < rows; start += chunk {
			var sb strings.Builder
			sb.WriteString("INSERT INTO m VALUES ")
			for i := start; i < start+chunk && i < rows; i++ {
				if i > start {
					sb.WriteByte(',')
				}
				// k has 8 recurring group buckets that span leaves; a stays small so the grouped/whole
				// SUM fits i64; b is NULL on every 7th row (the columnar COUNT null lane over interior +
				// leaf entries).
				bexpr := fmt.Sprintf("%d", i%100)
				if i%7 == 0 {
					bexpr = "NULL"
				}
				fmt.Fprintf(&sb, "(%d,%d,%d,%s)", i, i%8, i%1000, bexpr)
			}
			mustExec(t, db, sb.String())
		}
	}
	path := filepath.Join(t.TempDir(), "masked_multilevel.jed")
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	seed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	mem := NewDatabase().Session(SessionOptions{})
	seed(t, mem)
	paged, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()

	queries := []string{
		// Filter-free aggregates — the A2 columnar path on `paged` (interior separators + leaves gathered).
		"SELECT count(*) FROM m",
		"SELECT sum(a) FROM m",
		"SELECT sum(a), count(b) FROM m",
		"SELECT min(a), max(a) FROM m",
		"SELECT count(b) FROM m",
		"SELECT k, count(*) FROM m GROUP BY k",
		"SELECT k, sum(a) FROM m GROUP BY k",
		"SELECT k, sum(a), count(b) FROM m GROUP BY k",
		// Filtered / bounded — the row path over the same multi-level tree (columnar declines on filter).
		"SELECT sum(a) FROM m WHERE id >= 100 AND id < 400",
		"SELECT k, count(*) FROM m WHERE k >= 3 GROUP BY k",
	}
	for _, sql := range queries {
		if want, got := rowsSorted(t, mem, sql), rowsSorted(t, paged, sql); !eqStrings(want, got) {
			t.Fatalf("rows differ (paged-columnar vs resident) for %q:\n want %v\n  got %v", sql, want, got)
		}
		if want, got := costOf(t, mem, sql), costOf(t, paged, sql); want != got {
			t.Fatalf("cost differs (paged-columnar vs resident) for %q: want %d got %d", sql, want, got)
		}
	}
}
