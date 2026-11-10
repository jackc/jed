package jed

import (
	"hash/maphash"
	"strings"
)

// rowKey is a row's distinctRowKey encoding — the canonical key DISTINCT, GROUP BY, set operations,
// window partitions, and the state maps dedup and group by (spec/design/grammar.md §11) — built
// without copying long string values. A key holding no long string is flat, the encoding itself; any
// other key is segs, consecutive pieces whose concatenation is the encoding, each long text, bytea,
// uuid, or json body a piece of its own that shares the value's string. Whether a key is flat
// depends only on the values, so two equal keys always have the same form, and a flat key never
// equals a segmented one.
type rowKey struct {
	flat string
	segs []string
}

// rowKeySegmentMin is the shortest string body a rowKey shares instead of copying.
const rowKeySegmentMin = 128

// makeRowKey returns row's rowKey.
func makeRowKey(row []Value) rowKey {
	var b rowKeyBuilder
	writeRowKey(&b, row)
	return b.key()
}

// String is the key's flat encoding, distinctRowKey's string.
func (k rowKey) String() string {
	if k.segs == nil {
		return k.flat
	}
	return strings.Join(k.segs, "")
}

// equal reports whether two segmented keys encode the same bytes, comparing them in place.
func (k rowKey) equal(o rowKey) bool {
	n := 0
	for _, s := range k.segs {
		n += len(s)
	}
	for _, s := range o.segs {
		n -= len(s)
	}
	if n != 0 {
		return false
	}
	a, b := k.segs, o.segs
	var as, bs string
	for {
		for as == "" && len(a) > 0 {
			as, a = a[0], a[1:]
		}
		for bs == "" && len(b) > 0 {
			bs, b = b[0], b[1:]
		}
		if as == "" || bs == "" {
			return as == bs
		}
		m := min(len(as), len(bs))
		if as[:m] != bs[:m] {
			return false
		}
		as, bs = as[m:], bs[m:]
	}
}

// rowKeyBuilder accumulates a rowKey: short pieces are copied into b, and each long string body
// closes the pending piece and becomes a segment of its own.
type rowKeyBuilder struct {
	b    strings.Builder
	segs []string
}

func (kb *rowKeyBuilder) WriteByte(c byte) error { return kb.b.WriteByte(c) }

func (kb *rowKeyBuilder) WriteString(s string) { kb.b.WriteString(s) }

// writeBody appends a value's string body, sharing it when it is long.
func (kb *rowKeyBuilder) writeBody(s string) {
	if len(s) < rowKeySegmentMin {
		kb.b.WriteString(s)
		return
	}
	kb.flush()
	kb.segs = append(kb.segs, s)
}

func (kb *rowKeyBuilder) flush() {
	if kb.b.Len() > 0 {
		kb.segs = append(kb.segs, kb.b.String())
		kb.b = strings.Builder{}
	}
}

func (kb *rowKeyBuilder) key() rowKey {
	if kb.segs == nil {
		return rowKey{flat: kb.b.String()}
	}
	kb.flush()
	return rowKey{segs: kb.segs}
}

// keyMap maps rowKeys to values: flat keys in an ordinary map, segmented keys in hash buckets
// compared in place, so a key's long string values are never copied. It is never iterated where the
// order could reach a result (each serves only a spill's private placement).
type keyMap[V any] struct {
	flat  map[string]V
	seed  maphash.Seed
	large map[uint64][]keyMapEntry[V]
	n     int
}

type keyMapEntry[V any] struct {
	key rowKey
	val V
}

func newKeyMap[V any]() *keyMap[V] { return &keyMap[V]{flat: make(map[string]V)} }

func (m *keyMap[V]) hash(k rowKey) uint64 {
	if m.large == nil {
		m.seed = maphash.MakeSeed()
		m.large = make(map[uint64][]keyMapEntry[V])
	}
	var h maphash.Hash
	h.SetSeed(m.seed)
	for _, s := range k.segs {
		h.WriteString(s)
	}
	return h.Sum64()
}

// get returns k's value and whether k is present.
func (m *keyMap[V]) get(k rowKey) (V, bool) {
	if k.segs == nil {
		v, ok := m.flat[k.flat]
		return v, ok
	}
	if m.large != nil {
		for _, e := range m.large[m.hash(k)] {
			if e.key.equal(k) {
				return e.val, true
			}
		}
	}
	var zero V
	return zero, false
}

// has reports whether k is present.
func (m *keyMap[V]) has(k rowKey) bool {
	_, ok := m.get(k)
	return ok
}

// set maps k to v.
func (m *keyMap[V]) set(k rowKey, v V) {
	if k.segs == nil {
		if _, ok := m.flat[k.flat]; !ok {
			m.n++
		}
		m.flat[k.flat] = v
		return
	}
	h := m.hash(k)
	bucket := m.large[h]
	for i := range bucket {
		if bucket[i].key.equal(k) {
			bucket[i].val = v
			return
		}
	}
	m.large[h] = append(bucket, keyMapEntry[V]{key: k, val: v})
	m.n++
}

// add maps k to v if k is absent, reporting whether it was.
func (m *keyMap[V]) add(k rowKey, v V) bool {
	if k.segs == nil {
		if _, ok := m.flat[k.flat]; ok {
			return false
		}
		m.flat[k.flat] = v
		m.n++
		return true
	}
	h := m.hash(k)
	for _, e := range m.large[h] {
		if e.key.equal(k) {
			return false
		}
	}
	m.large[h] = append(m.large[h], keyMapEntry[V]{key: k, val: v})
	m.n++
	return true
}

// len is the number of keys.
func (m *keyMap[V]) len() int { return m.n }

// each visits every entry in an unspecified order.
func (m *keyMap[V]) each(f func(k rowKey, v V) error) error {
	for k, v := range m.flat {
		if err := f(rowKey{flat: k}, v); err != nil {
			return err
		}
	}
	for _, bucket := range m.large {
		for _, e := range bucket {
			if err := f(e.key, e.val); err != nil {
				return err
			}
		}
	}
	return nil
}
