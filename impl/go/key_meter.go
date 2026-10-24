package jed

import "unicode/utf8"

// Per-row key metering (spec/design/cost.md §3 "sort_key / hash_key"). A sort, a grouping or
// dedup table, and a window's partitioning charge each row's key values once, by size, where the
// row enters the operator. The comparisons and probes inside them stay unmetered. Each charge is
// guarded when it is nonzero, so a fixed-width or NULL key moves no abort point.

// chargeSortKey charges one sort-key value of a row the sort receives: collate per code point for
// a collated text or text[] key, else sort_key per payload byte.
func chargeSortKey(m *costMeter, v Value, coll *Collation) error {
	var units int64
	if coll != nil && (v.Kind == ValText || v.Kind == ValArray) {
		units = costs.Collate * collatedCodePoints(v)
	} else {
		units = costs.SortKey * memPayloadBytes(v)
	}
	if units == 0 {
		return nil
	}
	m.Charge(units)
	return m.Guard()
}

// chargeSortKeys charges every sort key of one row a sort receives, in key order. A collated sort
// charges through collationKeysForRow instead, which interleaves each key's charge with its
// decoration.
func chargeSortKeys[R ~[]Value](m *costMeter, row R, order []orderSlot) error {
	for _, k := range order {
		if err := chargeSortKey(m, row[k.idx], k.collation); err != nil {
			return err
		}
	}
	return nil
}

// collatedCodePoints is the code points a collated key's UCA sort key is built from: a text's, or
// a text[]'s non-NULL elements'.
func collatedCodePoints(v Value) int64 {
	switch v.Kind {
	case ValText:
		return int64(utf8.RuneCountInString(v.str()))
	case ValArray:
		var n int64
		for _, e := range v.arrayVal().Elements {
			if e.Kind == ValText {
				n += int64(utf8.RuneCountInString(e.str()))
			}
		}
		return n
	default:
		return 0
	}
}

// chargeHashKeys charges the key values of a row a grouping, dedup, or partition table receives:
// hash_key per payload byte of each, in key order.
func chargeHashKeys(m *costMeter, keys []Value) error {
	for _, v := range keys {
		if err := chargeHashKey(m, v); err != nil {
			return err
		}
	}
	return nil
}

// chargeHashKeysAt is chargeHashKeys over the key values at indices of row.
func chargeHashKeysAt(m *costMeter, row []Value, indices []int) error {
	for _, i := range indices {
		if err := chargeHashKey(m, row[i]); err != nil {
			return err
		}
	}
	return nil
}

func chargeHashKey(m *costMeter, v Value) error {
	units := costs.HashKey * memPayloadBytes(v)
	if units == 0 {
		return nil
	}
	m.Charge(units)
	return m.Guard()
}
