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
	"testing"
)

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
	rows := readTomlTables(t, specPath(t, "collation/vectors/compiler.toml"), "compiler")
	if len(rows) == 0 {
		t.Fatal("no compiler vectors")
	}
	for _, row := range rows {
		name := row.str("name")
		def := collDefinition(t, row.strs("def_files"))
		coll, err := CompileCollation(row.str("coll_name"), def)
		if err != nil {
			t.Fatalf("%s: compile: %v", name, err)
		}
		if got := hex.EncodeToString(SerializeTable(coll)); got != row.str("table_hex") {
			t.Errorf("%s: table\n got %s\nwant %s", name, got, row.str("table_hex"))
		}
		artifact := SaveCollation(coll)
		if got := hex.EncodeToString(artifact); got != row.str("artifact_hex") {
			t.Errorf("%s: artifact\n got %s\nwant %s", name, got, row.str("artifact_hex"))
		}
		reopened, err := OpenCollation(artifact)
		if err != nil {
			t.Fatalf("%s: open: %v", name, err)
		}
		if !reflect.DeepEqual(reopened, coll) {
			t.Errorf("%s: reopened collation != compiled", name)
		}
		if got := hex.EncodeToString(SaveCollation(reopened)); got != row.str("artifact_hex") {
			t.Errorf("%s: open→save round-trip mismatch", name)
		}
	}
}

func TestCollationSortKeyMatchesVectorsAndIsAscending(t *testing.T) {
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
			// The real version-pinned collations (unicode, es) resolve from the embedded .coll — the
			// production read path — rather than recompiling their ~2.3 MB source. The small dev
			// fixtures (not vendored) are compiled from their definition files.
			if vc := vendored()[collName]; vc != nil {
				coll = vc
			} else {
				c, err := CompileCollation(collName, collDefinition(t, row.strs("def_files")))
				if err != nil {
					t.Fatalf("%s: compile: %v", collName, err)
				}
				coll = c
			}
			lastColl = collName
			prev = nil
		}
		key, err := SortKey(coll, s)
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

func TestCollationOpenRejectsTamperedArtifact(t *testing.T) {
	coll, err := CompileCollation("dev-root", collDefinition(t, []string{"collation/fixtures/dev-root.allkeys"}))
	if err != nil {
		t.Fatal(err)
	}
	artifact := SaveCollation(coll)
	artifact[len(artifact)-1] ^= 0xFF
	_, err = OpenCollation(artifact)
	if err == nil {
		t.Fatal("expected an error opening a tampered artifact")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
		t.Fatalf("want XX001, got %v", err)
	}
}
