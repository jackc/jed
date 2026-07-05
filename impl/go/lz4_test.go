package jed

// Cross-check: the Go LZ4-block encoder must reproduce the byte-exact vectors in
// spec/fileformat/lz4_vectors.toml (CLAUDE.md §8; spec/fileformat/lz4.md §4). The encoder is
// pinned — a library would diverge (large-values.md §6) — so these vectors are what guarantee
// the Rust, Go, TS, and Ruby codecs emit identical compressed bytes (which the goldens and the
// deterministic cost both depend on). The decoder is checked by round-tripping each vector.
// Mirrors impl/rust/tests/lz4_vectors.rs and impl/ts/tests/lz4_vectors.test.ts.

import (
	"bytes"
	"encoding/hex"
	"strings"
	"testing"
)

func TestLZ4EncoderMatchesThePinnedVectors(t *testing.T) {
	t.Parallel()
	rows := readTomlTables(t, specPath(t, "fileformat/lz4_vectors.toml"), "vector")
	if len(rows) < 10 {
		t.Fatalf("vector corpus unexpectedly small: %d", len(rows))
	}
	for _, row := range rows {
		name := row.str("name")
		input, err := hex.DecodeString(row.str("input_hex"))
		if err != nil {
			t.Fatalf("%s: bad input_hex: %v", name, err)
		}
		comp := lz4Compress(input)
		if got, want := hex.EncodeToString(comp), row.str("compressed_hex"); got != want {
			t.Errorf("%s: compressed bytes\n got %s\nwant %s", name, got, want)
		}
		round, err := lz4Decompress(comp, len(input))
		if err != nil {
			t.Fatalf("%s: decompress: %v", name, err)
		}
		if !bytes.Equal(round, input) {
			t.Errorf("%s: round-trip mismatch", name)
		}
	}
}

func TestLZ4MalformedBlocksAreDataCorrupted(t *testing.T) {
	t.Parallel()
	cases := []struct {
		name   string
		comp   []byte
		rawLen int
	}{
		{"truncated literals", []byte{0x50}, 5},
		{"zero offset", []byte{0x14, 'a', 0x00, 0x00, 0x00}, 10},
		{"offset beyond prefix", []byte{0x14, 'a', 0x05, 0x00, 0x00}, 10},
		{"output overflow", []byte{0x1F, 'a', 0x01, 0x00, 0xFF, 0xFF, 0x00}, 4},
		{"length mismatch", []byte{0x10, 'a'}, 2},
	}
	for _, c := range cases {
		if _, err := lz4Decompress(c.comp, c.rawLen); err == nil {
			t.Errorf("%s: expected data_corrupted", c.name)
		} else if !strings.Contains(err.Error(), "XX001") {
			t.Errorf("%s: expected XX001, got %v", c.name, err)
		}
	}
}
