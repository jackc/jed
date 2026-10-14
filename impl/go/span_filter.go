package jed

// Encoded-span predicates for the columnar lanes (spec/design/packed-leaf.md §11 Track A4).
//
// A columnar lane (aggColumnar / projectColumnar) evaluates its WHERE per stored record while it walks
// the tree. compileSpanPred turns the predicate into a tree that mirrors rExpr.evalFilter exactly —
// each AND node keeps its Guard + `and` charge on entry and its left-then-right short-circuit — whose
// leaves are either a span atom, evaluated on the leaf page's stored bytes without building a value,
// or a generic predicate evaluated by the ordinary evaluator over a reusable scratch row. An atom
// charges exactly what the evaluator charges for its subtree, and whenever its column's bytes are not
// available in place (a Decoded leaf, an out-of-line or compressed value, a body that is not exactly
// well-formed) it evaluates through the ordinary evaluator instead — so results, charges, and errors
// are the evaluator's by construction. Mirrors impl/rust/src/executor/span_filter.rs.

import (
	"bytes"
	"encoding/binary"
	"unicode/utf8"
)

// scratchRow is the row a lane feeds the ordinary evaluator: column c holds record i's resolved value
// once ensure filled it for the current record; a predicate is evaluated only after every column it
// reads was filled. A stamp per column makes a fill happen at most once per record however many
// predicates read the column.
type scratchRow struct {
	vals       storedRow
	stamp      []uint64
	generation uint64
	// allFilled is the record generation whose every touched column a plain filter filled (the
	// columns may still hold deferred values — the evaluator resolves those on touch).
	allFilled uint64
}

func newScratchRow(width int) *scratchRow {
	return &scratchRow{vals: make(storedRow, width), stamp: make([]uint64, width), generation: 1}
}

// nextRecord starts a new record: every column's fill becomes stale.
func (s *scratchRow) nextRecord() { s.generation++ }

// ensure resolves column c of record i into the scratch row, once per record — the inline decode,
// chain read, or decompression the row path's resolveColumns performs.
func (s *scratchRow) ensure(n *pnode, i, c int) error {
	if s.stamp[c] == s.generation {
		return nil
	}
	v, err := n.colAt(i, c)
	if err != nil {
		return err
	}
	if v.Kind == ValUnfetched {
		if v, err = resolveUnfetchedSelf(v.unfetched()); err != nil {
			return err
		}
	}
	s.vals[c] = v
	s.stamp[c] = s.generation
	return nil
}

// read returns record i's resolved column c: the value a predicate already resolved for this record,
// else read straight from the leaf (not stored). Callers use it only for a variable-width column read
// beside a predicate — where reusing the predicate's decode pays — and readLeafColumn otherwise.
func (s *scratchRow) read(n *pnode, i, c int) (Value, error) {
	if s.allFilled == s.generation {
		if v := s.vals[c]; v.Kind == ValUnfetched {
			return resolveUnfetchedSelf(v.unfetched())
		}
		return s.vals[c], nil
	}
	if s.stamp[c] == s.generation {
		return s.vals[c], nil
	}
	return readLeafColumn(n, i, c)
}

// readLeafColumn returns record i's column c read straight from the leaf, a deferred value resolved.
func readLeafColumn(n *pnode, i, c int) (Value, error) {
	v, err := n.colAt(i, c)
	if err != nil || v.Kind != ValUnfetched {
		return v, err
	}
	return resolveUnfetchedSelf(v.unfetched())
}

// laneReuse reports, per column, whether a lane should reuse the value its filter resolved (a
// variable-width column beside a filter) rather than read the leaf again (a fixed-width slot decodes
// cheaper than the reuse costs).
func laneReuse(colTypes []colType, filtered bool) []bool {
	reuse := make([]bool, len(colTypes))
	for c, ty := range colTypes {
		_, fixed := fixedValueWidth(ty)
		reuse[c] = filtered && !fixed
	}
	return reuse
}

// laneFilter is a lane's compiled WHERE: the span evaluator when the predicate has at least one span
// atom, else the original lane filter — every touched column read into the scratch row (a deferred
// value resolves when the evaluator touches it), then rExpr.evalFilter — which is cheaper for a
// predicate the span compiler cannot help with.
type laneFilter struct {
	span  *spanPred
	plain *rExpr
	cols  []int
}

func compileLaneFilter(filter *rExpr, colTypes []colType, mask []bool, env *evalEnv) *laneFilter {
	if p := compileSpanPred(filter, colTypes, mask, env); p.hasAtom() {
		return &laneFilter{span: p}
	}
	f := &laneFilter{plain: filter}
	for c := range mask {
		if mask[c] {
			f.cols = append(f.cols, c)
		}
	}
	return f
}

// keep reports whether record i of n passes, starting a new scratch record.
func (f *laneFilter) keep(n *pnode, i int, scratch *scratchRow, env *evalEnv, m *costMeter) (bool, error) {
	scratch.nextRecord()
	if f.span != nil {
		return f.span.evalFilter(n, i, scratch, env, m)
	}
	for _, c := range f.cols {
		v, err := n.colAt(i, c)
		if err != nil {
			return false, err
		}
		scratch.vals[c] = v
	}
	scratch.allFilled = scratch.generation
	return f.plain.evalFilter(scratch.vals, env, m)
}

type spanPredKind uint8

const (
	spanPredAnd spanPredKind = iota
	spanPredAtom
	spanPredGeneric
)

// spanPred is a compiled lane predicate (the file comment).
type spanPred struct {
	kind     spanPredKind
	lhs, rhs *spanPred // spanPredAnd
	atom     *spanAtom // spanPredAtom
	expr     *rExpr    // spanPredGeneric
	// cols are the touched columns a generic predicate reads (its collectTouched set — the same
	// analysis that built the lane's mask).
	cols []int
}

type spanAtomKind uint8

const (
	// atomNullTest: col IS [NOT] NULL over a non-composite column.
	atomNullTest spanAtomKind = iota
	// atomBytesEq: col = k / col <> k over text or bytea.
	atomBytesEq
	// atomArrayOp: array containment / overlap against a constant array.
	atomArrayOp
)

type arrayOpKind uint8

const (
	arrayColContainsK arrayOpKind = iota // col @> k (or k <@ col)
	arrayKContainsCol                    // col <@ k (or k @> col)
	arrayOverlaps                        // col && k
)

// elemCodec delimits an array element body — the element types whose value equality is byte
// equality of their stored bodies (strict element equality over text/bytea is byte order; integers
// are a fixed-width bijective codec). width == 0 is a u16-length-prefixed body.
type elemCodec struct {
	width int  // > 0: a fixed-width big-endian integer body
	text  bool // a text element (validated UTF-8)
}

// spanAtom is a predicate leaf evaluated on its column's stored bytes. expr is the original leaf,
// evaluated by the ordinary evaluator when the bytes are not available in place.
type spanAtom struct {
	expr    *rExpr
	col     int
	kind    spanAtomKind
	negated bool // atomNullTest, atomBytesEq
	// atomBytesEq: k is the constant's payload bytes, kWidth its varlen_compare width (code points or
	// bytes). atomArrayOp: kElems holds the constant's non-NULL element bodies (encoded exactly as
	// stored), kHasNull whether it has a NULL element. base is the operator charge plus the constant
	// operand's replayed charge.
	text     bool
	k        []byte
	kWidth   int
	op       arrayOpKind
	elem     elemCodec
	kElems   [][]byte
	kHasNull bool
	base     int64
	spans    [][2]int
}

// compileSpanPred compiles filter for a lane over a single base relation whose column types are
// colTypes and whose touched set is mask. Never fails: a leaf the span evaluator does not recognize
// stays a generic predicate.
func compileSpanPred(filter *rExpr, colTypes []colType, mask []bool, env *evalEnv) *spanPred {
	if filter.kind == reAnd {
		return &spanPred{
			kind: spanPredAnd,
			lhs:  compileSpanPred(filter.lhs, colTypes, mask, env),
			rhs:  compileSpanPred(filter.rhs, colTypes, mask, env),
		}
	}
	if a := compileSpanAtom(filter, colTypes, mask, env); a != nil {
		return &spanPred{kind: spanPredAtom, atom: a}
	}
	reads := make([]bool, len(mask))
	collectTouched(filter, 0, reads)
	var cols []int
	for c := range mask {
		if reads[c] && mask[c] {
			cols = append(cols, c)
		}
	}
	return &spanPred{kind: spanPredGeneric, expr: filter, cols: cols}
}

// hasAtom reports whether the predicate has at least one span atom.
func (p *spanPred) hasAtom() bool {
	switch p.kind {
	case spanPredAnd:
		return p.lhs.hasAtom() || p.rhs.hasAtom()
	case spanPredAtom:
		return true
	}
	return false
}

// evalFilter reports whether record i of n passes — rExpr.evalFilter over the record's row, with
// identical charges.
func (p *spanPred) evalFilter(n *pnode, i int, scratch *scratchRow, env *evalEnv, m *costMeter) (bool, error) {
	switch p.kind {
	case spanPredAnd:
		if err := m.Guard(); err != nil {
			return false, err
		}
		m.Charge(operatorCost("and"))
		keep, err := p.lhs.evalFilter(n, i, scratch, env, m)
		if err != nil || !keep {
			return false, err
		}
		return p.rhs.evalFilter(n, i, scratch, env, m)
	case spanPredAtom:
		keep, ok, err := p.atom.evalSpan(n, i, m)
		if err != nil || ok {
			return keep, err
		}
		if err := scratch.ensure(n, i, p.atom.col); err != nil {
			return false, err
		}
		v, err := p.atom.expr.eval(scratch.vals, env, m)
		return v.IsTrue(), err
	default:
		for _, c := range p.cols {
			if err := scratch.ensure(n, i, c); err != nil {
				return false, err
			}
		}
		v, err := p.expr.eval(scratch.vals, env, m)
		return v.IsTrue(), err
	}
}

// evalSpan evaluates on the stored bytes, charging exactly what the evaluator charges for this leaf;
// ok=false (charging nothing) when the bytes are not available in place, so the caller evaluates the
// leaf through the evaluator.
func (a *spanAtom) evalSpan(n *pnode, i int, m *costMeter) (keep, ok bool, err error) {
	if a.kind == atomNullTest {
		if err := m.Guard(); err != nil {
			return false, false, err
		}
		m.Charge(costs.OperatorEval)
		return n.colIsNull(i, a.col) != a.negated, true, nil
	}
	span, present, packed := n.colSpan(i, a.col)
	if !packed {
		return false, false, nil
	}
	if !present {
		// A NULL operand: the operator is NULL (not TRUE) and charges no length work.
		if err := m.Guard(); err != nil {
			return false, false, err
		}
		m.Charge(a.base)
		return false, true, nil
	}
	body, inline := inlinePlainBody(span)
	if !inline {
		return false, false, nil
	}
	if a.kind == atomBytesEq {
		payload, ok := len16Payload(body)
		if !ok {
			return false, false, nil
		}
		width := len(payload)
		if a.text {
			if !utf8.Valid(payload) {
				return false, false, nil
			}
			width = utf8.RuneCount(payload)
		}
		if err := m.Guard(); err != nil {
			return false, false, err
		}
		m.Charge(a.base)
		w := min(width, a.kWidth)
		if w < 1 {
			w = 1
		}
		m.Charge(costs.VarlenCompare * int64(w-1))
		if err := m.Guard(); err != nil {
			return false, false, err
		}
		return bytes.Equal(payload, a.k) != a.negated, true, nil
	}
	colHasNull, ok := arrayElementSpans(body, a.elem, &a.spans)
	if !ok {
		return false, false, nil
	}
	if err := m.Guard(); err != nil {
		return false, false, err
	}
	m.Charge(a.base)
	inCol := func(kb []byte) bool {
		for _, s := range a.spans {
			if bytes.Equal(body[s[0]:s[1]], kb) {
				return true
			}
		}
		return false
	}
	inK := func(s [2]int) bool {
		for _, kb := range a.kElems {
			if bytes.Equal(body[s[0]:s[1]], kb) {
				return true
			}
		}
		return false
	}
	switch a.op {
	case arrayColContainsK:
		// Every k element (a NULL one matches nothing) equals some column element.
		if a.kHasNull {
			return false, true, nil
		}
		for _, kb := range a.kElems {
			if !inCol(kb) {
				return false, true, nil
			}
		}
		return true, true, nil
	case arrayKContainsCol:
		// Every column element (a NULL one matches nothing) equals some k element.
		if colHasNull {
			return false, true, nil
		}
		for _, s := range a.spans {
			if !inK(s) {
				return false, true, nil
			}
		}
		return true, true, nil
	default:
		for _, s := range a.spans {
			if inK(s) {
				return true, true, nil
			}
		}
		return false, true, nil
	}
}

// len16Payload returns the payload of a u16-length-prefixed body (text / bytea), only when the body
// is exactly the prefix plus that many bytes.
func len16Payload(body []byte) ([]byte, bool) {
	if len(body) < 2 || len(body)-2 != int(binary.BigEndian.Uint16(body)) {
		return nil, false
	}
	return body[2:], true
}

// arrayElementSpans walks an inline array body (spec/design/array.md §4) and collects its non-NULL
// element bodies as [start, end) offsets into body; it reports whether any element is NULL. ok=false
// unless the body is exactly well-formed — the header the decoder accepts, every element in bounds,
// text elements valid UTF-8, and no trailing bytes — so a malformed body falls back to the decoder,
// which reports it.
func arrayElementSpans(body []byte, elem elemCodec, spans *[][2]int) (anyNull, ok bool) {
	*spans = (*spans)[:0]
	if len(body) < 2 {
		return false, false
	}
	ndim, flags := int(body[0]), body[1]
	if flags&^0x01 != 0 {
		return false, false
	}
	pos := 2
	if ndim == 0 {
		return false, pos == len(body)
	}
	if ndim > 6 {
		return false, false
	}
	n := 1
	for d := 0; d < ndim; d++ {
		if pos+8 > len(body) {
			return false, false
		}
		dim := int(binary.BigEndian.Uint32(body[pos:]))
		if dim != 0 && n > int(^uint(0)>>1)/dim {
			return false, false
		}
		n *= dim
		pos += 8 // length + lower bound
	}
	hasNulls := flags&0x01 != 0
	var bitmap []byte
	if hasNulls {
		nb := (n + 7) / 8
		if nb > len(body)-pos {
			return false, false
		}
		bitmap = body[pos : pos+nb]
		pos += nb
	}
	for e := 0; e < n; e++ {
		if hasNulls && bitmap[e/8]&(0x80>>(e%8)) != 0 {
			anyNull = true
			continue
		}
		end := pos + elem.width
		if elem.width == 0 {
			if pos+2 > len(body) {
				return false, false
			}
			end = pos + 2 + int(binary.BigEndian.Uint16(body[pos:]))
		}
		if end > len(body) {
			return false, false
		}
		if elem.text && !utf8.Valid(body[pos+2:end]) {
			return false, false
		}
		*spans = append(*spans, [2]int{pos, end})
		pos = end
	}
	return anyNull, pos == len(body)
}

// constantOperand evaluates a row-independent constant operand once: its value and the charge the
// evaluator accrues evaluating it (replayed per row). Only literal shapes qualify — a text / bytea /
// integer / NULL literal, a folded array constant, a bind parameter, or a flat ARRAY[...] of those —
// whose evaluation charges at most the constructor's operator_eval and reserves nothing.
func constantOperand(e *rExpr, env *evalEnv) (Value, int64, bool) {
	literal := func(e *rExpr) bool {
		switch e.kind {
		case reConstText, reConstBytea, reConstInt, reConstNull, reConstArray, reParam:
			return true
		}
		return false
	}
	if e.kind == reArray {
		if e.nested {
			return Value{}, 0, false
		}
		for _, el := range e.sargs {
			if !literal(el) {
				return Value{}, 0, false
			}
		}
	} else if !literal(e) {
		return Value{}, 0, false
	}
	m := newMeter()
	v, err := e.eval(nil, env, m)
	if err != nil {
		return Value{}, 0, false
	}
	return v, m.Accrued, true
}

func compileSpanAtom(e *rExpr, colTypes []colType, mask []bool, env *evalEnv) *spanAtom {
	column := func(x *rExpr) (int, bool) {
		if x != nil && x.kind == reColumn && x.index >= 0 && x.index < len(mask) && mask[x.index] && x.index < len(colTypes) {
			return x.index, true
		}
		return 0, false
	}
	isScalar := func(t colType) bool { return !t.Composite && t.Elem == nil && t.RangeElem == nil }
	switch e.kind {
	case reIsNull:
		col, ok := column(e.operand)
		// A composite's IS NULL is the all-fields rule (composite.md §5) — not nullness.
		if !ok || colTypes[col].Composite {
			return nil
		}
		return &spanAtom{expr: e, col: col, kind: atomNullTest, negated: e.negated}
	case reCompare:
		if e.op != opEq && e.op != opNe {
			return nil
		}
		col, other := 0, (*rExpr)(nil)
		if c, ok := column(e.lhs); ok {
			if _, ok2 := column(e.rhs); ok2 {
				return nil
			}
			col, other = c, e.rhs
		} else if c, ok := column(e.rhs); ok {
			col, other = c, e.lhs
		} else {
			return nil
		}
		k, charge, ok := constantOperand(other, env)
		if !ok {
			return nil
		}
		t := colTypes[col]
		a := &spanAtom{expr: e, col: col, kind: atomBytesEq, negated: e.op == opNe, base: operatorCost(e.op.catalogName()) + charge}
		switch {
		case isScalar(t) && t.Scalar.IsText() && k.Kind == ValText:
			a.text, a.k, a.kWidth = true, []byte(k.str()), utf8.RuneCountInString(k.str())
		case isScalar(t) && t.Scalar.IsBytea() && k.Kind == ValBytea:
			a.k, a.kWidth = []byte(k.str()), len(k.str())
		default:
			return nil
		}
		return a
	case reArrayFunc:
		if len(e.sargs) != 2 {
			return nil
		}
		var col int
		var other *rExpr
		colFirst := false
		if c, ok := column(e.sargs[0]); ok {
			if _, ok2 := column(e.sargs[1]); ok2 {
				return nil
			}
			col, other, colFirst = c, e.sargs[1], true
		} else if c, ok := column(e.sargs[1]); ok {
			col, other = c, e.sargs[0]
		} else {
			return nil
		}
		var op arrayOpKind
		switch {
		case (e.afunc == afContains && colFirst) || (e.afunc == afContainedBy && !colFirst):
			op = arrayColContainsK
		case (e.afunc == afContains && !colFirst) || (e.afunc == afContainedBy && colFirst):
			op = arrayKContainsCol
		case e.afunc == afOverlaps:
			op = arrayOverlaps
		default:
			return nil
		}
		t := colTypes[col]
		if t.Elem == nil || !isScalar(*t.Elem) {
			return nil
		}
		es := t.Elem.Scalar
		var elem elemCodec
		switch {
		case es.IsText():
			elem = elemCodec{text: true}
		case es.IsBytea():
			elem = elemCodec{}
		case es.IsInteger():
			elem = elemCodec{width: es.WidthBytes()}
		default:
			return nil
		}
		k, charge, ok := constantOperand(other, env)
		if !ok || k.Kind != ValArray {
			return nil
		}
		a := &spanAtom{expr: e, col: col, kind: atomArrayOp, op: op, elem: elem, base: costs.OperatorEval + charge}
		for _, v := range k.arrayVal().Elements {
			var fits bool
			switch {
			case v.IsNull():
				a.kHasNull = true
				continue
			case es.IsText() && v.Kind == ValText, es.IsBytea() && v.Kind == ValBytea:
				fits = len(v.str()) <= 0xFFFF
			case es.IsInteger() && v.Kind == ValInt:
				fits = es.InRange(v.Int)
			}
			if !fits {
				return nil
			}
			a.kElems = append(a.kElems, encodeValue(*t.Elem, v)[1:])
		}
		return a
	}
	return nil
}
