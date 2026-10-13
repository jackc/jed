package jed

// GIN jsonb_ops term vectors (spec/encoding/gin_jsonb_terms.toml, spec/design/gin.md §2.1/§4.1):
// each document's index term SET and each query key's term must be byte-exact against the shared
// vectors — the term bytes are the GIN entry keys, so this is a byte-level contract no corpus entry
// can observe (CLAUDE.md §8/§10).

import (
	"encoding/hex"
	"os"
	"strconv"
	"strings"
	"testing"
)

type ginJsonbIndexVector struct {
	doc   string
	terms []string
}

type ginJsonbKeyVector struct {
	key  string
	term string
}

// readGinJsonbVectors is a line scanner for the vector file's narrow shape: `[[index]]` groups of a
// literal-string `doc = '…'` plus a multi-line `terms = [ "hex", … ]` array, and `[[key]]` groups of a
// basic-string `key = "…"` plus `term = "hex"`.
func readGinJsonbVectors(t *testing.T) ([]ginJsonbIndexVector, []ginJsonbKeyVector) {
	t.Helper()
	data, err := os.ReadFile(specPath(t, "encoding/gin_jsonb_terms.toml"))
	if err != nil {
		t.Fatal(err)
	}
	var index []ginJsonbIndexVector
	var keys []ginJsonbKeyVector
	section := ""
	inTerms := false
	basic := func(s string) string {
		v, err := strconv.Unquote(s)
		if err != nil {
			t.Fatalf("bad TOML basic string %q: %v", s, err)
		}
		return v
	}
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		switch {
		case inTerms:
			if line == "]" {
				inTerms = false
			} else if line != "" {
				cur := &index[len(index)-1]
				cur.terms = append(cur.terms, basic(strings.TrimSuffix(line, ",")))
			}
		case line == "[[index]]":
			section = "index"
			index = append(index, ginJsonbIndexVector{})
		case line == "[[key]]":
			section = "key"
			keys = append(keys, ginJsonbKeyVector{})
		case section == "index" && strings.HasPrefix(line, "doc = '"):
			index[len(index)-1].doc = strings.TrimSuffix(strings.TrimPrefix(line, "doc = '"), "'")
		case section == "index" && line == "terms = [":
			inTerms = true
		case section == "key" && strings.HasPrefix(line, "key = "):
			keys[len(keys)-1].key = basic(strings.TrimPrefix(line, "key = "))
		case section == "key" && strings.HasPrefix(line, "term = "):
			keys[len(keys)-1].term = basic(strings.TrimPrefix(line, "term = "))
		}
	}
	return index, keys
}

func TestGinJsonbTermVectors(t *testing.T) {
	t.Parallel()
	index, keys := readGinJsonbVectors(t)
	if len(index) == 0 || len(keys) == 0 {
		t.Fatalf("no vectors read (index=%d key=%d)", len(index), len(keys))
	}
	for _, v := range index {
		node, err := jsonbIn(v.doc)
		if err != nil {
			t.Fatalf("jsonbIn(%q): %v", v.doc, err)
		}
		var got []string
		for _, term := range ginJsonbIndexTerms(&node) {
			got = append(got, hex.EncodeToString(term))
		}
		if strings.Join(got, ",") != strings.Join(v.terms, ",") {
			t.Errorf("index terms of %.60q:\n got  %v\n want %v", v.doc, got, v.terms)
		}
	}
	for _, v := range keys {
		if got := hex.EncodeToString(ginJsonbKeyTerm(v.key)); got != v.term {
			t.Errorf("key term of %.60q: got %s want %s", v.key, got, v.term)
		}
	}
}
