package jed

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
		st, ok := scalarTypeFromName(c.typ)
		if !ok {
			t.Fatalf("unknown type %q", c.typ)
		}
		var got []byte
		if c.typ == "uuid" {
			// uuid is the first non-integer key: a uuid key is the bare 16 bytes ParseUUID
			// produces (encoding.md §2.7); nullable/descending use the shared tag/inversion.
			switch c.kind {
			case "bare":
				got, _ = parseUUID(c.strValue)
				if UuidValue(got).Render() != c.strValue {
					t.Errorf("bare uuid %s: round-trip got %s", c.strValue, UuidValue(got).Render())
				}
			case "nullable":
				got = nullableUUIDBytes(c)
			case "descending":
				got = invertBytes(nullableUUIDBytes(c))
			}
			if h := hex.EncodeToString(got); h != c.bytes {
				t.Errorf("%s uuid value=%q null=%v: got %s want %s", c.kind, c.strValue, c.isNull, h, c.bytes)
			}
			checked++
			continue
		}
		if c.typ == "boolean" {
			// boolean is the second non-integer key: a single bool-byte (0x00 false / 0x01 true)
			// EncodeBool produces (encoding.md §2.9); nullable/descending use the shared
			// tag/inversion.
			switch c.kind {
			case "bare":
				got = encodeBool(c.boolValue)
			case "nullable":
				got = nullableBoolBytes(c)
			case "descending":
				got = invertBytes(nullableBoolBytes(c))
			}
			if h := hex.EncodeToString(got); h != c.bytes {
				t.Errorf("%s boolean value=%v null=%v: got %s want %s", c.kind, c.boolValue, c.isNull, h, c.bytes)
			}
			checked++
			continue
		}
		switch c.kind {
		case "bare":
			got = encodeInt(st, c.value)
			if dec := decodeInt(st, got); dec != c.value {
				t.Errorf("bare %s %d: round-trip got %d", c.typ, c.value, dec)
			}
		case "nullable":
			if c.isNull {
				got = encodeNullable(st, nil)
			} else {
				v := c.value
				got = encodeNullable(st, &v)
			}
		case "descending":
			var asc []byte
			if c.isNull {
				asc = encodeNullable(st, nil)
			} else {
				v := c.value
				asc = encodeNullable(st, &v)
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
