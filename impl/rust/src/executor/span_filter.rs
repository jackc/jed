//! Encoded-span predicates for the columnar lanes (spec/design/packed-leaf.md §11 Track A4).
//!
//! A columnar lane (`agg_columnar` / `project_columnar`) evaluates its `WHERE` per stored record while
//! it walks the tree. [`SpanPred::compile`] turns the predicate into a tree that mirrors
//! [`RExpr::eval_filter`] exactly — each `AND` node keeps its guard + `and` charge on entry and its
//! left-then-right short-circuit — whose leaves are either a [`SpanAtom`], evaluated on the leaf page's
//! stored bytes without building a value, or a generic predicate evaluated by the ordinary evaluator
//! over a reusable masked scratch row. An atom charges exactly what the evaluator charges for its
//! subtree, and whenever its column's bytes are not available in place (a Decoded leaf, an out-of-line
//! or compressed value, a body that is not exactly well-formed) it evaluates through the ordinary
//! evaluator instead — so results, charges, and errors are the evaluator's by construction.

use super::*;
use crate::pmap::Node;

/// The scratch row a lane feeds the ordinary evaluator: column `c` holds record `i`'s resolved value
/// once [`ensure`](ScratchRow::ensure) filled it for the current record; a predicate is evaluated only
/// after every column it reads was filled. A stamp per column makes a fill happen at most once per
/// record however many predicates read the column.
pub(crate) struct ScratchRow {
    pub(crate) vals: Vec<Value>,
    stamp: Vec<u64>,
    generation: u64,
    /// The record generation whose every touched column a plain filter filled (the columns may still
    /// hold deferred values — the evaluator resolves those on touch).
    all_filled: u64,
}

impl ScratchRow {
    pub(crate) fn new(width: usize) -> ScratchRow {
        ScratchRow {
            vals: vec![Value::Null; width],
            stamp: vec![0; width],
            generation: 1,
            all_filled: 0,
        }
    }

    /// Start a new record: every column's fill becomes stale.
    #[inline]
    pub(crate) fn next_record(&mut self) {
        self.generation += 1;
    }

    /// Resolve column `c` of record `i` into the scratch row, once per record — the inline decode,
    /// chain read, or decompression the row path's `resolve_columns` performs.
    #[inline]
    pub(crate) fn ensure(&mut self, node: &Node, i: usize, c: usize) -> Result<()> {
        if self.stamp[c] != self.generation {
            self.vals[c] = match node.col_at(i, c)? {
                Value::Unfetched(u) => crate::format::resolve_unfetched_self(&u)?,
                v => v,
            };
            self.stamp[c] = self.generation;
        }
        Ok(())
    }

    /// Record `i`'s resolved column `c`: the value a predicate already resolved for this record, else
    /// read straight from the leaf (not stored). Callers use it only for a variable-width column read
    /// beside a predicate — where reusing the predicate's decode pays — and read the leaf directly
    /// otherwise.
    pub(crate) fn read(&self, node: &Node, i: usize, c: usize) -> Result<Value> {
        if self.all_filled == self.generation {
            return match &self.vals[c] {
                Value::Unfetched(u) => crate::format::resolve_unfetched_self(u),
                v => Ok(v.clone()),
            };
        }
        if self.stamp[c] == self.generation {
            return Ok(self.vals[c].clone());
        }
        match node.col_at(i, c)? {
            Value::Unfetched(u) => crate::format::resolve_unfetched_self(&u),
            v => Ok(v),
        }
    }
}

/// A lane's compiled `WHERE`: the span evaluator when the predicate has at least one span atom, else
/// the original lane filter — every touched column read into the scratch row (a deferred value
/// resolves when the evaluator touches it), then [`RExpr::eval_filter`] — which is cheaper for a
/// predicate the span compiler cannot help with.
pub(crate) enum LaneFilter<'a> {
    Span(SpanPred<'a>),
    Plain {
        expr: &'a RExpr,
        cols: Vec<usize>,
        #[cfg(all(feature = "expression-vm", not(feature = "expression-vm-baseline")))]
        vm: Option<std::cell::RefCell<expression_vm::Runner>>,
    },
}

impl<'a> LaneFilter<'a> {
    pub(crate) fn compile(
        filter: &'a RExpr,
        col_types: &[ColType],
        mask: &[bool],
        env: &EvalEnv,
    ) -> LaneFilter<'a> {
        let pred = SpanPred::compile(filter, col_types, mask, env);
        if pred.has_atom() {
            return LaneFilter::Span(pred);
        }
        LaneFilter::Plain {
            expr: filter,
            cols: (0..mask.len()).filter(|&c| mask[c]).collect(),
            #[cfg(all(feature = "expression-vm", not(feature = "expression-vm-baseline")))]
            vm: expression_vm::Runner::compile_lane(filter, col_types, env.params)
                .map(std::cell::RefCell::new),
        }
    }

    /// Whether record `i` of `node` passes, starting a new scratch record.
    pub(crate) fn keep(
        &self,
        node: &Node,
        i: usize,
        scratch: &mut ScratchRow,
        env: &EvalEnv,
        m: &mut Meter,
    ) -> Result<bool> {
        scratch.next_record();
        match self {
            LaneFilter::Span(p) => p.eval_filter(node, i, scratch, env, m),
            LaneFilter::Plain { expr, cols, .. } => {
                for &c in cols {
                    scratch.vals[c] = node.col_at(i, c)?;
                }
                scratch.all_filled = scratch.generation;
                #[cfg(all(feature = "expression-vm", not(feature = "expression-vm-baseline")))]
                if let LaneFilter::Plain { vm: Some(vm), .. } = self {
                    return vm
                        .borrow_mut()
                        .eval(&scratch.vals, env, m)
                        .map(|v| v.is_true());
                }
                expr.eval_filter(&scratch.vals, env, m)
            }
        }
    }
}

/// A compiled lane predicate (the module doc).
pub(crate) enum SpanPred<'a> {
    And(Box<SpanPred<'a>>, Box<SpanPred<'a>>),
    Atom(SpanAtom<'a>),
    /// A predicate for the ordinary evaluator, with the touched columns it reads (its
    /// `collect_touched` set — the same analysis that built the lane's mask).
    Generic(&'a RExpr, Vec<usize>),
}

/// A predicate leaf evaluated on its column's stored bytes. `expr` is the original leaf, evaluated
/// by the ordinary evaluator when the bytes are not available in place.
pub(crate) struct SpanAtom<'a> {
    expr: &'a RExpr,
    col: usize,
    kind: AtomKind,
}

enum AtomKind {
    /// `col IS [NOT] NULL` over a non-composite column.
    NullTest { negated: bool },
    /// `col = k` / `col <> k` over `text` or `bytea`: `k` is the constant's payload bytes, `k_width`
    /// its `varlen_compare` width (code points or bytes), `base` the operator charge plus the
    /// constant operand's replayed charge.
    BytesEq {
        negated: bool,
        text: bool,
        k: Vec<u8>,
        k_width: usize,
        base: i64,
    },
    /// Array containment / overlap against a constant array: `k` holds the constant's non-NULL
    /// element bodies (encoded exactly as stored), `k_has_null` whether it has a NULL element.
    ArrayOp {
        op: ArrayOpKind,
        elem: ElemCodec,
        k: Vec<Vec<u8>>,
        k_has_null: bool,
        base: i64,
        spans: std::cell::RefCell<Vec<(usize, usize)>>,
    },
}

#[derive(Clone, Copy)]
enum ArrayOpKind {
    /// `col @> k` (or `k <@ col`).
    ColContainsK,
    /// `col <@ k` (or `k @> col`).
    KContainsCol,
    /// `col && k`.
    Overlaps,
}

/// How an array element body is delimited — the element types whose value equality is byte
/// equality of their stored bodies (`strict_elem_eq` over text/bytea is byte order; integers are a
/// fixed-width bijective codec).
#[derive(Clone, Copy)]
enum ElemCodec {
    /// `u16` length + UTF-8 bytes.
    Text,
    /// `u16` length + bytes.
    Bytea,
    /// A fixed-width big-endian integer body.
    Fixed(usize),
}

impl<'a> SpanPred<'a> {
    /// Compile `filter` for a lane over a single base relation whose column types are `col_types`
    /// and whose touched set is `mask`. Never fails: a leaf the span evaluator does not recognize
    /// stays a generic predicate.
    pub(crate) fn compile(
        filter: &'a RExpr,
        col_types: &[ColType],
        mask: &[bool],
        env: &EvalEnv,
    ) -> SpanPred<'a> {
        match filter {
            RExpr::And(l, r) => SpanPred::And(
                Box::new(SpanPred::compile(l, col_types, mask, env)),
                Box::new(SpanPred::compile(r, col_types, mask, env)),
            ),
            e => match compile_atom(e, col_types, mask, env) {
                Some(a) => SpanPred::Atom(a),
                None => {
                    let mut reads = vec![false; mask.len()];
                    collect_touched(e, 0, &mut reads);
                    let cols = (0..mask.len()).filter(|&c| reads[c] && mask[c]).collect();
                    SpanPred::Generic(e, cols)
                }
            },
        }
    }

    /// Whether the predicate has at least one span atom.
    fn has_atom(&self) -> bool {
        match self {
            SpanPred::And(l, r) => l.has_atom() || r.has_atom(),
            SpanPred::Atom(_) => true,
            SpanPred::Generic(..) => false,
        }
    }

    /// Whether record `i` of `node` passes — `RExpr::eval_filter` over the record's row, with
    /// identical charges.
    pub(crate) fn eval_filter(
        &self,
        node: &Node,
        i: usize,
        scratch: &mut ScratchRow,
        env: &EvalEnv,
        m: &mut Meter,
    ) -> Result<bool> {
        match self {
            SpanPred::And(l, r) => {
                m.guard()?;
                m.charge(operator_cost("and"));
                Ok(l.eval_filter(node, i, scratch, env, m)?
                    && r.eval_filter(node, i, scratch, env, m)?)
            }
            SpanPred::Atom(a) => {
                if let Some(keep) = a.eval_span(node, i, m)? {
                    return Ok(keep);
                }
                scratch.ensure(node, i, a.col)?;
                Ok(a.expr.eval(&scratch.vals, env, m)?.is_true())
            }
            SpanPred::Generic(e, cols) => {
                for &c in cols {
                    scratch.ensure(node, i, c)?;
                }
                Ok(e.eval(&scratch.vals, env, m)?.is_true())
            }
        }
    }
}

impl SpanAtom<'_> {
    /// Evaluate on the stored bytes, charging exactly what the evaluator charges for this leaf;
    /// `None` (charging nothing) when the bytes are not available in place, so the caller evaluates
    /// the leaf through the evaluator.
    fn eval_span(&self, node: &Node, i: usize, m: &mut Meter) -> Result<Option<bool>> {
        match &self.kind {
            AtomKind::NullTest { negated } => {
                m.guard()?;
                m.charge(COSTS.operator_eval);
                Ok(Some(node.col_is_null(i, self.col) != *negated))
            }
            AtomKind::BytesEq {
                negated,
                text,
                k,
                k_width,
                base,
            } => {
                let Some(span) = node.col_span(i, self.col) else {
                    return Ok(None);
                };
                let Some(span) = span else {
                    // A NULL operand: the comparison is NULL (not TRUE) and charges no length work.
                    m.guard()?;
                    m.charge(*base);
                    return Ok(Some(false));
                };
                let Some(payload) = crate::format::inline_plain_body(span).and_then(len16_payload)
                else {
                    return Ok(None);
                };
                let width = if *text {
                    match std::str::from_utf8(payload) {
                        Ok(s) => s.chars().count(),
                        Err(_) => return Ok(None),
                    }
                } else {
                    payload.len()
                };
                m.guard()?;
                m.charge(*base);
                m.charge(COSTS.varlen_compare * (width.min(*k_width).max(1) as i64 - 1));
                m.guard()?;
                Ok(Some((payload == k.as_slice()) != *negated))
            }
            AtomKind::ArrayOp {
                op,
                elem,
                k,
                k_has_null,
                base,
                spans,
            } => {
                let Some(span) = node.col_span(i, self.col) else {
                    return Ok(None);
                };
                let Some(span) = span else {
                    // A NULL array operand: the operator is NULL (not TRUE).
                    m.guard()?;
                    m.charge(*base);
                    return Ok(Some(false));
                };
                let Some(body) = crate::format::inline_plain_body(span) else {
                    return Ok(None);
                };
                let mut spans = spans.borrow_mut();
                let Some(col_has_null) = array_element_spans(body, *elem, &mut spans) else {
                    return Ok(None);
                };
                m.guard()?;
                m.charge(*base);
                let elem_at = |s: &(usize, usize)| &body[s.0..s.1];
                let in_col = |kb: &[u8]| spans.iter().any(|s| elem_at(s) == kb);
                let in_k = |s: &(usize, usize)| k.iter().any(|kb| kb.as_slice() == elem_at(s));
                let keep = match op {
                    // Every k element (a NULL one matches nothing) equals some column element.
                    ArrayOpKind::ColContainsK => !*k_has_null && k.iter().all(|kb| in_col(kb)),
                    // Every column element (a NULL one matches nothing) equals some k element.
                    ArrayOpKind::KContainsCol => !col_has_null && spans.iter().all(in_k),
                    ArrayOpKind::Overlaps => spans.iter().any(in_k),
                };
                Ok(Some(keep))
            }
        }
    }
}

/// The payload of a `u16`-length-prefixed body (text / bytea), only when the body is exactly the
/// prefix plus that many bytes.
fn len16_payload(body: &[u8]) -> Option<&[u8]> {
    let (len, rest) = body.split_first_chunk::<2>()?;
    (rest.len() == u16::from_be_bytes(*len) as usize).then_some(rest)
}

/// Walk an inline array body (spec/design/array.md §4) and collect its non-NULL element bodies as
/// `(start, end)` offsets into `body`; returns whether any element is NULL. `None` unless the body is
/// exactly well-formed — the header the decoder accepts, every element in bounds, text elements valid
/// UTF-8, and no trailing bytes — so a malformed body falls back to the decoder, which reports it.
fn array_element_spans(
    body: &[u8],
    elem: ElemCodec,
    spans: &mut Vec<(usize, usize)>,
) -> Option<bool> {
    spans.clear();
    let (&ndim, rest) = body.split_first()?;
    let (&flags, _) = rest.split_first()?;
    if flags & !0x01 != 0 {
        return None;
    }
    let mut pos = 2usize;
    if ndim == 0 {
        return (pos == body.len()).then_some(false);
    }
    if ndim > 6 {
        return None;
    }
    let mut n = 1usize;
    for _ in 0..ndim {
        let len = body.get(pos..pos + 4)?;
        n = n.checked_mul(u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize)?;
        pos += 8; // length + lower bound
    }
    if pos > body.len() {
        return None;
    }
    let has_nulls = flags & 0x01 != 0;
    let bitmap = if has_nulls {
        let b = body.get(pos..pos + n.div_ceil(8))?;
        pos += b.len();
        b
    } else {
        &[]
    };
    let mut any_null = false;
    for e in 0..n {
        if has_nulls && bitmap[e / 8] & (0x80 >> (e % 8)) != 0 {
            any_null = true;
            continue;
        }
        let end = match elem {
            ElemCodec::Text | ElemCodec::Bytea => {
                let len = body.get(pos..pos + 2)?;
                pos + 2 + u16::from_be_bytes([len[0], len[1]]) as usize
            }
            ElemCodec::Fixed(w) => pos + w,
        };
        let bytes = body.get(pos..end)?;
        if matches!(elem, ElemCodec::Text) && std::str::from_utf8(&bytes[2..]).is_err() {
            return None;
        }
        spans.push((pos, end));
        pos = end;
    }
    (pos == body.len()).then_some(any_null)
}

/// A row-independent constant operand, evaluated once: its value and the charge the evaluator
/// accrues evaluating it (replayed per row). Only literal shapes qualify — a text / bytea / integer /
/// NULL literal, a folded array constant, a bind parameter, or a flat `ARRAY[...]` of those — whose
/// evaluation charges at most the constructor's `operator_eval` and reserves nothing.
fn constant_operand(e: &RExpr, env: &EvalEnv) -> Option<(Value, i64)> {
    fn literal(e: &RExpr) -> bool {
        matches!(
            e,
            RExpr::ConstText(_)
                | RExpr::ConstBytea(_)
                | RExpr::ConstInt(_)
                | RExpr::ConstNull
                | RExpr::ConstArray(_)
                | RExpr::Param(_)
        )
    }
    let ok = match e {
        RExpr::Array { elems, nested } => !*nested && elems.iter().all(literal),
        e => literal(e),
    };
    if !ok {
        return None;
    }
    let mut m = Meter::new();
    let v = e.eval(&[], env, &mut m).ok()?;
    Some((v, m.accrued))
}

fn compile_atom<'a>(
    e: &'a RExpr,
    col_types: &[ColType],
    mask: &[bool],
    env: &EvalEnv,
) -> Option<SpanAtom<'a>> {
    let column = |x: &RExpr| match x {
        RExpr::Column(c) if *c < mask.len() && mask[*c] && *c < col_types.len() => Some(*c),
        _ => None,
    };
    match e {
        RExpr::IsNull { operand, negated } => {
            let col = column(operand)?;
            // A composite's IS NULL is the all-fields rule (composite.md §5) — not nullness.
            if matches!(col_types[col], ColType::Composite { .. }) {
                return None;
            }
            Some(SpanAtom {
                expr: e,
                col,
                kind: AtomKind::NullTest { negated: *negated },
            })
        }
        RExpr::Compare { op, lhs, rhs, .. } => {
            let negated = match op {
                CmpOp::Eq => false,
                CmpOp::Ne => true,
                _ => return None,
            };
            let (col, other) = match (column(lhs), column(rhs)) {
                (Some(c), None) => (c, rhs),
                (None, Some(c)) => (c, lhs),
                _ => return None,
            };
            let (k, charge) = constant_operand(other, env)?;
            let (text, k, k_width) = match (&col_types[col], k) {
                (ColType::Scalar(s), Value::Text(t)) if s.is_text() => {
                    let w = t.chars().count();
                    (true, t.into_bytes(), w)
                }
                (ColType::Scalar(s), Value::Bytea(b)) if s.is_bytea() => {
                    let w = b.len();
                    (false, b, w)
                }
                _ => return None,
            };
            Some(SpanAtom {
                expr: e,
                col,
                kind: AtomKind::BytesEq {
                    negated,
                    text,
                    k,
                    k_width,
                    base: operator_cost(op.op_name()) + charge,
                },
            })
        }
        RExpr::ArrayFunc { func, args } if args.len() == 2 => {
            let (col, other, col_first) = match (column(&args[0]), column(&args[1])) {
                (Some(c), None) => (c, &args[1], true),
                (None, Some(c)) => (c, &args[0], false),
                _ => return None,
            };
            let op = match (func, col_first) {
                (ArrayFunc::Contains, true) | (ArrayFunc::ContainedBy, false) => {
                    ArrayOpKind::ColContainsK
                }
                (ArrayFunc::Contains, false) | (ArrayFunc::ContainedBy, true) => {
                    ArrayOpKind::KContainsCol
                }
                (ArrayFunc::Overlaps, _) => ArrayOpKind::Overlaps,
                _ => return None,
            };
            let ColType::Array(elem_ty) = &col_types[col] else {
                return None;
            };
            let (elem, scalar) = match elem_ty.as_ref() {
                ColType::Scalar(s) if s.is_text() => (ElemCodec::Text, *s),
                ColType::Scalar(s) if s.is_bytea() => (ElemCodec::Bytea, *s),
                ColType::Scalar(s) if s.is_integer() => (ElemCodec::Fixed(s.width_bytes()), *s),
                _ => return None,
            };
            let (k, charge) = constant_operand(other, env)?;
            let Value::Array(arr) = k else {
                return None;
            };
            let mut bodies = Vec::with_capacity(arr.elements.len());
            let mut k_has_null = false;
            for v in &arr.elements {
                let fits = match (elem, v) {
                    (_, Value::Null) => {
                        k_has_null = true;
                        continue;
                    }
                    (ElemCodec::Text, Value::Text(t)) => t.len() <= u16::MAX as usize,
                    (ElemCodec::Bytea, Value::Bytea(b)) => b.len() <= u16::MAX as usize,
                    (ElemCodec::Fixed(_), Value::Int(n)) => scalar.in_range(*n),
                    _ => false,
                };
                if !fits {
                    return None;
                }
                bodies.push(crate::format::encode_value(elem_ty, v)[1..].to_vec());
            }
            Some(SpanAtom {
                expr: e,
                col,
                kind: AtomKind::ArrayOp {
                    op,
                    elem,
                    k: bodies,
                    k_has_null,
                    base: COSTS.operator_eval + charge,
                    spans: std::cell::RefCell::new(Vec::new()),
                },
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_array(elems: &[Option<&str>]) -> Vec<u8> {
        let vals = elems
            .iter()
            .map(|e| e.map_or(Value::Null, |s| Value::Text(s.into())))
            .collect();
        let ty = ColType::Array(Box::new(ColType::Scalar(ScalarType::Text)));
        crate::format::encode_value(&ty, &Value::Array(ArrayVal::one_dim(vals)))[1..].to_vec()
    }

    #[test]
    fn len16_payload_requires_an_exact_body() {
        assert_eq!(len16_payload(&[0, 0]), Some(&[][..]));
        assert_eq!(len16_payload(&[0, 2, 0, b'a']), Some(&[0, b'a'][..]));
        assert_eq!(len16_payload(&[0, 2, 0]), None); // short
        assert_eq!(len16_payload(&[0, 1, b'a', b'b']), None); // trailing byte
        assert_eq!(len16_payload(&[0]), None);
    }

    #[test]
    fn array_element_spans_walks_well_formed_bodies() {
        let mut spans = Vec::new();
        let body = text_array(&[Some("a"), None, Some("é"), Some("")]);
        assert_eq!(
            array_element_spans(&body, ElemCodec::Text, &mut spans),
            Some(true)
        );
        let elems: Vec<&[u8]> = spans.iter().map(|s| &body[s.0..s.1]).collect();
        assert_eq!(
            elems,
            vec![&[0, 1, b'a'][..], &[0, 2, 0xC3, 0xA9][..], &[0, 0][..]]
        );
        let empty = text_array(&[]);
        assert_eq!(
            array_element_spans(&empty, ElemCodec::Text, &mut spans),
            Some(false)
        );
        assert!(spans.is_empty());
    }

    #[test]
    fn array_element_spans_rejects_malformed_bodies() {
        let mut spans = Vec::new();
        let body = text_array(&[Some("ab"), Some("c")]);
        // Truncated, trailing bytes, a reserved flag bit, ndim over the maximum.
        assert_eq!(
            array_element_spans(&body[..body.len() - 1], ElemCodec::Text, &mut spans),
            None
        );
        let mut trailing = body.clone();
        trailing.push(0);
        assert_eq!(
            array_element_spans(&trailing, ElemCodec::Text, &mut spans),
            None
        );
        let mut flags = body.clone();
        flags[1] = 0x02;
        assert_eq!(
            array_element_spans(&flags, ElemCodec::Text, &mut spans),
            None
        );
        let mut ndim = body.clone();
        ndim[0] = 7;
        assert_eq!(
            array_element_spans(&ndim, ElemCodec::Text, &mut spans),
            None
        );
        // An element that is not valid UTF-8 leaves the decoder to report it.
        let bad = text_array(&[Some("ab")]);
        let mut bad = bad.clone();
        let last = bad.len() - 1;
        bad[last] = 0xFF;
        assert_eq!(array_element_spans(&bad, ElemCodec::Text, &mut spans), None);
        // The same bytes are a well-formed bytea element.
        assert_eq!(
            array_element_spans(&bad, ElemCodec::Bytea, &mut spans),
            Some(false)
        );
    }
}
