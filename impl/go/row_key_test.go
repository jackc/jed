package jed

import (
	"strings"
	"testing"
)

// A rowKey is distinctRowKey's encoding without the copies: its bytes are the flat key, two keys are
// equal exactly when their flat keys are, and a keyMap agrees with a map keyed by the flat strings.
func TestRowKeyMatchesDistinctRowKey(t *testing.T) {
	t.Parallel()
	long := strings.Repeat("ab", rowKeySegmentMin)
	longer := long + "c"
	rows := [][]Value{
		nil,
		{NullValue()},
		{IntValue(1), IntValue(23)},
		{IntValue(12), IntValue(3)},
		{TextValue("short")},
		{TextValue(long)},
		{TextValue(long[:len(long)-1] + "c")},
		{TextValue(longer)},
		{TextValue(long), TextValue(long)},
		{TextValue(long + long)},
		{TextValue(long), IntValue(7)},
		{IntValue(7), TextValue(long)},
		{ByteaValue([]byte(long))},
		{JsonValue(long)},
		{TextValue(strings.Repeat("x", rowKeySegmentMin-1))},
		{TextValue(strings.Repeat("x", rowKeySegmentMin))},
		{CompositeValue([]Value{TextValue(long), NullValue()})},
		{ArrayValue([]Value{TextValue(long), TextValue("s")})},
	}
	m := newKeyMap[int]()
	flat := map[string]int{}
	for i, a := range rows {
		ka := makeRowKey(a)
		if ka.String() != distinctRowKey(a) {
			t.Fatalf("row %d: key bytes differ from distinctRowKey", i)
		}
		for j, b := range rows {
			kb := makeRowKey(b)
			want := distinctRowKey(a) == distinctRowKey(b)
			got := ka.segs == nil && kb.segs == nil && ka.flat == kb.flat || ka.segs != nil && kb.segs != nil && ka.equal(kb)
			if got != want {
				t.Fatalf("rows %d and %d: equal=%v, distinctRowKey equal=%v", i, j, got, want)
			}
		}
		m.add(ka, i)
		if _, ok := flat[distinctRowKey(a)]; !ok {
			flat[distinctRowKey(a)] = i
		}
	}
	// A long body is a segment of its own, sharing the value's string.
	if k := makeRowKey([]Value{TextValue(long)}); len(k.segs) != 2 || k.segs[1] != long {
		t.Fatalf("long body not shared: %d segments", len(k.segs))
	}
	if k := makeRowKey([]Value{TextValue("short")}); k.segs != nil {
		t.Fatal("a short key is not flat")
	}
	if m.len() != len(flat) {
		t.Fatalf("keyMap holds %d keys, want %d", m.len(), len(flat))
	}
	for _, r := range rows {
		got, ok := m.get(makeRowKey(r))
		if want := flat[distinctRowKey(r)]; !ok || got != want {
			t.Fatalf("keyMap get = %d, %v; want %d", got, ok, want)
		}
	}
}
