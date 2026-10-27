package jed

// The integrity check's host surface and cross-core contract (spec/design/integrity-check.md §6): the
// shared damaged fixtures reproduce their expected reports exactly, every golden that opens checks
// clean at both levels, every emitted kind is registered, and the host API behaves (42704, 57014,
// truncation, a check beside an open writer, live-only damage).

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"testing"
)

// integrityFixtureRun is one configuration of a fixture's expected reports.
type integrityFixtureRun struct {
	Level       string `json:"level"`
	MaxFindings int    `json:"max_findings"`
	Report      struct {
		Findings []struct {
			Kind     string  `json:"kind"`
			Database string  `json:"database"`
			Object   *string `json:"object"`
			Page     *uint32 `json:"page"`
			Key      *string `json:"key"`
		} `json:"findings"`
		Truncated bool `json:"truncated"`
		Skipped   []struct {
			Object string `json:"object"`
			Reason string `json:"reason"`
		} `json:"skipped"`
		Pages uint64 `json:"pages"`
		Rows  uint64 `json:"rows"`
	} `json:"report"`
}

// integrityContract renders a report's contract part (everything but detail) as JSON-shaped text.
func integrityContract(r IntegrityReport) string {
	type finding struct {
		Kind     string  `json:"kind"`
		Database string  `json:"database"`
		Object   *string `json:"object"`
		Page     *uint32 `json:"page"`
		Key      *string `json:"key"`
	}
	type skip struct {
		Object string `json:"object"`
		Reason string `json:"reason"`
	}
	out := struct {
		Findings  []finding `json:"findings"`
		Truncated bool      `json:"truncated"`
		Skipped   []skip    `json:"skipped"`
		Pages     uint64    `json:"pages"`
		Rows      uint64    `json:"rows"`
	}{Findings: []finding{}, Truncated: r.Truncated, Skipped: []skip{}, Pages: r.Pages, Rows: r.Rows}
	for _, f := range r.Findings {
		g := finding{Kind: f.Kind, Database: f.Database, Page: f.Page}
		if f.Object != "" {
			o := f.Object
			g.Object = &o
		}
		if f.Key != nil {
			h := hex.EncodeToString(f.Key)
			g.Key = &h
		}
		out.Findings = append(out.Findings, g)
	}
	for _, s := range r.Skipped {
		out.Skipped = append(out.Skipped, skip(s))
	}
	b, _ := json.MarshalIndent(out, "", "  ")
	return string(b)
}

// openIntegrityCopy opens a copy of the file at src read-only, as the CLI would (§7).
func openIntegrityCopy(t *testing.T, src string, cred *Credential) (*Database, error) {
	t.Helper()
	data, err := os.ReadFile(src)
	if err != nil {
		t.Fatalf("read %s: %v", src, err)
	}
	path := filepath.Join(t.TempDir(), filepath.Base(src))
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
	return OpenDatabaseWithOptions(path, OpenOptions{ReadOnly: true, Environment: goldenEnv(), Credential: cred, Locking: LockingExclusive})
}

func TestIntegrityFixtures(t *testing.T) {
	dir := specPath(t, "fileformat/integrity")
	files, err := filepath.Glob(filepath.Join(dir, "*.json"))
	if err != nil || len(files) == 0 {
		t.Fatalf("no integrity fixtures in %s (%v)", dir, err)
	}
	for _, file := range files {
		name := strings.TrimSuffix(filepath.Base(file), ".json")
		t.Run(name, func(t *testing.T) {
			raw, err := os.ReadFile(file)
			if err != nil {
				t.Fatal(err)
			}
			var spec struct {
				Fixture string                `json:"fixture"`
				Runs    []integrityFixtureRun `json:"runs"`
			}
			if err := json.Unmarshal(raw, &spec); err != nil {
				t.Fatal(err)
			}
			db, err := openIntegrityCopy(t, filepath.Join(dir, spec.Fixture), nil)
			if err != nil {
				t.Fatalf("open %s: %v", spec.Fixture, err)
			}
			defer db.Close()
			for _, run := range spec.Runs {
				opts := IntegrityCheckOptions{MaxFindings: run.MaxFindings}
				if run.MaxFindings == 0 {
					opts.MaxFindings = IntegrityUnlimited
				}
				if run.Level == "quick" {
					opts.Level = IntegrityQuick
				}
				got, err := db.IntegrityCheck(context.Background(), "main", opts)
				if err != nil {
					t.Fatalf("%s/%d: %v", run.Level, run.MaxFindings, err)
				}
				var want IntegrityReport
				want.Truncated, want.Pages, want.Rows = run.Report.Truncated, run.Report.Pages, run.Report.Rows
				for _, f := range run.Report.Findings {
					g := IntegrityFinding{Kind: f.Kind, Database: f.Database, Page: f.Page}
					if f.Object != nil {
						g.Object = *f.Object
					}
					if f.Key != nil {
						g.Key, _ = hex.DecodeString(*f.Key)
					}
					want.Findings = append(want.Findings, g)
				}
				for _, s := range run.Report.Skipped {
					want.Skipped = append(want.Skipped, IntegritySkip{Object: s.Object, Reason: s.Reason})
				}
				if g, w := integrityContract(got), integrityContract(want); g != w {
					t.Errorf("%s/%d:\n got: %s\nwant: %s", run.Level, run.MaxFindings, g, w)
				}
			}
		})
	}
}

// Every golden that opens checks clean at both levels, except the deliberately misordered
// collation_skew_corrupt.jed. The goldens the existing tests reject (malformed_*, the cow_* files no
// generation of which opens) fail to open here too and are skipped.
func TestIntegrityGoldensClean(t *testing.T) {
	dir := specPath(t, "fileformat/fixtures")
	files, err := filepath.Glob(filepath.Join(dir, "*.jed"))
	if err != nil || len(files) == 0 {
		t.Fatalf("no goldens in %s (%v)", dir, err)
	}
	var skipped []string
	checked := 0
	for _, file := range files {
		name := filepath.Base(file)
		// collation_skew_corrupt's index is deliberately ordered for another collation version; the
		// malformed_* goldens are deliberately damaged (each must report a finding instead).
		damaged := strings.HasPrefix(name, "malformed_")
		if name == "collation_skew_corrupt.jed" {
			continue
		}
		var cred *Credential
		if strings.HasPrefix(name, "encrypted_") {
			cred = encGoldenKey()
		}
		db, err := openIntegrityCopy(t, file, cred)
		if err != nil {
			skipped = append(skipped, name)
			continue
		}
		for _, level := range []IntegrityLevel{IntegrityQuick, IntegrityFull} {
			report, err := db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{Level: level, MaxFindings: IntegrityUnlimited})
			if err != nil {
				t.Errorf("%s (level %d): %v", name, level, err)
			} else if damaged {
				if report.OK() {
					t.Errorf("%s (level %d): a malformed golden checks clean", name, level)
				}
			} else if !report.OK() || len(report.Skipped) != 0 {
				t.Errorf("%s (level %d): %+v", name, level, report.Findings)
			}
		}
		checked++
		db.Close()
	}
	for _, name := range skipped {
		if !strings.HasPrefix(name, "malformed_") && !strings.HasPrefix(name, "cow_") {
			t.Errorf("golden %s did not open", name)
		}
	}
	if checked < 80 {
		t.Errorf("only %d goldens checked", checked)
	}
	t.Logf("checked %d goldens; did not open: %v", checked, skipped)
}

// Every kind and skip reason the check emits is registered (spec/integrity/findings.toml), and every
// registered kind the check can emit in this slice is one of the generated constants.
func TestIntegrityKindsRegistered(t *testing.T) {
	src, err := os.ReadFile("integrity.go")
	if err != nil {
		t.Fatal(err)
	}
	text := string(src)
	consts := map[string]string{
		IntegrityMetaInvalid: "IntegrityMetaInvalid", IntegrityManifestPageMismatch: "IntegrityManifestPageMismatch",
		IntegrityCatalogPageCorrupt: "IntegrityCatalogPageCorrupt", IntegrityCatalogMismatch: "IntegrityCatalogMismatch",
		IntegritySequenceOutOfRange: "IntegritySequenceOutOfRange", IntegrityPageOutOfRange: "IntegrityPageOutOfRange",
		IntegrityPageReachedTwice: "IntegrityPageReachedTwice", IntegrityPageCorrupt: "IntegrityPageCorrupt",
		IntegrityPageTypeWrong: "IntegrityPageTypeWrong", IntegrityTreeDepthUneven: "IntegrityTreeDepthUneven",
		IntegrityKeysUnordered: "IntegrityKeysUnordered", IntegritySeparatorViolation: "IntegritySeparatorViolation",
		IntegrityRowKeyNotPrimaryKey: "IntegrityRowKeyNotPrimaryKey", IntegrityValueUndecodable: "IntegrityValueUndecodable",
		IntegrityOverflowChainCorrupt: "IntegrityOverflowChainCorrupt", IntegrityRowCountMismatch: "IntegrityRowCountMismatch",
		IntegrityLiveCountMismatch: "IntegrityLiveCountMismatch", IntegrityFreeListCorrupt: "IntegrityFreeListCorrupt",
		IntegrityFreePageOutOfRange: "IntegrityFreePageOutOfRange", IntegrityFreePageDuplicate: "IntegrityFreePageDuplicate",
		IntegrityFreePageLive: "IntegrityFreePageLive", IntegrityFreePageManifestDependency: "IntegrityFreePageManifestDependency",
		IntegrityIndexEntryMissing: "IntegrityIndexEntryMissing", IntegrityIndexEntryCountMismatch: "IntegrityIndexEntryCountMismatch",
		IntegrityIndexEntryError:     "IntegrityIndexEntryError",
		IntegritySkipTableUnreadable: "IntegritySkipTableUnreadable", IntegritySkipIndexUnreadable: "IntegritySkipIndexUnreadable",
	}
	for value, ident := range consts {
		if !strings.Contains(text, ident) {
			t.Errorf("integrity.go does not emit %s", ident)
		}
		if !slices.Contains(integrityFindingKinds, value) && !slices.Contains(integritySkipReasons, value) {
			t.Errorf("%s (%q) is not registered", ident, value)
		}
	}
	// No kind or reason is spelled as a literal: every one goes through a generated constant.
	for _, kind := range append(append([]string(nil), integrityFindingKinds...), integritySkipReasons...) {
		if strings.Contains(text, `"`+kind+`"`) {
			t.Errorf("integrity.go spells %q as a literal", kind)
		}
	}
	// Every Integrity* identifier integrity.go references is one checked above (or the API).
	api := map[string]bool{
		"Integrity": true, "IntegrityLevel": true, "IntegrityFull": true, "IntegrityQuick": true, "IntegrityUnlimited": true,
		"IntegrityCheckOptions": true, "IntegrityFinding": true, "IntegritySkip": true, "IntegrityReport": true, "IntegrityCheck": true,
	}
	known := map[string]bool{}
	for _, ident := range consts {
		known[ident] = true
	}
	var unknown []string
	for _, word := range strings.FieldsFunc(text, func(r rune) bool {
		return !(r == '_' || r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9')
	}) {
		if strings.HasPrefix(word, "Integrity") && !api[word] && !known[word] && !slices.Contains(unknown, word) {
			unknown = append(unknown, word)
		}
	}
	sort.Strings(unknown)
	if len(unknown) > 0 {
		t.Errorf("integrity.go references unchecked identifiers %v", unknown)
	}
}

// integrityTestDB creates a file database with a table and an index spanning several pages.
func integrityTestDB(t *testing.T) (*Database, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "check.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 512, SkipFsync: true, Locking: LockingExclusive})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	for _, sql := range []string{
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32, note text)",
		"CREATE INDEX t_v ON t (v)",
		"INSERT INTO t SELECT g, g % 7, repeat('x', g % 50) FROM generate_series(1, 300) g",
	} {
		if _, err := s.Exec(context.Background(), sql); err != nil {
			t.Fatalf("%s: %v", sql, err)
		}
	}
	return db, path
}

func integrityCode(err error) string {
	var ee *EngineError
	if errors.As(err, &ee) {
		return ee.Code()
	}
	return ""
}

func TestIntegrityHostSurface(t *testing.T) {
	db, _ := integrityTestDB(t)
	defer db.Close()
	ctx := context.Background()

	report, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{})
	if err != nil || !report.OK() {
		t.Fatalf("clean check: %v %+v", err, report.Findings)
	}
	if report.Rows != 300 || report.Pages == 0 {
		t.Errorf("counters: pages %d rows %d", report.Pages, report.Rows)
	}

	for _, name := range []string{"nope", "temp", "MAIN"} {
		if _, err := db.IntegrityCheck(ctx, name, IntegrityCheckOptions{}); integrityCode(err) != "42704" {
			t.Errorf("%s: want 42704, got %v", name, err)
		}
	}

	canceled, cancel := context.WithCancel(ctx)
	cancel()
	if _, err := db.IntegrityCheck(canceled, "main", IntegrityCheckOptions{}); integrityCode(err) != "57014" {
		t.Errorf("canceled: want 57014, got %v", err)
	}

	// A check beside an open write transaction neither waits for it nor sees its writes.
	w := db.WriteSession()
	if _, err := w.Exec(ctx, "INSERT INTO t VALUES (1000, 1, 'pending')"); err != nil {
		t.Fatal(err)
	}
	report, err = db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{})
	if err != nil || !report.OK() || report.Rows != 300 {
		t.Fatalf("beside a writer: %v %d %+v", err, report.Rows, report.Findings)
	}
	if err := w.Commit(); err != nil {
		t.Fatal(err)
	}
	w.Close()
	report, err = db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{Level: IntegrityQuick})
	if err != nil || !report.OK() || report.Rows != 301 {
		t.Fatalf("after the commit: %v %d %+v", err, report.Rows, report.Findings)
	}

	// An in-memory attachment checks too.
	if err := db.Attach("scratch", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	if _, err := s.Exec(ctx, "CREATE TABLE scratch.u (id i64 PRIMARY KEY, v text)"); err != nil {
		t.Fatal(err)
	}
	if _, err := s.Exec(ctx, "INSERT INTO scratch.u SELECT g, repeat('y', 40) FROM generate_series(1, 100) g"); err != nil {
		t.Fatal(err)
	}
	report, err = db.IntegrityCheck(ctx, "scratch", IntegrityCheckOptions{})
	if err != nil || !report.OK() || report.Rows != 100 {
		t.Fatalf("in-memory attachment: %v %d %+v", err, report.Rows, report.Findings)
	}
}

func TestIntegrityInMemory(t *testing.T) {
	db := newInMemoryWithPageSize(512, NewEnvironment())
	ctx := context.Background()
	report, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{})
	if err != nil || !report.OK() {
		t.Fatalf("empty: %v %+v", err, report.Findings)
	}
	s := db.Session(SessionOptions{})
	for _, sql := range []string{
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"CREATE INDEX t_v ON t (v)",
		"INSERT INTO t SELECT g, g % 5 FROM generate_series(1, 400) g",
		"DELETE FROM t WHERE id % 3 = 0",
	} {
		if _, err := s.Exec(ctx, sql); err != nil {
			t.Fatalf("%s: %v", sql, err)
		}
	}
	report, err = db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{})
	if err != nil || !report.OK() || report.Rows != 267 {
		t.Fatalf("in memory: %v %d %+v", err, report.Rows, report.Findings)
	}
}

// damagePage flips one byte of page in the file at path, under the open handle.
func damagePage(t *testing.T, path string, pageSize int, page uint32, offset int) {
	t.Helper()
	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	at := int64(page)*int64(pageSize) + int64(offset)
	b := make([]byte, 1)
	if _, err := f.ReadAt(b, at); err != nil {
		t.Fatal(err)
	}
	b[0] ^= 0x01
	if _, err := f.WriteAt(b, at); err != nil {
		t.Fatal(err)
	}
}

func integrityKinds(r IntegrityReport) []string {
	var out []string
	for _, f := range r.Findings {
		out = append(out, f.Kind)
	}
	return out
}

// Damage after open that only a live handle sees (fresh = false kinds, §4): the selected meta slot,
// a page the commit manifest lists, the catalog. Also max_findings truncation.
func TestIntegrityLiveDamage(t *testing.T) {
	ctx := context.Background()

	t.Run("meta", func(t *testing.T) {
		db, path := integrityTestDB(t)
		defer db.Close()
		pin, err := db.core.integrityPin(ctx, "main", nil)
		if err != nil {
			t.Fatal(err)
		}
		slot := uint32(2)
		for s := uint32(0); s < 2; s++ {
			if string(pin.slotRaw[s]) == string(pin.metaRaw) {
				slot = s
			}
		}
		pin.release()
		damagePage(t, path, 512, slot, 100)
		report, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{})
		if err != nil {
			t.Fatal(err)
		}
		if len(report.Findings) == 0 || report.Findings[0].Kind != IntegrityMetaInvalid || *report.Findings[0].Page != slot {
			t.Fatalf("want meta_invalid at slot %d, got %+v", slot, report.Findings)
		}
	})

	t.Run("manifest and catalog", func(t *testing.T) {
		db, path := integrityTestDB(t)
		defer db.Close()
		pin, err := db.core.integrityPin(ctx, "main", nil)
		if err != nil {
			t.Fatal(err)
		}
		catalog := pin.gen.meta.rootPage
		pin.release()
		// The catalog root is rewritten by every commit, so the last commit's manifest lists it.
		damagePage(t, path, 512, catalog, 40)
		report, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{})
		if err != nil {
			t.Fatal(err)
		}
		kinds := integrityKinds(report)
		if len(kinds) < 2 || kinds[0] != IntegrityManifestPageMismatch || kinds[1] != IntegrityCatalogPageCorrupt {
			t.Fatalf("want manifest_page_mismatch then catalog_page_corrupt, got %v", kinds)
		}
		if *report.Findings[1].Page != catalog {
			t.Errorf("catalog_page_corrupt names page %d, want %d", *report.Findings[1].Page, catalog)
		}
	})

	t.Run("leaf and truncation", func(t *testing.T) {
		db, path := integrityTestDB(t)
		defer db.Close()
		table := db.core.roots.Load().committed.stores["t"]
		root := table.treeRoot()
		if root == nil || len(root.children) < 3 {
			t.Fatal("the table needs an interior root over several leaves")
		}
		var leaves []uint32
		for _, c := range root.children {
			leaves = append(leaves, c.ref().page)
		}
		// One more commit, which rewrites only the last leaf and the root, so the leaves damaged
		// below are not in the generation's manifest. Two damaged leaves: two page_corrupt
		// findings, then a table_unreadable skip of the index.
		if _, err := db.Session(SessionOptions{}).Exec(ctx, "INSERT INTO t VALUES (100000, 1, 'last')"); err != nil {
			t.Fatal(err)
		}
		damagePage(t, path, 512, leaves[0], 511)
		damagePage(t, path, 512, leaves[1], 511)
		report, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{})
		if err != nil {
			t.Fatal(err)
		}
		kinds := integrityKinds(report)
		if !slices.Equal(kinds, []string{IntegrityPageCorrupt, IntegrityPageCorrupt}) || len(report.Skipped) != 1 ||
			report.Skipped[0].Reason != IntegritySkipTableUnreadable {
			t.Fatalf("got %v %+v", kinds, report.Skipped)
		}
		one, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{MaxFindings: 1})
		if err != nil {
			t.Fatal(err)
		}
		if !one.Truncated || len(one.Findings) != 1 || one.Findings[0].Kind != IntegrityPageCorrupt || one.OK() {
			t.Fatalf("truncated: %+v", one)
		}
		if one.Pages >= report.Pages || one.Rows >= report.Rows {
			t.Errorf("a truncated report counts what it reached: %d/%d pages, %d/%d rows", one.Pages, report.Pages, one.Rows, report.Rows)
		}
	})
}

// A page at depth integrityMaxTreeDepth or deeper is reached (counted) and then reported page_corrupt
// without being read; a page already reached is page_reached_twice first (§5.2).
func TestIntegrityDepthCap(t *testing.T) {
	db, _ := integrityTestDB(t)
	defer db.Close()
	pin, err := db.core.integrityPin(context.Background(), "main", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer pin.release()
	k := newIntegrityChecker(context.Background(), "main", IntegrityCheckOptions{MaxFindings: IntegrityUnlimited}, pin.snap, pin.paging, pin.gen)
	k.report.Findings = []IntegrityFinding{}
	root := pin.snap.stores["t"].treeRoot().ref()
	w := &integrityTree{k: k, object: "t", colTypes: pin.snap.stores["t"].colTypes, table: pin.snap.tables["t"], leafDepth: -1, readable: true}
	if err := w.visit(root, integrityMaxTreeDepth, nil, nil); err != nil {
		t.Fatal(err)
	}
	if err := w.visit(root, integrityMaxTreeDepth, nil, nil); err != nil {
		t.Fatal(err)
	}
	kinds := integrityKinds(k.report)
	if !slices.Equal(kinds, []string{IntegrityPageCorrupt, IntegrityPageReachedTwice}) || w.readable || k.reachedN != 1 || k.report.Rows != 0 {
		t.Fatalf("got %v readable %v reached %d rows %d", kinds, w.readable, k.reachedN, k.report.Rows)
	}
	g := &integrityGist{k: k, object: "g", leafDepth: -1, readable: true}
	other := root
	other.page = root.page + 1
	if err := g.visit(other, integrityMaxTreeDepth+3); err != nil {
		t.Fatal(err)
	}
	if last := k.report.Findings[len(k.report.Findings)-1]; last.Kind != IntegrityPageCorrupt || g.readable || k.reachedN != 2 {
		t.Fatalf("GiST: got %+v readable %v reached %d", last, g.readable, k.reachedN)
	}
}
