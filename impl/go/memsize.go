package jed

// The logical query-memory size schedule (spec/design/memory.md §3).
//
// Representation-independent byte counts the query-memory account reserves and releases. They are a
// pure function of a value's logical content — never of Go's object layout, a leaf's physical state,
// or whether a column was decoded — so a 54P05 abort is cross-core identical. The constants are
// codegen'd from spec/cost/schedule.toml [memory] (costs.go); the shared vectors in
// spec/cost/memory_sizes.toml pin the measurement (memory_sizes_test.go).

// memPayloadBytes is payload(v) = value_bytes(v) − VALUE: the logical bytes an output-constructing
// kernel charges as scalar_byte (spec/design/cost.md §8.1).
func memPayloadBytes(v Value) int64 {
	return memValueBytes(v) - memoryValue
}

// memValueBytes is value_bytes(v) = VALUE + payload(v).
func memValueBytes(v Value) int64 {
	switch v.Kind {
	case ValText, ValJson, ValJsonPath, ValBytea:
		return memoryValue + int64(len(v.str()))
	case ValDecimal:
		return memoryValue + memoryDecimalGroup*int64(v.decimal().codecGroupCount())
	case ValArray:
		a := v.arrayVal()
		n := memoryValue + memoryArrayDim*int64(len(a.Dims))
		for _, e := range a.Elements {
			n += memValueBytes(e)
		}
		return n
	case ValComposite:
		n := memoryValue
		for _, f := range *v.composite() {
			n += memValueBytes(f)
		}
		return n
	case ValRange:
		r := v.rangeVal()
		n := memoryValue
		if r.Lower != nil {
			n += memValueBytes(*r.Lower)
		}
		if r.Upper != nil {
			n += memValueBytes(*r.Upper)
		}
		return n
	case ValJsonb:
		return memJsonbBytes(v.jsonb())
	default:
		// NULL, fixed-width scalars, and an unfetched reference (only ever an untouched slot) are a
		// bare node.
		return memoryValue
	}
}

// memJsonbBytes is one VALUE per jsonb node plus its string/key bytes and number groups.
func memJsonbBytes(n *JsonNode) int64 {
	b := memoryValue
	switch n.Kind {
	case JString:
		b += int64(len(n.S))
	case JNumber:
		b += memoryDecimalGroup * int64(n.Num.codecGroupCount())
	case JArray:
		for i := range n.Arr {
			b += memJsonbBytes(&n.Arr[i])
		}
	case JObject:
		for i := range n.Obj {
			b += int64(len(n.Obj[i].Key)) + memJsonbBytes(&n.Obj[i].Val)
		}
	}
	return b
}

// memRowBytes is row_bytes(row) = ROW + Σ value_bytes — a projected row, every slot a computed value.
func memRowBytes(row []Value) int64 {
	n := memoryRow
	for _, v := range row {
		n += memValueBytes(v)
	}
	return n
}

// memRowBytesMasked measures a pre-projection row under the plan's touched mask (memory.md §3): an
// untouched slot (i < len(mask) && !mask[i]) is a bare VALUE whatever its physical form; touched and
// appended slots count in full.
func memRowBytesMasked(row []Value, mask []bool) int64 {
	n := memoryRow
	for i, v := range row {
		if i < len(mask) && !mask[i] {
			n += memoryValue
		} else {
			n += memValueBytes(v)
		}
	}
	return n
}

// memoryMask is the touched mask over the logical combined row (each relation's mask concatenated
// in column order) — how query memory measures a pre-projection row (spec/design/memory.md §3). nil
// when the meter is unlimited, since no measurement will run.
func (p *selectPlan) memoryMask(meter *costMeter) []bool {
	if !meter.queryMemoryActive() {
		return nil
	}
	var mask []bool
	for _, m := range p.relMasks {
		mask = append(mask, m...)
	}
	return mask
}

// memKeyBytes is key_bytes(values) = Σ value_bytes — a key or state tuple with no ROW header
// (memory.md §6.1).
func memKeyBytes(values []Value) int64 {
	var n int64
	for _, v := range values {
		n += memValueBytes(v)
	}
	return n
}

// memEntryBytes is entry_bytes(values) = ENTRY + key_bytes — one hash/dedup/group entry
// (memory.md §6.1).
func memEntryBytes(values []Value) int64 {
	return memoryEntry + memKeyBytes(values)
}

// memEntryBytesAt is entry_bytes of the key at indices of row, without copying the key out.
func memEntryBytesAt(row []Value, indices []int) int64 {
	n := memoryEntry
	for _, i := range indices {
		n += memValueBytes(row[i])
	}
	return n
}

// memMapEntryBytes is a spill-capable keyed state map's resident measure of one element
// (memory.md §6.6): ENTRY + key_bytes(key) + key_bytes(value).
func memMapEntryBytes(key, value []Value) int64 {
	return memoryEntry + memKeyBytes(key) + memKeyBytes(value)
}

// memHashRowBytes is a spill-capable hash row table's resident measure of one row (memory.md §6.6).
func memHashRowBytes(row []Value) int64 {
	return memoryEntry + memRowBytes(row)
}
