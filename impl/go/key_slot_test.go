package jed

import (
	"encoding/hex"
	"os"
	"regexp"
	"strings"
	"testing"
)

// TestKeySlotSkipsEveryOrderedVector runs the unified key-slot skipper over every ordered slot in
// spec/encoding/ordered.toml (indexes.md §5.1/§11, encoding.md §2.16): for each scalar group's four
// orders, the skip recognizes the order's NULL tag and steps over the (DESC: inverted) body exactly,
// with trailing bytes after the slot — the storage-key suffix a real entry carries. Byte-level
// skipping is out of the corpus's reach (CLAUDE.md §10).
func TestKeySlotSkipsEveryOrderedVector(t *testing.T) {
	data, err := os.ReadFile("../../spec/encoding/ordered.toml")
	if err != nil {
		t.Fatal(err)
	}
	orders := map[string]keyOrder{
		"asc_nulls_last":   {},
		"asc_nulls_first":  {NullsFirst: true},
		"desc_nulls_first": {Desc: true, NullsFirst: true},
		"desc_nulls_last":  {Desc: true},
	}
	field := regexp.MustCompile(`(asc_nulls_last|asc_nulls_first|desc_nulls_first|desc_nulls_last) = "([0-9a-f]*)"`)
	var slot keySlot
	skip := true
	checked := 0
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		if name, ok := strings.CutPrefix(line, "type = "); ok {
			name = strings.Trim(name, `"`)
			switch {
			case strings.HasPrefix(name, "collated text"):
				slot, skip = keySlot{kind: keySlotCollated}, false
			case strings.Contains(name, "range") || strings.Contains(name, "[]") || strings.Contains(name, "("):
				skip = true // a container group: its slot is not skippable by keySlot (§5.1)
			default:
				ty, ok := scalarTypeFromName(name)
				if !ok {
					t.Fatalf("unknown type %q", name)
				}
				slot, _ = keySlotForScalar(ty, false)
				skip = false
			}
			continue
		}
		if skip || !strings.HasPrefix(line, "{") {
			continue
		}
		for _, m := range field.FindAllStringSubmatch(line, -1) {
			b, err := hex.DecodeString(m[2])
			if err != nil {
				t.Fatal(err)
			}
			key := append(append([]byte(nil), b...), 0x00, 0x01, 0xFE, 0xFF)
			n, ok := slot.withOrder(orders[m[1]]).slotLen(key, 0)
			if !ok || n != len(b) {
				t.Fatalf("%s %s: slotLen = %d, %v; want %d", line, m[1], n, ok, len(b))
			}
			checked++
		}
	}
	if checked < 200 {
		t.Fatalf("only %d slots checked", checked)
	}
}
