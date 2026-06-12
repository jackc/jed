package bench

import "testing"

// The pinned splitmix64 vectors from spec/design/benchmarks.md §4 — the cross-language
// contract that makes every harness draw identical param sequences.
func TestPrngVectors(t *testing.T) {
	vectors := map[uint64][5]uint64{
		1:       {0x910a2dec89025cc1, 0xbeeb8da1658eec67, 0xf893a2eefb32555e, 0x71c18690ee42c90b, 0x71bb54d8d101b5b9},
		1234567: {0x599ed017fb08fc85, 0x2c73f08458540fa5, 0x883ebce5a3f27c77, 0x3fbef740e9177b3f, 0xe3b8346708cb5ecd},
	}
	for seed, want := range vectors {
		p := NewPrng(seed)
		for i, w := range want {
			if got := p.Next(); got != w {
				t.Errorf("seed %d output %d: got %#016x, want %#016x", seed, i, got, w)
			}
		}
	}
}

func TestTextDraw(t *testing.T) {
	p := NewPrng(1)
	s := p.Text(8, 32)
	if len(s) < 8 || len(s) > 32 {
		t.Fatalf("text length %d out of [8,32]", len(s))
	}
	for _, c := range s {
		if c < 'a' || c > 'z' {
			t.Fatalf("text char %q out of a-z", c)
		}
	}
}
