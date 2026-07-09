//! Array/range value operations and literal coercion (mirrors part of impl/go eval.go/kernels.go): the
//! array_* and range value kernels (eval_array_func/eval_range_func/eval_range_op), subscript/CASE
//! helpers, and the string-literal-to-Value coercion path (coerce_string_literal and friends).

use super::*;

impl Bound {
    /// The bound as `Option<i64>` (omitted → `None`, to be defaulted by the slice); `Null` must be
    /// handled by the caller before this is called.
    pub(crate) fn value(self) -> Option<i64> {
        match self {
            Bound::Val(i) => Some(i),
            _ => None,
        }
    }
}

/// Count the NULL (when `want_nulls`) or non-NULL values in `vals` — the shared kernel of
/// num_nulls / num_nonnulls (spec/design/array-functions.md §12), over either the spread arguments
/// or a VARIADIC array's flattened elements.
pub(crate) fn count_nulls<'a>(vals: impl Iterator<Item = &'a Value>, want_nulls: bool) -> usize {
    vals.filter(|v| matches!(v, Value::Null) == want_nulls)
        .count()
}

/// Evaluate an array function over its already-evaluated argument values
/// (spec/design/array-functions.md §3). The introspectors `propagate` NULL and return NULL for an
/// out-of-shape request; the builders are non-strict (a NULL array argument is the identity/empty,
/// NOT a propagated NULL). The resolver guarantees the array operand is an array or NULL, so the
/// `_` arms are genuinely unreachable.
pub(crate) fn eval_array_func(func: &ArrayFunc, vals: &[Value]) -> Result<Value> {
    match func {
        ArrayFunc::ArrayNdims => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Array(a) if a.ndim() == 0 => Ok(Value::Null), // empty array → NULL (PG)
            Value::Array(a) => Ok(Value::Int(a.ndim() as i64)),
            _ => unreachable!("array_ndims: array operand"),
        },
        ArrayFunc::Cardinality => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Array(a) => Ok(Value::Int(a.elements.len() as i64)), // 0 for empty (NOT NULL)
            _ => unreachable!("cardinality: array operand"),
        },
        ArrayFunc::ArrayDims => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Array(a) if a.ndim() == 0 => Ok(Value::Null),
            Value::Array(a) => Ok(Value::Text(array_dims_text(a))),
            _ => unreachable!("array_dims: array operand"),
        },
        // array_to_json(anyarray) → the array's compact JSON image (the to_jsonb node kernel). STRICT;
        // a multidimensional array propagates the to_jsonb 0A000.
        ArrayFunc::ArrayToJson => match &vals[0] {
            Value::Null => Ok(Value::Null),
            _ => Ok(Value::Json(json::json_compact_out(&value_to_node(
                &vals[0],
            )?))),
        },
        // array_length / array_lower / array_upper (anyarray, dim): propagate either NULL arg,
        // and return NULL for an empty array or an out-of-range dimension.
        ArrayFunc::ArrayLength | ArrayFunc::ArrayLower | ArrayFunc::ArrayUpper => {
            let a = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Array(a) => a,
                _ => unreachable!("array_length/lower/upper: array operand"),
            };
            let dim = match &vals[1] {
                Value::Null => return Ok(Value::Null),
                Value::Int(d) => *d,
                _ => unreachable!("the dimension argument is the integer family"),
            };
            if a.ndim() == 0 || dim < 1 || dim > a.ndim() as i64 {
                return Ok(Value::Null);
            }
            let d = (dim - 1) as usize;
            let v = match func {
                ArrayFunc::ArrayLength => a.dims[d] as i64,
                ArrayFunc::ArrayLower => a.lbounds[d] as i64,
                ArrayFunc::ArrayUpper => a.ubound(d) as i64,
                _ => unreachable!(),
            };
            Ok(Value::Int(v))
        }
        ArrayFunc::ArrayAppend => array_extend(&vals[0], &vals[1], true),
        ArrayFunc::ArrayPrepend => array_extend(&vals[1], &vals[0], false),
        ArrayFunc::ArrayCat => array_cat_values(&vals[0], &vals[1]),
        ArrayFunc::ArrayRemove => array_remove_value(&vals[0], &vals[1]),
        ArrayFunc::ArrayReplace => array_replace_value(&vals[0], &vals[1], &vals[2]),
        ArrayFunc::ArrayPosition => array_position_value(&vals[0], &vals[1], vals.get(2)),
        ArrayFunc::ArrayPositions => array_positions_value(&vals[0], &vals[1]),
        ArrayFunc::Contains => array_contains_value(&vals[0], &vals[1]),
        ArrayFunc::ContainedBy => array_contains_value(&vals[1], &vals[0]),
        ArrayFunc::Overlaps => array_overlaps_value(&vals[0], &vals[1]),
    }
}

/// Evaluate a range accessor (spec/design/range-functions.md §1). STRICT: a NULL range → NULL.
/// `lower`/`upper` yield the bound value (NULL when empty or unbounded on that side); the `_inc`/
/// `_inf` readers + `isempty` yield boolean. For the empty range every reader but `isempty` is
/// false/NULL; for an infinite bound the `_inf` reader is true and the `_inc` reader false.
pub(crate) fn eval_range_func(func: &RangeFunc, vals: &[Value]) -> Result<Value> {
    let rv = match &vals[0] {
        Value::Null => return Ok(Value::Null),
        Value::Range(rv) => rv,
        _ => unreachable!("range accessor: range operand"),
    };
    Ok(match func {
        RangeFunc::Lower => match (rv.empty, &rv.lower) {
            (false, Some(v)) => (**v).clone(),
            _ => Value::Null,
        },
        RangeFunc::Upper => match (rv.empty, &rv.upper) {
            (false, Some(v)) => (**v).clone(),
            _ => Value::Null,
        },
        RangeFunc::IsEmpty => Value::Bool(rv.empty),
        // For the empty range both inclusivity flags are false by the canonical invariant, so reading
        // them directly already yields PG's `false`; an infinite bound likewise stores `_inc = false`.
        RangeFunc::LowerInc => Value::Bool(rv.lower_inc),
        RangeFunc::UpperInc => Value::Bool(rv.upper_inc),
        // The empty range is NOT infinite on either side (PG): guard before reading the bound.
        RangeFunc::LowerInf => Value::Bool(!rv.empty && rv.lower.is_none()),
        RangeFunc::UpperInf => Value::Bool(!rv.empty && rv.upper.is_none()),
    })
}

/// Build a range value from a constructor call's evaluated arguments (range-functions.md §2). `vals`
/// is `[lo, hi]` or `[lo, hi, bounds]`. Each bound is coerced to the element `elem` assignment-style
/// (a NULL bound → an infinite bound; an integer range-checks 22003; an int→decimal / text→temporal
/// adapts), the bounds flags are read (default `[)`; a NULL 3-arg flags → 22000; an invalid flags
/// string → 42601), and `finalize` produces the canonical value (order-check 22000, canonicalize,
/// empty-normalize).
pub(crate) fn eval_range_ctor(elem: ScalarType, vals: &[Value]) -> Result<Value> {
    let desc =
        crate::range::range_for_element(elem).expect("a range constructor's elem has a range");
    let lower = coerce_range_bound(vals[0].clone(), elem)?;
    let upper = coerce_range_bound(vals[1].clone(), elem)?;
    let (lower_inc, upper_inc) = match vals.get(2) {
        None => (true, false), // 2-arg form defaults to `[)`
        Some(Value::Null) => {
            return Err(EngineError::new(
                SqlState::DataException,
                "range constructor flags argument must not be null".to_string(),
            ));
        }
        Some(Value::Text(s)) => crate::range::parse_bound_flags(s)?,
        Some(_) => unreachable!("resolver restricts the range bounds flags to text"),
    };
    Ok(Value::Range(crate::range::finalize(
        desc, lower, upper, lower_inc, upper_inc,
    )?))
}

/// Coerce one constructor bound value to the range element `elem`, returning `None` for a NULL bound
/// (an infinite bound). Reuses [`store_value`] (the INSERT/UPDATE assignment coercion): an integer
/// range-checks into the element (22003), an int→decimal widens, a text→temporal parses, and a
/// non-assignable value is 42804 (the resolver already screened the common 42883 cases).
pub(crate) fn coerce_range_bound(v: Value, elem: ScalarType) -> Result<Option<Value>> {
    match store_value(v, elem, None, None, false, "range bound")? {
        Value::Null => Ok(None),
        other => Ok(Some(other)),
    }
}

/// Evaluate a range boolean operator (range-functions.md §3, RF3) over two already-evaluated operand
/// values. STRICT: a NULL operand → NULL. For the range-against-range operators both operands are
/// ranges; for the element overloads (`ContainsElem`/`ElemContainedBy`) the non-range operand is
/// coerced to the range's element type `elem` (assignment-style, matching the resolver's hint). The
/// boolean kernels live in `range.rs`.
pub(crate) fn eval_range_op(op: RangeOp, l: &Value, r: &Value, elem: ScalarType) -> Result<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let result = match op {
        // `range @> element`: l is the range, r the element (coerced to the range's element type).
        RangeOp::ContainsElem => {
            let e = store_value(r.clone(), elem, None, None, false, "range element")?;
            crate::range::range_contains_elem(expect_range(l), &e)
        }
        // `element <@ range`: l is the element, r the range.
        RangeOp::ElemContainedBy => {
            let e = store_value(l.clone(), elem, None, None, false, "range element")?;
            crate::range::range_contains_elem(expect_range(r), &e)
        }
        _ => {
            let (a, b) = (expect_range(l), expect_range(r));
            match op {
                RangeOp::Contains => crate::range::range_contains(a, b),
                RangeOp::ContainedBy => crate::range::range_contains(b, a),
                RangeOp::Overlaps => crate::range::range_overlaps(a, b),
                RangeOp::Before => crate::range::range_before(a, b),
                RangeOp::After => crate::range::range_after(a, b),
                RangeOp::Overleft => crate::range::range_overleft(a, b),
                RangeOp::Overright => crate::range::range_overright(a, b),
                RangeOp::Adjacent => crate::range::range_adjacent(a, b),
                RangeOp::ContainsElem | RangeOp::ElemContainedBy => {
                    unreachable!("element overloads handled above")
                }
            }
        }
    };
    Ok(Value::Bool(result))
}

/// Evaluate a range SET operator (range-functions.md §4, RF4) over two already-evaluated operands.
/// STRICT: a NULL operand → NULL. Dispatches to the `range.rs` kernels; `+` (`Union`) and `-`
/// (`Difference`) raise 22000 on a non-contiguous result, `*` (`Intersect`) and `range_merge`
/// (`Merge`) never error.
pub(crate) fn eval_range_set_op(op: RangeSetOp, l: &Value, r: &Value) -> Result<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let (a, b) = (expect_range(l), expect_range(r));
    let rv = match op {
        RangeSetOp::Union => crate::range::range_union(a, b, true)?,
        RangeSetOp::Merge => crate::range::range_union(a, b, false)?,
        RangeSetOp::Intersect => crate::range::range_intersect(a, b),
        RangeSetOp::Difference => crate::range::range_minus(a, b)?,
    };
    Ok(Value::Range(rv))
}

/// Extract the [`RangeVal`] from a value the resolver guaranteed is a (non-NULL) range operand.
pub(crate) fn expect_range(v: &Value) -> &RangeVal {
    match v {
        Value::Range(rv) => rv,
        _ => unreachable!("the range-operator resolver guarantees a range operand here"),
    }
}

/// STRICT element equality for the containment/overlap operators (array-functions.md §10): a NULL
/// element equals NOTHING — including another NULL — the deliberate inverse of `not_distinct` (§5
/// #10). For two non-NULL values it is jed's total element comparator (`value_cmp == Equal`), which
/// for jed's element types agrees with PostgreSQL's per-type btree equality.
pub(crate) fn strict_elem_eq(a: &Value, b: &Value) -> bool {
    !matches!(a, Value::Null)
        && !matches!(b, Value::Null)
        && value_cmp(a, b) == std::cmp::Ordering::Equal
}

/// `a @> b` (array-functions.md §10): does `a` CONTAIN `b` — is every element of `b` present in `a`
/// under STRICT equality, over the flattened element multiset (any dimensionality)? A NULL
/// whole-array operand → NULL. The empty array is contained by anything (`a @> {}` is true).
pub(crate) fn array_contains_value(a: &Value, b: &Value) -> Result<Value> {
    let (ca, cb) = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Array(ca), Value::Array(cb)) => (ca, cb),
        _ => unreachable!("array containment: array operands"),
    };
    let contained = cb
        .elements
        .iter()
        .all(|eb| ca.elements.iter().any(|ea| strict_elem_eq(ea, eb)));
    Ok(Value::Bool(contained))
}

/// `a && b` (array-functions.md §10): do `a` and `b` OVERLAP — share at least one element under
/// STRICT equality, over the flattened element multiset (any dimensionality)? A NULL whole-array
/// operand → NULL. The empty array overlaps nothing.
pub(crate) fn array_overlaps_value(a: &Value, b: &Value) -> Result<Value> {
    let (ca, cb) = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Array(ca), Value::Array(cb)) => (ca, cb),
        _ => unreachable!("array overlap: array operands"),
    };
    let overlaps = ca
        .elements
        .iter()
        .any(|ea| cb.elements.iter().any(|eb| strict_elem_eq(ea, eb)));
    Ok(Value::Bool(overlaps))
}

/// IS NOT DISTINCT FROM at the value level (array-functions.md §5 #10): jed's total element
/// comparator (the array-element / btree equality), so `NULL` equals `NULL` and a non-NULL never
/// equals `NULL`. For jed's element types this agrees with PostgreSQL's per-type btree equality.
pub(crate) fn not_distinct(a: &Value, b: &Value) -> bool {
    value_cmp(a, b) == std::cmp::Ordering::Equal
}

/// array_remove(a, e) (array-functions.md §8): drop every element NOT DISTINCT FROM `e`. NULL array
/// → NULL; **1-D/empty only** (a multidimensional array is 0A000); the lower bound is preserved and
/// an all-removed result is the empty array `{}`.
pub(crate) fn array_remove_value(arr: &Value, elem: &Value) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_remove: array operand"),
    };
    if a.ndim() > 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "removing elements from multidimensional arrays is not supported",
        ));
    }
    let kept: Vec<Value> = a
        .elements
        .iter()
        .filter(|e| !not_distinct(e, elem))
        .cloned()
        .collect();
    if kept.is_empty() {
        return Ok(Value::Array(ArrayVal::empty()));
    }
    let lb = a.lbounds.first().copied().unwrap_or(1);
    Ok(Value::Array(ArrayVal {
        dims: vec![kept.len()],
        lbounds: vec![lb],
        elements: kept,
    }))
}

/// array_replace(a, from, to) (array-functions.md §8): substitute every element NOT DISTINCT FROM
/// `from` with `to`. Works on **any** dimensionality — the shape (dims/lbounds) is preserved and
/// only matching element values change. NULL array → NULL.
pub(crate) fn array_replace_value(arr: &Value, from: &Value, to: &Value) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_replace: array operand"),
    };
    let elements = a
        .elements
        .iter()
        .map(|e| {
            if not_distinct(e, from) {
                to.clone()
            } else {
                e.clone()
            }
        })
        .collect();
    Ok(Value::Array(ArrayVal {
        dims: a.dims.clone(),
        lbounds: a.lbounds.clone(),
        elements,
    }))
}

/// array_position(a, e[, start]) (array-functions.md §8): the SUBSCRIPT (in the array's lower-bound
/// space) of the first element NOT DISTINCT FROM `e`, NULL if absent. **1-D/empty only** (a
/// multidimensional array is 0A000); the optional `start` is a subscript to begin the scan at, and a
/// NULL `start` is 22004.
pub(crate) fn array_position_value(
    arr: &Value,
    elem: &Value,
    start: Option<&Value>,
) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_position: array operand"),
    };
    if a.ndim() > 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "searching for elements in multidimensional arrays is not supported",
        ));
    }
    let lb = a.lbounds.first().copied().unwrap_or(1);
    // The scan's 0-based start offset into `elements`: by default the array's lower bound; an
    // explicit `start` is a SUBSCRIPT, so the offset is `start - lb` (clamped to >= 0).
    let begin = match start {
        None => 0usize,
        Some(Value::Null) => {
            return Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "initial position must not be null",
            ));
        }
        Some(Value::Int(s)) => (s - lb as i64).max(0) as usize,
        _ => unreachable!("array_position: start is the integer family"),
    };
    for (i, e) in a.elements.iter().enumerate().skip(begin) {
        if not_distinct(e, elem) {
            return Ok(Value::Int(lb as i64 + i as i64));
        }
    }
    Ok(Value::Null)
}

/// array_positions(a, e) (array-functions.md §8): the i32[] of every match's subscript (in the
/// array's lower-bound space), the empty array `{}` if none. NULL array → NULL; **1-D/empty only**
/// (a multidimensional array is 0A000).
pub(crate) fn array_positions_value(arr: &Value, elem: &Value) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_positions: array operand"),
    };
    if a.ndim() > 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "searching for elements in multidimensional arrays is not supported",
        ));
    }
    let lb = a.lbounds.first().copied().unwrap_or(1);
    let positions: Vec<Value> = a
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| not_distinct(e, elem))
        .map(|(i, _)| Value::Int(lb as i64 + i as i64))
        .collect();
    Ok(Value::Array(ArrayVal::one_dim(positions)))
}

/// The `array_dims` text form `[l1:u1][l2:u2]…` (no trailing `=`, unlike `array_out`'s prefix —
/// array-functions.md §3.1).
pub(crate) fn array_dims_text(a: &ArrayVal) -> String {
    let mut s = String::new();
    for d in 0..a.ndim() {
        s.push('[');
        s.push_str(&a.lbounds[d].to_string());
        s.push(':');
        s.push_str(&a.ubound(d).to_string());
        s.push(']');
    }
    s
}

/// array_append (`append=true`) / array_prepend (array-functions.md §3.2). The array side is
/// non-strict: a NULL or empty array yields the 1-D singleton `{elem}` (lower bound 1). A 1-D array
/// grows by one element, preserving its lower bound; a multidimensional array is `22000`.
pub(crate) fn array_extend(arr: &Value, elem: &Value, append: bool) -> Result<Value> {
    let av = match arr {
        Value::Null => None,
        Value::Array(a) => Some(a),
        _ => unreachable!("array_append/prepend: array operand"),
    };
    match av {
        None => Ok(Value::Array(ArrayVal::one_dim(vec![elem.clone()]))),
        Some(a) if a.ndim() == 0 => Ok(Value::Array(ArrayVal::one_dim(vec![elem.clone()]))),
        Some(a) if a.ndim() == 1 => {
            let mut elements = a.elements.clone();
            if append {
                elements.push(elem.clone());
            } else {
                elements.insert(0, elem.clone());
            }
            Ok(Value::Array(ArrayVal {
                dims: vec![a.dims[0] + 1],
                lbounds: a.lbounds.clone(),
                elements,
            }))
        }
        Some(_) => Err(EngineError::new(
            SqlState::DataException,
            "argument must be empty or one-dimensional array",
        )),
    }
}

/// array_cat (array-functions.md §3.2): identity-aware concatenation along the outer dimension.
/// NULL/empty is the identity (both NULL → NULL). Same dimensionality concatenates if the inner
/// dims match; an off-by-one dimensionality appends/prepends the lower one as an outer slice; any
/// other pairing — or an inner-dim mismatch — is `2202E`. The flattened element list is always
/// `a ++ b` (row-major, outer-first); the result lower bounds come from the higher-dim operand.
pub(crate) fn array_cat_values(a: &Value, b: &Value) -> Result<Value> {
    match (a, b) {
        (Value::Null, Value::Null) => return Ok(Value::Null),
        (Value::Null, _) => return Ok(b.clone()),
        (_, Value::Null) => return Ok(a.clone()),
        _ => {}
    }
    let av = match a {
        Value::Array(x) => x,
        _ => unreachable!("array_cat: array operand"),
    };
    let bv = match b {
        Value::Array(x) => x,
        _ => unreachable!("array_cat: array operand"),
    };
    if av.ndim() == 0 {
        return Ok(b.clone());
    }
    if bv.ndim() == 0 {
        return Ok(a.clone());
    }
    let mismatch = || {
        EngineError::new(
            SqlState::ArraySubscriptError,
            "cannot concatenate incompatible arrays",
        )
    };
    let mut elements = av.elements.clone();
    elements.extend(bv.elements.iter().cloned());
    let (na, nb) = (av.ndim(), bv.ndim());
    if na == nb {
        if av.dims[1..] != bv.dims[1..] {
            return Err(mismatch());
        }
        let mut dims = av.dims.clone();
        dims[0] = av.dims[0] + bv.dims[0];
        Ok(Value::Array(ArrayVal {
            dims,
            lbounds: av.lbounds.clone(),
            elements,
        }))
    } else if na == nb + 1 {
        if av.dims[1..] != bv.dims[..] {
            return Err(mismatch());
        }
        let mut dims = av.dims.clone();
        dims[0] = av.dims[0] + 1;
        Ok(Value::Array(ArrayVal {
            dims,
            lbounds: av.lbounds.clone(),
            elements,
        }))
    } else if nb == na + 1 {
        if bv.dims[1..] != av.dims[..] {
            return Err(mismatch());
        }
        let mut dims = bv.dims.clone();
        dims[0] = bv.dims[0] + 1;
        Ok(Value::Array(ArrayVal {
            dims,
            lbounds: bv.lbounds.clone(),
            elements,
        }))
    } else {
        Err(mismatch())
    }
}

/// Evaluate an array subscript `base[..][..]` (spec/design/array.md §6) — the body of
/// [`RExpr::Subscript`]'s eval arm, kept here so its locals stay out of `eval`'s frame. A NULL
/// array or any NULL subscript bound yields NULL; element access returns the element (or NULL),
/// slice access a (renumbered) sub-array.
pub(crate) fn eval_subscript(
    base: &RExpr,
    subscripts: &[RSubscript],
    is_slice: bool,
    row: &[Value],
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Value> {
    let a = match base.eval(row, env, m)? {
        Value::Array(a) => a,
        Value::Null => return Ok(Value::Null),
        other => unreachable!("subscript on a non-array value: {other:?}"),
    };
    if is_slice {
        // Per-dimension (lower, upper); a scalar index `i` becomes `1:i` (PG), an omitted bound
        // defers to the array's own bound. A NULL bound → NULL.
        let mut bounds = Vec::with_capacity(subscripts.len());
        for s in subscripts {
            let b = match s {
                RSubscript::Index(e) => match e.eval(row, env, m)? {
                    Value::Int(i) => (Some(1i64), Some(i)),
                    Value::Null => return Ok(Value::Null),
                    other => unreachable!("non-int array subscript: {other:?}"),
                },
                RSubscript::Slice { lower, upper } => {
                    let lo = eval_opt_bound(lower, row, env, m)?;
                    let hi = eval_opt_bound(upper, row, env, m)?;
                    match (lo, hi) {
                        (Bound::Null, _) | (_, Bound::Null) => return Ok(Value::Null),
                        (lo, hi) => (lo.value(), hi.value()),
                    }
                }
            };
            bounds.push(b);
        }
        Ok(array_get_slice(&a, &bounds))
    } else {
        // Element access: every spec is an index (a slice would have set `is_slice`).
        let mut idxs = Vec::with_capacity(subscripts.len());
        for s in subscripts {
            let RSubscript::Index(e) = s else {
                unreachable!("non-index subscript in element access")
            };
            match e.eval(row, env, m)? {
                Value::Int(i) => idxs.push(i),
                Value::Null => return Ok(Value::Null),
                other => unreachable!("non-int array subscript: {other:?}"),
            }
        }
        Ok(array_get_element(&a, &idxs))
    }
}

/// Evaluate an optional slice-bound expression (spec/design/array.md §6).
pub(crate) fn eval_opt_bound(
    b: &Option<Box<RExpr>>,
    row: &[Value],
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Bound> {
    match b {
        None => Ok(Bound::Omitted),
        Some(e) => match e.eval(row, env, m)? {
            Value::Int(i) => Ok(Bound::Val(i)),
            Value::Null => Ok(Bound::Null),
            other => unreachable!("non-int array slice bound: {other:?}"),
        },
    }
}

/// Read a single array element by `idxs` (1-based per dimension, using the value's lower bounds) —
/// spec/design/array.md §6. NULL when the subscript count ≠ `ndim` or any index is out of range.
pub(crate) fn array_get_element(a: &ArrayVal, idxs: &[i64]) -> Value {
    if idxs.len() != a.ndim() || a.elements.is_empty() {
        return Value::Null;
    }
    let mut flat = 0usize;
    let mut stride = 1usize;
    for d in (0..a.ndim()).rev() {
        let lb = a.lbounds[d] as i64;
        let ub = a.ubound(d) as i64;
        if idxs[d] < lb || idxs[d] > ub {
            return Value::Null;
        }
        flat += (idxs[d] - lb) as usize * stride;
        stride *= a.dims[d];
    }
    a.elements[flat].clone()
}

/// Read an array slice (spec/design/array.md §6): per-dimension `(lower, upper)` requested bounds
/// (`None` defers to the value's own bound), clamped to each dimension's `[lb, ub]`. Too many
/// subscripts, an empty source, or any empty clamped dimension yields the empty array; fewer
/// subscripts than `ndim` leave the trailing dimensions at their full range. The result is
/// renumbered to lower bound 1 on every dimension (PG `array_get_slice`).
pub(crate) fn array_get_slice(a: &ArrayVal, bounds: &[(Option<i64>, Option<i64>)]) -> Value {
    let ndim = a.ndim();
    if bounds.len() > ndim || ndim == 0 {
        return Value::Array(ArrayVal::empty());
    }
    let mut new_dims = Vec::with_capacity(ndim);
    let mut starts = Vec::with_capacity(ndim); // source 0-based start per dimension
    for d in 0..ndim {
        let lb = a.lbounds[d] as i64;
        let ub = a.ubound(d) as i64;
        let (req_lo, req_hi) = if d < bounds.len() {
            (bounds[d].0.unwrap_or(lb), bounds[d].1.unwrap_or(ub))
        } else {
            (lb, ub) // a trailing unspecified dimension spans its full range
        };
        let lo = req_lo.max(lb);
        let hi = req_hi.min(ub);
        if lo > hi {
            return Value::Array(ArrayVal::empty()); // any empty dimension → empty slice
        }
        new_dims.push((hi - lo + 1) as usize);
        starts.push((lo - lb) as usize);
    }
    // Row-major strides over the SOURCE array.
    let mut strides = vec![1usize; ndim];
    for d in (0..ndim - 1).rev() {
        strides[d] = strides[d + 1] * a.dims[d + 1];
    }
    let total: usize = new_dims.iter().product();
    let mut elements = Vec::with_capacity(total);
    let mut counter = vec![0usize; ndim];
    for _ in 0..total {
        let mut flat = 0usize;
        for d in 0..ndim {
            flat += (starts[d] + counter[d]) * strides[d];
        }
        elements.push(a.elements[flat].clone());
        for d in (0..ndim).rev() {
            counter[d] += 1;
            if counter[d] < new_dims[d] {
                break;
            }
            counter[d] = 0;
        }
    }
    Value::Array(ArrayVal {
        dims: new_dims,
        lbounds: vec![1i32; ndim],
        elements,
    })
}

pub(crate) fn unify_case_types(arms: &[ResolvedType]) -> Result<ResolvedType> {
    let non_null: Vec<&ResolvedType> = arms.iter().filter(|t| **t != ResolvedType::Null).collect();
    let Some(&first) = non_null.first() else {
        // Every arm is NULL/untyped — PostgreSQL types the CASE as text.
        return Ok(ResolvedType::Text);
    };
    let all_numeric = non_null
        .iter()
        .all(|t| matches!(t, ResolvedType::Int(_) | ResolvedType::Decimal));
    if all_numeric {
        if non_null.iter().any(|t| **t == ResolvedType::Decimal) {
            return Ok(ResolvedType::Decimal);
        }
        // All integer: the widest via the promotion tower (width is unobservable in output —
        // every integer renders under the `I` tag — but the fold keeps the type precise).
        let mut acc = first.clone();
        for t in &non_null[1..] {
            acc = ResolvedType::Int(promote(&acc, t));
        }
        return Ok(acc);
    }
    // Non-numeric: every arm must be the same family as the first (cross-family is 42804).
    for t in &non_null[1..] {
        if std::mem::discriminant(*t) != std::mem::discriminant(first) {
            return Err(type_error("CASE result types must be compatible"));
        }
    }
    Ok(first.clone())
}

/// Coerce a CASE arm's value to the unified result type. The only runtime coercion needed is
/// widening an integer result to decimal when the unified type is decimal — integer-width
/// unification needs none (all integers are `i64`), and an all-NULL CASE is text but every arm
/// evaluates to NULL anyway.
pub(crate) fn coerce_case(v: Value, to_decimal: bool) -> Value {
    match (to_decimal, v) {
        (true, Value::Int(n)) => Value::Decimal(Decimal::from_i64(n)),
        (_, v) => v,
    }
}

/// The operator's name for an error message (PostgreSQL phrasing).
pub(crate) fn setop_name(op: SetOpKind) -> &'static str {
    match op {
        SetOpKind::Union => "UNION",
        SetOpKind::Intersect => "INTERSECT",
        SetOpKind::Except => "EXCEPT",
    }
}

/// Unify one output column's type across the two operands of a set operation
/// (spec/design/grammar.md §25, types.md §4): integer widths promote to the widest; integer with
/// decimal -> decimal; a NULL-typed operand takes the other's type (an all-NULL column stays NULL
/// — PostgreSQL would call a top-level one `text`, but the type is never observed in output); a
/// same-family non-numeric pair gives that type; anything else is 42804. The set of unifiable
/// pairs mirrors the comparability matrix (compare.toml).
/// Unify two row value types for the SAME VALUES-body column (spec/design/grammar.md §42), the
/// set-operation rule (§25): integer widths widen, `int`+`decimal` → `decimal`, anything + `NULL`
/// keeps the other, and a same-type scalar pair (`text`, `bool`, `bytea`, `uuid`, a `timestamp` /
/// `timestamptz`, an `interval`, a same-width `float`) unifies to itself; any other pair — including
/// a composite or array column across rows (a deferred edge) — is 42804. Enumerated EXPLICITLY (not
/// a generic `a == b`) so all three cores compute byte-identical results (CLAUDE.md §8).
pub(crate) fn unify_values_column(a: &ResolvedType, b: &ResolvedType) -> Result<ResolvedType> {
    use ResolvedType::*;
    Ok(match (a, b) {
        (Null, Null) => Null,
        (Null, x) | (x, Null) => x.clone(),
        (Int(_), Int(_)) => Int(promote(a, b)),
        (Decimal, Decimal) | (Int(_), Decimal) | (Decimal, Int(_)) => Decimal,
        (Text, Text) => Text,
        (Bool, Bool) => Bool,
        (Bytea, Bytea) => Bytea,
        (Uuid, Uuid) => Uuid,
        (Timestamp, Timestamp) => Timestamp,
        (Timestamptz, Timestamptz) => Timestamptz,
        (Date, Date) => Date,
        (Interval, Interval) => Interval,
        (Float(x), Float(y)) if x == y => Float(*x),
        _ => {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "VALUES types {} and {} cannot be matched",
                    a.type_name(),
                    b.type_name()
                ),
            ));
        }
    })
}

/// The scalar type to note a bind parameter at, given its VALUES column's unified type
/// (spec/design/grammar.md §42). A scalar type flows through; a NULL / composite / array column
/// has no scalar parameter type, so the parameter stays untyped (42P18 at `finalize`).
pub(crate) fn scalar_for_param_hint(rt: &ResolvedType) -> Option<ScalarType> {
    match rt {
        ResolvedType::Int(s) | ResolvedType::Float(s) => Some(*s),
        ResolvedType::Bool => Some(ScalarType::Bool),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Uuid => Some(ScalarType::Uuid),
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        ResolvedType::Date => Some(ScalarType::Date),
        ResolvedType::Interval => Some(ScalarType::Interval),
        ResolvedType::Json => Some(ScalarType::Json),
        ResolvedType::JsonPath => Some(ScalarType::JsonPath),
        ResolvedType::Jsonb => Some(ScalarType::Jsonb),
        ResolvedType::Null
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => None,
    }
}

pub(crate) fn unify_setop_column(
    a: &ResolvedType,
    b: &ResolvedType,
    op: SetOpKind,
) -> Result<ResolvedType> {
    use ResolvedType::*;
    let out = match (a, b) {
        (Null, Null) => Null,
        (Null, x) | (x, Null) => x.clone(),
        (Int(_), Int(_)) => Int(promote(a, b)),
        (Decimal, Decimal) | (Int(_), Decimal) | (Decimal, Int(_)) => Decimal,
        (Text, Text) => Text,
        (Bool, Bool) => Bool,
        (Bytea, Bytea) => Bytea,
        (Uuid, Uuid) => Uuid,
        (Timestamp, Timestamp) => Timestamp,
        (Timestamptz, Timestamptz) => Timestamptz,
        (Date, Date) => Date,
        _ => {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "{} types {} and {} cannot be matched",
                    setop_name(op),
                    a.type_name(),
                    b.type_name()
                ),
            ));
        }
    };
    Ok(out)
}

/// Convert each row's values in place to the unified set-operation column types — the only runtime
/// change is integer -> decimal (a NULL stays NULL; integer-width promotion is a value no-op since
/// every integer is i64). Same conversion `coerce_case` uses for CASE.
pub(crate) fn coerce_setop_rows(
    rows: &mut [Vec<Value>],
    from: &[ResolvedType],
    to: &[ResolvedType],
) {
    for (i, (f, t)) in from.iter().zip(to.iter()).enumerate() {
        if matches!(f, ResolvedType::Int(_)) && *t == ResolvedType::Decimal {
            for row in rows.iter_mut() {
                if let Value::Int(n) = &row[i] {
                    let n = *n;
                    row[i] = Value::Decimal(Decimal::from_i64(n));
                }
            }
        }
    }
}

/// Combine the operands' rows per the set operator + ALL flag (spec/design/grammar.md §25). Rows
/// match by NULL-safe, value-canonical equality (the `Value` Eq/Hash — two NULLs match, 1.5 ==
/// 1.50, and a converted int matches the decimal). The emitted representative for a matched /
/// deduplicated key is its FIRST occurrence scanning the LEFT operand then the right, and emitted
/// rows keep that left-then-right scan order — deterministic and identical across cores. (A later
/// ORDER BY re-sorts; without one, output order is unspecified and the corpus compares rowsort.)
pub(crate) fn combine_setop(
    op: SetOpKind,
    all: bool,
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
) -> Vec<Vec<Value>> {
    match (op, all) {
        // UNION ALL: every left row then every right row, no dedup.
        (SetOpKind::Union, true) => {
            let mut rows = left;
            rows.extend(right);
            rows
        }
        // UNION: one copy per key present in either, first occurrence (left scanned first).
        (SetOpKind::Union, false) => {
            let mut seen: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left.into_iter().chain(right) {
                if seen.insert(row.clone()) {
                    out.push(row);
                }
            }
            out
        }
        // INTERSECT ALL: min(m, n) copies — emit a left row while the right still has budget.
        (SetOpKind::Intersect, true) => {
            let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
            for row in right {
                *counts.entry(row).or_insert(0) += 1;
            }
            let mut out = Vec::new();
            for row in left {
                if let Some(c) = counts.get_mut(&row) {
                    if *c > 0 {
                        *c -= 1;
                        out.push(row);
                    }
                }
            }
            out
        }
        // INTERSECT: one copy per distinct left key also present in the right.
        (SetOpKind::Intersect, false) => {
            let right_set: HashSet<Vec<Value>> = right.into_iter().collect();
            let mut emitted: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left {
                if right_set.contains(&row) && emitted.insert(row.clone()) {
                    out.push(row);
                }
            }
            out
        }
        // EXCEPT ALL: max(0, m - n) copies — the right cancels the first n left occurrences.
        (SetOpKind::Except, true) => {
            let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
            for row in right {
                *counts.entry(row).or_insert(0) += 1;
            }
            let mut out = Vec::new();
            for row in left {
                match counts.get_mut(&row) {
                    Some(c) if *c > 0 => *c -= 1,
                    _ => out.push(row),
                }
            }
            out
        }
        // EXCEPT: one copy per distinct left key absent from the right.
        (SetOpKind::Except, false) => {
            let right_set: HashSet<Vec<Value>> = right.into_iter().collect();
            let mut emitted: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left {
                if !right_set.contains(&row) && emitted.insert(row.clone()) {
                    out.push(row);
                }
            }
            out
        }
    }
}

/// Resolve a trailing ORDER BY key for a set operation against the OUTPUT column names (the left
/// operand's). A qualified key is 42P01 (no relation scope after a set operation); an unknown name
/// is 42703. Returns the output column index.
pub(crate) fn resolve_setop_order_key(key: &OrderKey, names: &[String]) -> Result<usize> {
    // A set-operation ORDER BY accepts only an output column name or ordinal — a general expression key
    // (after the inputs are unified) is 0A000, matching PostgreSQL's "invalid UNION/INTERSECT/EXCEPT
    // ORDER BY clause" (grammar.md §10).
    if key.expr.is_some() {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "invalid UNION/INTERSECT/EXCEPT ORDER BY clause",
        ));
    }
    // An output-column ordinal (`... ORDER BY 1`) resolves by position into the output columns; out
    // of [1, ncols] is 42P10 (grammar.md §10). It precedes the name path (an ordinal has no column).
    if let Some(ord) = key.ordinal {
        if ord < 1 || ord > names.len() as i64 {
            return Err(EngineError::new(
                SqlState::InvalidColumnReference,
                format!("ORDER BY position {ord} is not in select list"),
            ));
        }
        return Ok((ord - 1) as usize);
    }
    if let Some(q) = &key.qualifier {
        return Err(EngineError::new(
            SqlState::UndefinedTable,
            format!("missing FROM-clause entry for table {q}"),
        ));
    }
    names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(&key.column))
        .ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedColumn,
                format!("column {} does not exist", key.column),
            )
        })
}

pub(crate) fn require_bool(ty: &ResolvedType, msg: &str) -> Result<()> {
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(()),
        ResolvedType::Int(_)
        | ResolvedType::Text
        | ResolvedType::Decimal
        | ResolvedType::Bytea
        | ResolvedType::Uuid
        | ResolvedType::Timestamp
        | ResolvedType::Timestamptz
        | ResolvedType::Date
        | ResolvedType::Interval
        | ResolvedType::Float(_)
        | ResolvedType::Json
        | ResolvedType::Jsonb
        | ResolvedType::JsonPath
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => Err(type_error(msg)),
    }
}

/// A value assigned to a column must match its family: an integer column takes an
/// integer (or NULL) value; a text column takes a text (or NULL) value; a boolean column
/// takes a boolean (or NULL) value. Any cross-family pair is a 42804 type error. Mirrors
/// the INSERT literal type-check, generalized to expressions.
pub(crate) fn require_assignable(ty: &ResolvedType, col_ty: ScalarType, col: &str) -> Result<()> {
    let ok = if col_ty.is_integer() {
        matches!(ty, ResolvedType::Int(_) | ResolvedType::Null)
    } else if col_ty.is_decimal() {
        // int → decimal is implicit (lossless); decimal → decimal re-scales. A decimal value
        // into an integer column is NOT assignable (decimal→int is explicit-CAST only).
        matches!(
            ty,
            ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
        )
    } else if col_ty.is_bool() {
        matches!(ty, ResolvedType::Bool | ResolvedType::Null)
    } else if col_ty.is_bytea() {
        matches!(ty, ResolvedType::Bytea | ResolvedType::Null)
    } else if col_ty.is_uuid() {
        matches!(ty, ResolvedType::Uuid | ResolvedType::Null)
    } else if col_ty.is_timestamp() {
        matches!(ty, ResolvedType::Timestamp | ResolvedType::Null)
    } else if col_ty.is_timestamptz() {
        matches!(ty, ResolvedType::Timestamptz | ResolvedType::Null)
    } else if col_ty.is_interval() {
        matches!(ty, ResolvedType::Interval | ResolvedType::Null)
    } else if col_ty.is_date() {
        matches!(ty, ResolvedType::Date | ResolvedType::Null)
    } else if col_ty.is_float() {
        // A float value assigns to an equal-or-wider float column: f32 → f32/f64
        // (implicit widening), f64 → f64 only (f64 → f32 is explicit-CAST only).
        matches!(ty, ResolvedType::Float(st) if st.rank() <= col_ty.rank())
            || matches!(ty, ResolvedType::Null)
    } else {
        // text column
        matches!(ty, ResolvedType::Text | ResolvedType::Null)
    };
    if ok {
        Ok(())
    } else {
        Err(type_error(format!(
            "cannot assign a value to column {col} of type {}",
            col_ty.canonical_name()
        )))
    }
}

pub(crate) fn col_idx(table: &Table, name: &str) -> Result<usize> {
    table
        .column_index(name)
        .ok_or_else(|| undefined_column(name))
}

/// 42703 — a column name that no relation in scope defines.
pub(crate) fn undefined_column(name: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedColumn,
        format!("column does not exist: {name}"),
    )
}

/// 42702 — a bare column name that more than one relation in scope defines (grammar.md §15).
pub(crate) fn ambiguous_column(name: &str) -> EngineError {
    EngineError::new(
        SqlState::AmbiguousColumn,
        format!("column reference {name} is ambiguous"),
    )
}

/// 42P01 — a qualifier that names no relation in the FROM clause (grammar.md §15).
pub(crate) fn missing_from_entry(qualifier: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedTable,
        format!("missing FROM-clause entry for table {qualifier}"),
    )
}

/// Resolve a type name + optional type modifier used in a column definition or a CAST target.
/// All canonical names and aliases (including `boolean`/`bool` and `numeric`/`decimal`/`dec`)
/// resolve here; a genuinely unknown name is a 42704. A type modifier is meaningful only for
/// decimal (validated to `numeric(p,s)` — 22023); on any other type it is `0A000` (varchar(n)
/// and other parameterized types are deferred — spec/design/grammar.md §14). Type-specific
/// narrowings (a text/decimal PRIMARY KEY, a CAST to text/boolean) are enforced at the
/// call site, not here.
/// The maximum `varchar(n)` length — PostgreSQL's `varchar` ceiling (spec/design/types.md §15).
/// Stored on disk as a `u32`, so it fits comfortably.
pub(crate) const MAX_VARCHAR_LEN: u32 = 10485760;

/// Resolve a scalar type name + optional type modifier, returning the type, the decimal typmod
/// (when the type is `decimal`), and the `varchar(n)` max length (when the type is `text` —
/// spec/design/types.md §15). At most one of the two typmods is ever `Some` (they belong to
/// different types). A typmod on any other type is `0A000`.
pub(crate) fn resolve_type_and_typmod(
    name: &str,
    type_mod: &Option<TypeMod>,
) -> Result<(ScalarType, Option<DecimalTypmod>, Option<u32>)> {
    let ty = if let Some(ty) = ScalarType::from_name(name) {
        ty
    } else {
        return Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("type does not exist: {name}"),
        ));
    };
    match type_mod {
        None => Ok((ty, None, None)),
        Some(tm) => {
            if ty.is_decimal() {
                Ok((ty, Some(validate_decimal_typmod(tm)?), None))
            } else if ty.is_text() {
                Ok((ty, None, Some(validate_varchar_typmod(tm)?)))
            } else {
                Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "a type modifier is not supported for type {}",
                        ty.canonical_name()
                    ),
                ))
            }
        }
    }
}

/// Validate a `varchar(n)` type modifier: `1 <= n <= 10485760` (PostgreSQL's `varchar` ceiling),
/// else trap 22023 (spec/design/types.md §15). A scale (`varchar(n, m)`) is a syntax error here —
/// `varchar` takes a single length argument.
pub(crate) fn validate_varchar_typmod(tm: &TypeMod) -> Result<u32> {
    if tm.scale.is_some() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "varchar takes exactly one type modifier (a length)".to_string(),
        ));
    }
    let n = tm.precision;
    if n < 1 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "length for type varchar must be at least 1".to_string(),
        ));
    }
    if n > MAX_VARCHAR_LEN as u64 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("length for type varchar cannot exceed {MAX_VARCHAR_LEN}"),
        ));
    }
    Ok(n as u32)
}

/// Validate a decimal `numeric(p[,s])` type modifier: `1 <= p <= 1000`, `0 <= s <= p`; else
/// trap 22023 (spec/design/decimal.md §2). `numeric(p)` means scale 0.
pub(crate) fn validate_decimal_typmod(tm: &TypeMod) -> Result<DecimalTypmod> {
    let p = tm.precision;
    if p < 1 || p > MAX_PRECISION as u64 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("NUMERIC precision {p} must be between 1 and {MAX_PRECISION}"),
        ));
    }
    let s = tm.scale.unwrap_or(0);
    if s > p || s > MAX_SCALE as u64 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("NUMERIC scale {s} must be between 0 and precision {p}"),
        ));
    }
    Ok(DecimalTypmod {
        precision: p as u16,
        scale: s as u16,
    })
}

pub(crate) fn overflow(ty: ScalarType) -> EngineError {
    EngineError::new(
        SqlState::NumericValueOutOfRange,
        format!("value out of range for type {}", ty.canonical_name()),
    )
    .with_data_type(ty.canonical_name())
}

pub(crate) fn type_error(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::DatatypeMismatch, msg.into())
}

/// Decode a single-quoted literal's content as a bytea value via the hex input form
/// (`value::parse_bytea_hex`), mapping malformed hex to a `22P02`
/// (invalid_text_representation). Used when a string literal adapts to a bytea context
/// (types.md §6/§13); the trap is deterministic and fires at resolve time, before any scan.
pub(crate) fn decode_bytea_literal(s: &str) -> Result<Vec<u8>> {
    parse_bytea_hex(s).map_err(|detail| {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type bytea: {detail}"),
        )
    })
}

/// Decode a single-quoted literal's content as a uuid value via PostgreSQL-flexible input
/// (`value::parse_uuid`), mapping malformed input to a `22P02` (invalid_text_representation).
/// Used when a string literal adapts to a uuid context (types.md §6/§14); the trap is
/// deterministic and fires at resolve time, before any scan.
pub(crate) fn decode_uuid_literal(s: &str) -> Result<[u8; 16]> {
    parse_uuid(s).map_err(|detail| {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type uuid: {detail}"),
        )
    })
}

/// Coerce a string literal's content to the named scalar `target` at resolve time — the shared
/// engine of the `type 'string'` typed literal and `CAST(<string literal> AS target)` (PG's
/// text→T cast over a literal operand; spec/design/grammar.md §36, types.md §5). Every scalar is
/// reachable: the string-native types parse by their own input (datetime / interval / bytea /
/// uuid), `text` is identity, and the native-syntax types (int / decimal / boolean) are the cast
/// from text admitted only for a literal operand. Errors: `22P02` malformed / `22003` out of
/// range / the type's own parse code. `typmod` (decimal only) re-scales the result.
/// Coerce a composite text literal `'(…)'` to a folded `Value::Composite` — PostgreSQL's
/// `record_in`, the exact inverse of `record_out` (spec/design/composite.md §8). Used by
/// `'(…)'::type` and the `type '(…)'` typed literal. Tokenizes via `value::parse_record_tokens`
/// (a malformed literal or a field-count mismatch is `22P02`), then coerces each present token to
/// its field's type — a scalar via the same string-literal coercion as a typed literal, a NULL
/// token to a NULL, a nested composite field recursively. Folds to a constant `RExpr::Row` of the
/// coerced field nodes (so `eval` rebuilds the `Value::Composite`), statically typed as the named
/// composite. The recursion is sound because every field type was proven to exist at `CREATE TYPE`.
pub(crate) fn coerce_string_to_composite(
    text: &str,
    ct: &CompositeType,
    catalog: &Engine,
) -> Result<(RExpr, ResolvedType)> {
    let malformed = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("malformed record literal: \"{text}\" for type {}", ct.name),
        )
    };
    let tokens = crate::value::parse_record_tokens(text).ok_or_else(malformed)?;
    if tokens.len() != ct.fields.len() {
        return Err(malformed());
    }
    let mut nodes = Vec::with_capacity(tokens.len());
    let mut field_types = Vec::with_capacity(tokens.len());
    for (tok, f) in tokens.into_iter().zip(ct.fields.iter()) {
        match tok {
            // A NULL field: a NULL value, typed by the field's declared type.
            None => {
                nodes.push(RExpr::ConstNull);
                field_types.push((f.name.clone(), resolved_type_of_col(&f.ty, catalog)));
            }
            Some(s) => {
                let (node, ty) = match &f.ty {
                    Type::Composite(r) => {
                        let nested = catalog
                            .composite_type(&r.name)
                            .expect("nested composite type resolved at CREATE TYPE / load");
                        coerce_string_to_composite(&s, nested, catalog)?
                    }
                    Type::Scalar(scalar) => {
                        coerce_string_literal(&s, *scalar, f.decimal, f.varchar_len)?
                    }
                    // An array-typed field (spec/design/array.md §12): the token is an array text
                    // literal, coerced through `array_in` against the element type — the same path a
                    // bare `'{…}'::T[]` cast uses, one level down. Folds to a constant array.
                    Type::Array(elem_ty) => {
                        let elem_col = resolve_col_type(elem_ty, &catalog.read_snap().types);
                        let val = coerce_string_to_array(&s, &elem_col)?;
                        let rt = resolved_type_of_col(&f.ty, catalog);
                        (value_to_rexpr(&val), rt)
                    }
                    // A range field cannot occur: CREATE TYPE rejects a range field (range columns
                    // are not storable yet — R2), so a composite field type is never a range.
                    Type::Range(_) => {
                        unreachable!("a composite range field is rejected at CREATE TYPE (R2)")
                    }
                };
                nodes.push(node);
                field_types.push((f.name.clone(), ty));
            }
        }
    }
    Ok((
        RExpr::Row(nodes),
        ResolvedType::Composite(Box::new(CompositeRType {
            name: Some(ct.name.clone()),
            fields: field_types,
        })),
    ))
}

/// Coerce a range text literal to a constant range expression (`'[1,5)'::i32range` /
/// `i32range '[1,5)'`). Parses the literal, coerces each bound to the element type via the
/// string-literal coercion, then canonicalizes (spec/design/ranges.md §4/§5). Folds to a
/// `ConstRange`. Malformed → `22P02`; `lower>upper` → `22000`; a canonicalize overflow → `22003`.
/// Resolve an UPDATE assignment RHS against a RANGE or ARRAY column (the caller has already
/// rejected composite — 0A000). Mirrors INSERT's value adaptation (ranges.md §5 / array.md §7): a
/// bare string literal adapts to the container via range_in / array_in, a bare NULL is the typed
/// NULL, and any other expression must resolve to the SAME container type (matching element) else
/// 42804. A top-level `$N` parameter is deferred (0A000) — INSERT's param-to-container handling is
/// special and not generalized to the assignment RHS yet.
pub(crate) fn resolve_container_assign(
    scope: &Scope,
    col: &Column,
    e: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    let col_rt = resolved_type_of_col(&col.ty, scope.catalog);
    // A bare string literal adapts to the container context (the same string-adapts-to-context
    // rule the cast and INSERT VALUES paths use).
    if let Expr::Literal(Literal::Text(s)) = e {
        match &col.ty {
            Type::Range(elem) => {
                let desc = crate::range::range_for_element(elem.scalar())
                    .expect("a range column's element always has a range type");
                let (node, _) = coerce_string_to_range_expr(s, desc)?;
                return Ok(node);
            }
            Type::Array(elem) => {
                let elem_col = resolve_col_type(elem, &scope.catalog.read_snap().types);
                let val = coerce_string_to_array(s, &elem_col)?;
                return Ok(value_to_rexpr(&val));
            }
            _ => unreachable!("resolve_container_assign is only called for range/array columns"),
        }
    }
    if let Expr::Literal(Literal::Null) = e {
        return Ok(RExpr::ConstNull);
    }
    if let Expr::Param(_) = e {
        let kind = if col.ty.is_array() { "array" } else { "range" };
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!(
                "updating {kind} column {} from a parameter is not supported yet",
                col.name
            ),
        ));
    }
    // For an array column over a SCALAR element, pass the element type as the hint so a bare
    // `ARRAY[1,2]` constructor adapts its literal elements to the column's element type (the same
    // adaptation `col = ARRAY[…]` uses — without it, bare int literals would type as i64 and miss a
    // narrower i32[]/i16[] column). A range gets no scalar hint (its bare-literal form was handled
    // above; other forms self-describe their element).
    let hint = col.ty.array_element().and_then(|t| t.as_scalar());
    let (node, ty) = resolve(scope, e, hint, agg, params)?;
    if matches!(ty, ResolvedType::Null) {
        return Ok(node); // a NULL-typed expression (e.g. a CASE that may be NULL)
    }
    // Ranges/arrays compare equal only over equal element types (ResolvedType's derived Eq compares
    // the boxed element), matching the comparison rule (ranges.md §6 / array.md §5).
    if ty != col_rt {
        return Err(type_error(format!(
            "column {} is of type {} but expression is of type {}",
            col.name,
            col.ty.canonical_name(),
            ty.type_name()
        )));
    }
    Ok(node)
}

pub(crate) fn coerce_string_to_range_expr(
    text: &str,
    desc: &crate::ranges_gen::RangeDesc,
) -> Result<(RExpr, ResolvedType)> {
    let val = coerce_string_to_range(text, desc)?;
    let elem_rt = resolved_type_of(crate::range::element_scalar(desc));
    Ok((
        RExpr::ConstRange(Box::new(val)),
        ResolvedType::Range(Box::new(elem_rt)),
    ))
}

/// Parse a range text literal and coerce its bounds to the element type, producing a canonical
/// [`RangeVal`] (spec/design/ranges.md §4). The shared core of the range cast / typed-literal paths.
pub(crate) fn coerce_string_to_range(
    text: &str,
    desc: &crate::ranges_gen::RangeDesc,
) -> Result<RangeVal> {
    let parsed = crate::range::parse_range_text(text)?;
    if parsed.empty {
        return Ok(RangeVal::empty());
    }
    let elem = crate::range::element_scalar(desc);
    let coerce_bound = |b: &Option<String>| -> Result<Option<Value>> {
        match b {
            None => Ok(None),
            Some(s) => {
                let (node, _) = coerce_string_literal(s, elem, None, None)?;
                Ok(Some(rexpr_const_to_value(&node)?))
            }
        }
    };
    let lower = coerce_bound(&parsed.lower)?;
    let upper = coerce_bound(&parsed.upper)?;
    crate::range::finalize(desc, lower, upper, parsed.lower_inc, parsed.upper_inc)
}

pub(crate) fn coerce_string_literal(
    s: &str,
    target: ScalarType,
    typmod: Option<DecimalTypmod>,
    varchar_len: Option<u32>,
) -> Result<(RExpr, ResolvedType)> {
    Ok(match target {
        ScalarType::Bytea => (
            RExpr::ConstBytea(decode_bytea_literal(s)?),
            ResolvedType::Bytea,
        ),
        ScalarType::Uuid => (
            RExpr::ConstUuid(decode_uuid_literal(s)?),
            ResolvedType::Uuid,
        ),
        ScalarType::Timestamp => (
            RExpr::ConstTimestamp(parse_timestamp(s)?),
            ResolvedType::Timestamp,
        ),
        ScalarType::Timestamptz => (
            RExpr::ConstTimestamptz(parse_timestamptz(s)?),
            ResolvedType::Timestamptz,
        ),
        ScalarType::Interval => (
            RExpr::ConstInterval(parse_interval(s)?),
            ResolvedType::Interval,
        ),
        ScalarType::Date => (RExpr::ConstDate(parse_date(s)?), ResolvedType::Date),
        // `json '…'` / CAST('…' AS json) — validate well-formedness, store the bytes verbatim
        // (spec/design/json.md §4); malformed → 22P02.
        ScalarType::Json => {
            json::validate_json(s)?;
            (RExpr::ConstJson(s.to_string()), ResolvedType::Json)
        }
        // `jsonb '…'` / CAST('…' AS jsonb) — parse + canonicalize (numbers→decimal, keys deduped +
        // sorted — §2); malformed → 22P02.
        ScalarType::Jsonb => (
            RExpr::ConstJsonb(Box::new(json::jsonb_in(s)?)),
            ResolvedType::Jsonb,
        ),
        // `'…'::jsonpath` / `jsonpath '…'` — compile (P1a structural subset) + store the canonical
        // normalized text. Malformed → 42601; an unsupported (valid-PG) construct → 0A000.
        ScalarType::JsonPath => (
            RExpr::ConstJsonPath(crate::jsonpath::JsonPath::compile(s)?.render()),
            ResolvedType::JsonPath,
        ),
        // `text 'x'` is identity — the string IS the value. A `varchar(n) 'x'` typed literal /
        // `CAST('x' AS varchar(n))` silently truncates to n code points (the explicit-cast rule,
        // spec/design/types.md §15) — no 22001 at resolve.
        ScalarType::Text => (
            RExpr::ConstText(match varchar_len {
                Some(n) => truncate_to_chars(s, n as usize),
                None => s.to_string(),
            }),
            ResolvedType::Text,
        ),
        ScalarType::Bool => (RExpr::ConstBool(parse_bool_literal(s)?), ResolvedType::Bool),
        ScalarType::Decimal => {
            let d = parse_decimal_literal(s)?;
            let d = match typmod {
                Some(tm) => d.coerce_to_typmod(tm.precision as u32, tm.scale as u32)?,
                None => d.check_cap()?,
            };
            (RExpr::ConstDecimal(d), ResolvedType::Decimal)
        }
        ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64 => (
            RExpr::ConstInt(parse_int_literal(s, target)?),
            ResolvedType::Int(target),
        ),
        // `float '…'` / `real '…'` / CAST('…' AS f64) — parse via the float input function
        // (sign, digits, `.`, e-notation, Infinity/inf/NaN; spec/design/float.md §4). Malformed →
        // 22P02, out of range → 22003.
        ScalarType::Float64 => (
            RExpr::ConstFloat64(parse_f64_literal(s)?),
            ResolvedType::Float(ScalarType::Float64),
        ),
        ScalarType::Float32 => (
            RExpr::ConstFloat32(parse_f32_literal(s)?),
            ResolvedType::Float(ScalarType::Float32),
        ),
    })
}

/// Parse a string literal's content as a `f64` — the text→float coercion for `float '1.5e10'`
/// / `CAST('Infinity' AS f64)` (spec/design/float.md §4). Accepts an optional leading sign,
/// decimal digits with an optional point and `e`-notation, and the case-insensitive special words
/// `Infinity`/`+Infinity`/`-Infinity`/`inf`/`+inf`/`-inf`/`NaN` (PG `float8in` spellings).
/// Surrounding ASCII whitespace is trimmed. Malformed input traps `22P02`; a value outside the
/// binary64 range traps `22003`.
pub(crate) fn parse_f64_literal(s: &str) -> Result<f64> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type f64: \"{s}\""),
        )
    };
    if let Some(v) = parse_float_special_f64(t) {
        return Ok(v);
    }
    // Rust's `f64::from_str` accepts the same finite grammar PG does (sign/digits/point/e-notation),
    // but also `inf`/`nan` spellings — already handled above, so reject any non-finite result that
    // sneaks through (defensive) and any parse failure.
    let v: f64 = t.parse().map_err(|_| invalid())?;
    if v.is_finite() {
        Ok(v)
    } else {
        // A finite-looking literal that overflows binary64 parses to ±Inf — that is 22003, not a
        // first-class infinity (only the special words above produce ±Inf).
        Err(overflow(ScalarType::Float64))
    }
}

/// As [`parse_f64_literal`], for `f32` (binary32). A finite value beyond the binary32 range
/// traps `22003`.
pub(crate) fn parse_f32_literal(s: &str) -> Result<f32> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type f32: \"{s}\""),
        )
    };
    if let Some(v) = parse_float_special_f32(t) {
        return Ok(v);
    }
    let v: f32 = t.parse().map_err(|_| invalid())?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(overflow(ScalarType::Float32))
    }
}

/// Recognize PG's special float spellings (case-insensitive): `infinity`/`inf` (± optional sign),
/// `nan`. Returns the value, or `None` if `t` is not one of them (a finite literal). Shared shape
/// for both widths.
pub(crate) fn parse_float_special_f64(t: &str) -> Option<f64> {
    let lower = t.to_ascii_lowercase();
    let (sign, body) = match lower.strip_prefix('-') {
        Some(r) => (-1.0, r),
        None => (1.0, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    match body {
        "infinity" | "inf" => Some(sign * f64::INFINITY),
        "nan" => Some(f64::NAN),
        _ => None,
    }
}

/// As [`parse_float_special_f64`], at binary32.
pub(crate) fn parse_float_special_f32(t: &str) -> Option<f32> {
    let lower = t.to_ascii_lowercase();
    let (sign, body) = match lower.strip_prefix('-') {
        Some(r) => (-1.0, r),
        None => (1.0, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    match body {
        "infinity" | "inf" => Some(sign * f32::INFINITY),
        "nan" => Some(f32::NAN),
        _ => None,
    }
}

/// Parse a string literal's content as a signed integer of type `ty` — the text→integer coercion
/// for `INTEGER '42'` / `CAST('42' AS int)` (grammar.md §36). Matches jed's OWN integer-literal
/// grammar: surrounding ASCII whitespace trimmed, an optional leading `+`/`-`, then one or more
/// ASCII decimal digits. NO hex/octal/binary or digit underscores (those trap `22P02`, a documented
/// PG divergence). A value outside `ty`'s range traps `22003`; anything else `22P02`.
pub(crate) fn parse_int_literal(s: &str, ty: ScalarType) -> Result<i64> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!(
                "invalid input syntax for type {}: \"{s}\"",
                ty.canonical_name()
            ),
        )
    };
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    // All-digit but too large for i128 is an out-of-range value (22003), not malformed (22P02).
    let mag: i128 = digits.parse().map_err(|_| overflow(ty))?;
    let val = if neg { -mag } else { mag };
    if val < ty.min() as i128 || val > ty.max() as i128 {
        return Err(overflow(ty));
    }
    Ok(val as i64)
}

/// Parse a string literal's content as a decimal — the text→decimal coercion for `NUMERIC '1.5'`
/// / `CAST('1.5' AS numeric)` (grammar.md §36). Matches jed's OWN decimal-literal grammar: trimmed
/// ASCII whitespace, optional sign, ASCII digits with at most one `.` and a digit on at least one
/// side, plus optional scientific `e`-notation (`numeric '1.5e3'` → `1500`) — built into the SAME
/// `(digits, scale)` the lexer feeds `from_digits_scale` (via the shared `decimal_from_parts`), so a
/// `NUMERIC 'x'` is byte-identical to writing `x`. NO `NaN` / `Infinity` and no hex/underscore
/// (those trap `22P02` — jed's decimal is always finite; documented PG divergences). The caller
/// applies the typmod / cap-check.
pub(crate) fn parse_decimal_literal(s: &str) -> Result<Decimal> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type numeric: \"{s}\""),
        )
    };
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    // Split off an optional exponent. Unlike the lexer (which leaves a bare `e` for the next
    // token), an isolated string must be a COMPLETE numeric, so an `e` with no `[+-]?digit+`
    // after it is malformed (`22P02`), matching PG's `numeric_in`.
    let (mantissa, exp) = match rest.find(|c: char| c == 'e' || c == 'E') {
        Some(pos) => {
            let (m, e) = (&rest[..pos], &rest[pos + 1..]);
            let (eneg, edigits) = match e.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, e.strip_prefix('+').unwrap_or(e)),
            };
            if edigits.is_empty() || !edigits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            // Clamp the magnitude to `EXP_LIMIT` while accumulating (keeps it in `i64` and
            // bounds the coefficient the shared builder may materialize).
            let mut v: i64 = 0;
            for b in edigits.bytes() {
                if v < decimal::EXP_LIMIT {
                    v = v * 10 + (b - b'0') as i64;
                    if v > decimal::EXP_LIMIT {
                        v = decimal::EXP_LIMIT;
                    }
                }
            }
            (m, Some(if eneg { -v } else { v }))
        }
        None => (rest, None),
    };
    let mut parts = mantissa.splitn(2, '.');
    let int_part = parts.next().unwrap_or("");
    let frac = parts.next().unwrap_or("");
    // A second `.` lands in `frac` (splitn(2) does not split it); reject it.
    if frac.contains('.')
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || (int_part.is_empty() && frac.is_empty())
    {
        return Err(invalid());
    }
    let (digits, scale) = decimal::decimal_from_parts(int_part, frac, exp);
    Ok(Decimal::from_digits_scale(neg, &digits, scale))
}

/// Parse a string literal's content as a boolean — the text→boolean coercion for `BOOLEAN 'true'`
/// / `CAST('t' AS boolean)` (grammar.md §36). Matches PostgreSQL's `boolin`: trimmed ASCII
/// whitespace, case-insensitive; `t`/`tr`/`tru`/`true`, `y`/`ye`/`yes`, `on`, `1` → true and
/// `f`/`fa`/`fal`/`fals`/`false`, `n`/`no`, `off`, `0` → false; anything else `22P02`.
pub(crate) fn parse_bool_literal(s: &str) -> Result<bool> {
    let t = s
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    match t.as_str() {
        "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" | "1" => Ok(true),
        "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "off" | "0" => Ok(false),
        _ => Err(EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type boolean: \"{s}\""),
        )),
    }
}

/// A resolved `ON CONFLICT` clause (spec/design/upsert.md), built by `resolve_on_conflict`.
pub(crate) struct ConflictPlan {
    /// The arbiter constraint whose violation triggers the action. `None` only with
    /// `DoNothing` (any uniqueness conflict is then skipped).
    pub(crate) arbiter: Option<Arbiter>,
    pub(crate) action: ConflictActionPlan,
}

/// Which uniqueness constraint an `ON CONFLICT` arbitrates (spec/design/upsert.md §2).
pub(crate) enum Arbiter {
    /// The primary key — the arbiter key is the storage key.
    PrimaryKey,
    /// A unique index, by position in the table's `indexes` list.
    Index(usize),
}

/// The resolved `ON CONFLICT` action (spec/design/upsert.md §5).
pub(crate) enum ConflictActionPlan {
    DoNothing,
    DoUpdate {
        assignments: Vec<AssignPlan>,
        filter: Option<RExpr>,
    },
}

/// Resolve an `ON CONFLICT` target into an `Arbiter` (spec/design/upsert.md §2): a column list is
/// matched as an order-independent SET against a unique index / the primary key (no match →
/// 42P10); `ON CONSTRAINT name` names a unique index or the synthesized `<table>_pkey` (miss →
/// 42704). `None` target → `None` arbiter (legal only with `DO NOTHING`).
pub(crate) fn resolve_arbiter(
    tdef: &Table,
    target: Option<&ConflictTarget>,
) -> Result<Option<Arbiter>> {
    let target = match target {
        None => return Ok(None),
        Some(t) => t,
    };
    let pk = tdef.pk_indices();
    match target {
        ConflictTarget::Columns(cols) => {
            let mut want = std::collections::BTreeSet::new();
            for c in cols {
                want.insert(col_idx(tdef, c)?); // unknown column → 42703
            }
            if !pk.is_empty()
                && pk
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    == want
            {
                return Ok(Some(Arbiter::PrimaryKey));
            }
            for (i, def) in tdef.indexes.iter().enumerate() {
                // A conflict-target COLUMN list matches only a plain-column unique index (an
                // expression unique index is arbitrated by `ON CONSTRAINT <name>` — upsert.md §3).
                // A PARTIAL unique index is NOT matched by a bare column list (PostgreSQL requires
                // the predicate to be restated — `ON CONFLICT (amt) WHERE …`, a deferred upsert
                // follow-on, indexes.md §9): so a column target that only a partial index covers
                // reports "no matching arbiter", agreeing with PG.
                if def.unique
                    && def.predicate.is_none()
                    && def.column_ordinals().is_some_and(|c| {
                        c.into_iter().collect::<std::collections::BTreeSet<_>>() == want
                    })
                {
                    return Ok(Some(Arbiter::Index(i)));
                }
            }
            Err(EngineError::new(
                SqlState::InvalidColumnReference,
                "there is no unique or exclusion constraint matching the ON CONFLICT specification",
            ))
        }
        ConflictTarget::Constraint(name) => {
            let pkey = format!("{}_pkey", tdef.name.to_ascii_lowercase());
            if !pk.is_empty() && name.eq_ignore_ascii_case(&pkey) {
                return Ok(Some(Arbiter::PrimaryKey));
            }
            if let Some(i) = tdef
                .indexes
                .iter()
                .position(|d| d.unique && d.name.eq_ignore_ascii_case(name))
            {
                return Ok(Some(Arbiter::Index(i)));
            }
            Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("constraint {} for table {} does not exist", name, tdef.name),
            ))
        }
    }
}

/// The arbiter key of a candidate row (spec/design/upsert.md §3): the storage key for a PK
/// arbiter (never NULL), or the unique-index prefix for an index arbiter (`None` when a nullable
/// arbiter column is NULL — NULLS DISTINCT, so the row never conflicts).
pub(crate) fn arbiter_key(
    arb: &Arbiter,
    pk: &[(usize, Type)],
    colls: &[Option<std::sync::Arc<Collation>>],
    columns: &[Column],
    rindexes: &[ResolvedIndex],
    row: &Row,
    env: &EvalEnv,
) -> Result<Option<Vec<u8>>> {
    match arb {
        Arbiter::PrimaryKey => Ok(Some(encode_pk_key(pk, colls, row)?)),
        Arbiter::Index(i) => index_prefix_key(columns, colls, &rindexes[*i], row, env),
    }
}

/// A resolved UPDATE assignment: which column to write, the target type/nullability so
/// the new value is re-checked exactly like INSERT, and the resolved RHS expression
/// (evaluated against the *old* row).
pub(crate) struct AssignPlan {
    pub(crate) idx: usize,
    pub(crate) name: String,
    pub(crate) target: ScalarType,
    pub(crate) decimal: Option<DecimalTypmod>,
    /// The `varchar(n)` length for a text column (spec/design/types.md §15) — UPDATE re-checks
    /// the new value's length exactly like INSERT (over-length 22001, trailing-space truncate).
    pub(crate) varchar_len: Option<u32>,
    pub(crate) not_null: bool,
    pub(crate) source: RExpr,
    /// The resolved `ColType` for a NON-scalar (range / array) column — `Some` ⇒ `check` stores
    /// through `coerce_for_store` (the container codec, ranges.md §4 / array.md §4); `None` for a
    /// scalar column, which stays on the `store_value` fast path. Composite columns are deferred
    /// (0A000) at resolution, so they never reach here.
    pub(crate) col_type: Option<ColType>,
}
