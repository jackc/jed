package jed

import (
	"bytes"
	"encoding/binary"
)

// hashJoinTable is a deterministic lookup-only hash table. A hash bucket retains build-row order;
// probe filters the full key bytes so even a forced hash collision cannot admit a false match. The
// executor never iterates the map to emit rows.
type hashJoinTable struct {
	entries     map[uint64][]hashJoinEntry
	hash        func([]byte) uint64
	probeOffset int
	// charge is the table's query-memory charge: entry_bytes(build key) per entry (memory.md §6.2),
	// returned by release when the join step completes.
	charge *stateCharge
}

// release returns the table's query-memory charge — its join step completed (memory.md §6.2).
func (t *hashJoinTable) release() {
	if t != nil {
		t.charge.releaseAll()
	}
}

type hashJoinEntry struct {
	key []byte
	row storedRow
	// pos is the row's position in the build rows.
	pos int
}

func newHashJoinTable(plan *hashJoinPlan, buildOffset, probeOffset int, rows []storedRow, meter *costMeter) (*hashJoinTable, error) {
	return newHashJoinTableWithHash(plan, buildOffset, probeOffset, rows, meter, hashJoinFNV1a)
}

func newHashJoinTableWithHash(plan *hashJoinPlan, buildOffset, probeOffset int, rows []storedRow, meter *costMeter, hasher func([]byte) uint64) (*hashJoinTable, error) {
	t := &hashJoinTable{entries: make(map[uint64][]hashJoinEntry), hash: hasher, probeOffset: probeOffset, charge: meter.stateCharge()}
	for pos, row := range rows {
		indices := make([]int, len(plan.keys))
		types := make([]dataType, len(plan.keys))
		for i, key := range plan.keys {
			indices[i] = key.right - buildOffset
			types[i] = key.ty
		}
		encoded, present, err := hashJoinRowKey(row, indices, types, costs.HashBuild, meter)
		if err != nil {
			return nil, err
		}
		if !present {
			continue
		}
		if t.charge.active() {
			if err := t.charge.reserve(meter, memEntryBytesAt(row, indices)); err != nil {
				return nil, err
			}
		}
		h := t.hash(encoded)
		t.entries[h] = append(t.entries[h], hashJoinEntry{key: encoded, row: row, pos: pos})
	}
	return t, nil
}

func (t *hashJoinTable) probe(plan *hashJoinPlan, row storedRow, meter *costMeter) ([]storedRow, error) {
	encoded, entries, err := t.bucket(plan, row, meter)
	if err != nil || entries == nil {
		return nil, err
	}
	out := make([]storedRow, 0, len(entries))
	for i := range entries {
		hit, err := probeEntry(&entries[i], encoded, meter)
		if err != nil {
			return nil, err
		}
		if hit {
			out = append(out, entries[i].row)
		}
	}
	return out, nil
}

// probePositions is probe returning the matched build rows' positions, in build order.
func (t *hashJoinTable) probePositions(plan *hashJoinPlan, row storedRow, meter *costMeter) ([]int, error) {
	encoded, entries, err := t.bucket(plan, row, meter)
	if err != nil || entries == nil {
		return nil, err
	}
	out := make([]int, 0, len(entries))
	for i := range entries {
		hit, err := probeEntry(&entries[i], encoded, meter)
		if err != nil {
			return nil, err
		}
		if hit {
			out = append(out, entries[i].pos)
		}
	}
	return out, nil
}

// bucket encodes a probe row's key (charging hash_probe) and returns its hash bucket; nil entries for
// a NULL key, which matches nothing.
func (t *hashJoinTable) bucket(plan *hashJoinPlan, row storedRow, meter *costMeter) ([]byte, []hashJoinEntry, error) {
	indices := make([]int, len(plan.keys))
	types := make([]dataType, len(plan.keys))
	for i, key := range plan.keys {
		indices[i] = key.left - t.probeOffset
		types[i] = key.ty
	}
	encoded, present, err := hashJoinRowKey(row, indices, types, costs.HashProbe, meter)
	if err != nil || !present {
		return nil, nil, err
	}
	entries := t.entries[t.hash(encoded)]
	if entries == nil {
		entries = []hashJoinEntry{}
	}
	return encoded, entries, nil
}

// probeEntry compares one bucket entry's full key bytes with the probe key, charging the compared
// bytes.
func probeEntry(entry *hashJoinEntry, encoded []byte, meter *costMeter) (bool, error) {
	if err := meter.Step(); err != nil {
		return false, err
	}
	work := len(entry.key)
	if len(encoded) < work {
		work = len(encoded)
	}
	if work == 0 {
		work = 1
	}
	meter.Charge(costs.HashProbe * int64(work))
	return bytes.Equal(entry.key, encoded), nil
}

func hashJoinRowKey(row storedRow, indices []int, types []dataType, unit int64, meter *costMeter) ([]byte, bool, error) {
	parts := make([][]byte, len(indices))
	present := true
	for i, index := range indices {
		if err := meter.Step(); err != nil {
			return nil, false, err
		}
		if row[index].Kind == ValNull {
			meter.Charge(unit)
			present = false
			continue
		}
		part, err := encodeTypedKey(types[i], row[index], nil)
		if err != nil {
			return nil, false, err
		}
		charge := int64(len(part))
		if charge == 0 {
			charge = 1
		}
		meter.Charge(unit * charge)
		parts[i] = part
	}
	if !present {
		return nil, false, nil
	}
	var out []byte
	var size [4]byte
	for _, part := range parts {
		binary.BigEndian.PutUint32(size[:], uint32(len(part)))
		out = append(out, size[:]...)
		out = append(out, part...)
	}
	return out, true, nil
}

func hashJoinFNV1a(key []byte) uint64 {
	const offset uint64 = 14695981039346656037
	const prime uint64 = 1099511628211
	h := offset
	for _, b := range key {
		h ^= uint64(b)
		h *= prime
	}
	return h
}
