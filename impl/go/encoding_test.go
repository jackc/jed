package abide

// Cross-check: the Go key encoder must reproduce the byte-exact vectors in
// spec/encoding/integers.toml (CLAUDE.md §8). This is what guarantees the Rust and
// Go cores iterate keys in the same order.

import (
	"encoding/hex"
	"testing"
)

func tomlGroupsWithCases(t *testing.T, section string) []tomlRow {
	t.Helper()
	return readTomlTables(t, specPath(t, "encoding/integers.toml"), section)
}

// The cases live as an inline array-of-inline-tables on one logical key; the tiny
// test reader handles arrays of *tables* only at the [[section]] level, so for the
// encoding fixtures we read the cases with a dedicated line scanner.
func TestEncodingVectors(t *testing.T) {
	cases := readEncodingCases(t, specPath(t, "encoding/integers.toml"))
	checked := 0
	for _, c := range cases {
		st, ok := ScalarTypeFromName(c.typ)
		if !ok {
			t.Fatalf("unknown type %q", c.typ)
		}
		var got []byte
		switch c.kind {
		case "bare":
			got = EncodeInt(st, c.value)
			if dec := DecodeInt(st, got); dec != c.value {
				t.Errorf("bare %s %d: round-trip got %d", c.typ, c.value, dec)
			}
		case "nullable":
			if c.isNull {
				got = EncodeNullable(st, nil)
			} else {
				v := c.value
				got = EncodeNullable(st, &v)
			}
		case "descending":
			var asc []byte
			if c.isNull {
				asc = EncodeNullable(st, nil)
			} else {
				v := c.value
				asc = EncodeNullable(st, &v)
			}
			got = invertBytes(asc)
		}
		if h := hex.EncodeToString(got); h != c.bytes {
			t.Errorf("%s %s value=%d null=%v: got %s want %s", c.kind, c.typ, c.value, c.isNull, h, c.bytes)
		}
		checked++
	}
	if checked == 0 {
		t.Fatal("no encoding cases parsed")
	}
}

func invertBytes(b []byte) []byte {
	out := make([]byte, len(b))
	for i, x := range b {
		out[i] = x ^ 0xFF
	}
	return out
}
