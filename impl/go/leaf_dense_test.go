package jed

// expDense returns a fixed-width region as a plain one, its body the dense full-width slots, for the
// experiment tests that measure regions from their slots. A resident leaf no longer holds them
// (base-plus-offsets and elided key regions are read in place); this builds them on demand.
func expDense(l *paxLeaf, r *paxRegion) *paxRegion {
	if r.width == 0 || (!r.isFOR && !r.elided) {
		return r
	}
	d := *r
	d.body = make([]byte, l.n*r.width)
	if r.elided {
		d.bitmap = make([]byte, (l.n+7)/8)
	}
	for i := 0; i < l.n; i++ {
		if l.isNullRegion(r, i) {
			continue
		}
		var buf [16]byte
		copy(d.body[i*r.width:], l.fixedSlot(r, i, &buf))
	}
	d.isFOR = false
	return &d
}

func (l *paxLeaf) isNullRegion(r *paxRegion, i int) bool {
	return !r.elided && r.bitmap[i/8]&(0x80>>(i%8)) != 0
}
