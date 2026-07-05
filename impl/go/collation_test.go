package jed

// Cross-core collation contract: the Go compiler + executor + artifact codec must reproduce the
// byte-exact vectors in spec/collation/vectors/{compiler,sortkey}.toml (CLAUDE.md §8;
// spec/collation/README.md §2/§3/§4). Mirrors impl/rust/tests/collation.rs and
// impl/ts/tests/collation.test.ts.

import (
	"bytes"
	"encoding/hex"
	"os"
	"reflect"
	"strings"
	"testing"
)

// loadFixtureBundle loads jed's pinned production JUCD bundle (spec/collation/fixtures/unicode.jucd)
// into the engine-global loaded set — the production read path the cores now take (no embed).
// Idempotent (the set is global + first-wins).
func loadFixtureBundle(t *testing.T) {
	t.Helper()
	data, err := os.ReadFile(specPath(t, "collation/fixtures/unicode.jucd"))
	if err != nil {
		t.Fatalf("read unicode.jucd: %v", err)
	}
	if err := LoadUnicodeData(data); err != nil {
		t.Fatalf("load unicode.jucd: %v", err)
	}
}

func collDefinition(t *testing.T, files []string) string {
	t.Helper()
	parts := make([]string, 0, len(files))
	for _, f := range files {
		data, err := os.ReadFile(specPath(t, f))
		if err != nil {
			t.Fatalf("read %s: %v", f, err)
		}
		parts = append(parts, string(data))
	}
	out := ""
	for i, p := range parts {
		if i > 0 {
			out += "\n"
		}
		out += p
	}
	return out
}

func TestCollationCompilerMatchesVectors(t *testing.T) {
	t.Parallel()
	rows := readTomlTables(t, specPath(t, "collation/vectors/compiler.toml"), "compiler")
	if len(rows) == 0 {
		t.Fatal("no compiler vectors")
	}
	for _, row := range rows {
		name := row.str("name")
		def := collDefinition(t, row.strs("def_files"))
		coll, err := compileCollation(row.str("coll_name"), def)
		if err != nil {
			t.Fatalf("%s: compile: %v", name, err)
		}
		if got := hex.EncodeToString(serializeTable(coll)); got != row.str("table_hex") {
			t.Errorf("%s: table\n got %s\nwant %s", name, got, row.str("table_hex"))
		}
		artifact := saveCollation(coll)
		if got := hex.EncodeToString(artifact); got != row.str("artifact_hex") {
			t.Errorf("%s: artifact\n got %s\nwant %s", name, got, row.str("artifact_hex"))
		}
		reopened, err := openCollation(artifact)
		if err != nil {
			t.Fatalf("%s: open: %v", name, err)
		}
		if !reflect.DeepEqual(reopened, coll) {
			t.Errorf("%s: reopened collation != compiled", name)
		}
		if got := hex.EncodeToString(saveCollation(reopened)); got != row.str("artifact_hex") {
			t.Errorf("%s: open→save round-trip mismatch", name)
		}
	}
}

func TestCollationSortKeyMatchesVectorsAndIsAscending(t *testing.T) {
	t.Parallel()
	loadFixtureBundle(t)
	rows := readTomlTables(t, specPath(t, "collation/vectors/sortkey.toml"), "sortkey")
	if len(rows) == 0 {
		t.Fatal("no sortkey vectors")
	}
	lastColl := ""
	var coll *Collation
	var prev []byte
	for _, row := range rows {
		collName := row.str("coll_name")
		s := row.str("string")
		want := row.str("sortkey_hex")
		if collName != lastColl {
			// The real version-pinned collations (unicode, es) resolve from the loaded production
			// bundle (the host-load read path), not by recompiling their ~2.3 MB source. The small dev
			// fixtures (not in the bundle) fall back to compiling from their definition files.
			if vc := LoadedCollation(collName); vc != nil {
				coll = vc
			} else {
				c, err := compileCollation(collName, collDefinition(t, row.strs("def_files")))
				if err != nil {
					t.Fatalf("%s: compile: %v", collName, err)
				}
				coll = c
			}
			lastColl = collName
			prev = nil
		}
		key, err := sortKey(coll, s)
		if err != nil {
			t.Fatalf("%s %q: sort key: %v", collName, s, err)
		}
		if got := hex.EncodeToString(key); got != want {
			t.Errorf("%s %q: sort key\n got %s\nwant %s", collName, s, got, want)
		}
		if prev != nil && bytes.Compare(prev, key) >= 0 {
			t.Errorf("%s: %q must sort strictly after the previous entry", collName, s)
		}
		prev = key
	}
}

func TestCollationBundleVectorsRoundTripAndMerge(t *testing.T) {
	t.Parallel()
	rows := readTomlTables(t, specPath(t, "collation/vectors/bundle.toml"), "bundle")
	if len(rows) == 0 {
		t.Fatal("no bundle vectors")
	}
	for _, row := range rows {
		rootName := row.str("root_name")
		root, err := compileCollation(rootName, collDefinition(t, row.strs("root_def_files")))
		if err != nil {
			t.Fatalf("%s: compile root: %v", rootName, err)
		}
		// Flat layout: tailoring_def_files[i] is the i-th tailoring's files joined by '|'.
		names := row.strs("tailoring_names")
		defs := row.strs("tailoring_def_files")
		if len(names) != len(defs) {
			t.Fatalf("tailoring_names/tailoring_def_files length mismatch")
		}
		tailorings := make([]*Collation, len(names))
		for i, n := range names {
			c, err := compileCollation(n, collDefinition(t, strings.Split(defs[i], "|")))
			if err != nil {
				t.Fatalf("%s: compile tailoring: %v", n, err)
			}
			tailorings[i] = c
		}

		bundle := buildBundle(root, tailorings, nil, row.str("description"))
		enc := saveBundle(bundle)
		want := row.str("bundle_hex")
		if got := hex.EncodeToString(enc); got != want {
			t.Errorf("bundle bytes\n got %s\nwant %s", got, want)
		}

		reopened, err := openBundle(enc)
		if err != nil {
			t.Fatalf("open bundle: %v", err)
		}
		if got := hex.EncodeToString(saveBundle(reopened)); got != want {
			t.Errorf("bundle open→save round-trip mismatch")
		}

		colls, _, err := loadBundle(reopened)
		if err != nil {
			t.Fatalf("load bundle: %v", err)
		}
		find := func(name string) *Collation {
			for _, c := range colls {
				if c.Name == name {
					return c
				}
			}
			t.Fatalf("loaded bundle missing collation %q", name)
			return nil
		}
		if !bytes.Equal(serializeTable(find(rootName)), serializeTable(root)) {
			t.Errorf("root table changed through the bundle")
		}
		for _, tl := range tailorings {
			if !bytes.Equal(serializeTable(find(tl.Name)), serializeTable(tl)) {
				t.Errorf("merge identity failed for %s", tl.Name)
			}
		}
	}
}

func TestCollationOpenRejectsTamperedArtifact(t *testing.T) {
	t.Parallel()
	coll, err := compileCollation("dev-root", collDefinition(t, []string{"collation/fixtures/dev-root.allkeys"}))
	if err != nil {
		t.Fatal(err)
	}
	artifact := saveCollation(coll)
	artifact[len(artifact)-1] ^= 0xFF
	_, err = openCollation(artifact)
	if err == nil {
		t.Fatal("expected an error opening a tampered artifact")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
		t.Fatalf("want XX001, got %v", err)
	}
}

// TestCasingKernels pins the casing kernels' DIVERGENCES from the PG/glibc oracle (collation.md §16) —
// what the oracle corpus cannot express (CLAUDE.md §10): the ASCII baseline (non-ASCII passes through)
// and full SpecialCasing (ß→SS). The kernels take an EXPLICIT property table, so the un-loaded (ASCII)
// regime is deterministic regardless of the engine-global loaded set. Mirrors collation.rs casing_tests.
func TestCasingKernels(t *testing.T) {
	t.Parallel()
	p := &propertyTable{
		Simple: []simpleCase{
			{0x41, 0x41, 0x61, 0x41}, // A
			{0x61, 0x41, 0x61, 0x41}, // a
			{0xC9, 0xC9, 0xE9, 0xC9}, // É
			{0xE9, 0xC9, 0xE9, 0xC9}, // é
		},
		Special: []specialCasing{{Cp: 0xDF, Upper: []uint32{0x53, 0x53}, Lower: []uint32{0xDF}, Title: []uint32{0x53, 0x73}}},
	}
	cases := []struct {
		got, want string
	}{
		// ASCII baseline (nil property): fold a–z/A–Z, pass the rest through.
		{foldCase("café", true, nil), "CAFé"},
		{foldCase("CAFÉ", false, nil), "cafÉ"},
		{foldCase("ß", true, nil), "ß"},
		// Full Unicode via the property table.
		{foldCase("aé", true, p), "AÉ"},
		{foldCase("AÉ", false, p), "aé"},
		{foldCase("ß", true, p), "SS"}, // SpecialCasing expansion — the glibc divergence
		{foldCase("aßa", true, p), "ASSA"},
		{foldCase("z", true, p), "z"}, // not in the table → identity
		// ILIKE folding is simple-only (never expands): ß stays one code point.
		{foldLowerSimple("ß", p), "ß"},
		{foldLowerSimple("É", p), "é"},
		{foldLowerSimple("HELLO", nil), "hello"},
		{foldLowerSimple("É", nil), "É"},
	}
	for i, c := range cases {
		if c.got != c.want {
			t.Errorf("case %d: got %q want %q", i, c.got, c.want)
		}
	}
}
