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
pub(crate) fn eval_array_func(func: &ArrayFunc, vals: &[Value], m: &mut Meter) -> Result<Value> {
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
        ArrayFunc::ArrayReplace => array_replace_value(&vals[0], &vals[1], &vals[2], m),
        ArrayFunc::ArrayToString => {
            unreachable!("array_to_string renders in the session zone (eval)")
        }
        ArrayFunc::StringToArray => Ok(string_to_array_value(vals)),
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
pub(crate) fn eval_range_ctor(elem: ScalarType, vals: &[Value], iz: &InputZone) -> Result<Value> {
    let desc =
        crate::range::range_for_element(elem).expect("a range constructor's elem has a range");
    let lower = coerce_range_bound(vals[0].clone(), elem, iz)?;
    let upper = coerce_range_bound(vals[1].clone(), elem, iz)?;
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
pub(crate) fn coerce_range_bound(
    v: Value,
    elem: ScalarType,
    iz: &InputZone,
) -> Result<Option<Value>> {
    match store_value(v, elem, Typmod::NONE, false, "range bound", iz)? {
        Value::Null => Ok(None),
        other => Ok(Some(other)),
    }
}

/// Evaluate a range boolean operator (range-functions.md §3, RF3) over two already-evaluated operand
/// values. STRICT: a NULL operand → NULL. For the range-against-range operators both operands are
/// ranges; for the element overloads (`ContainsElem`/`ElemContainedBy`) the non-range operand is
/// coerced to the range's element type `elem` (assignment-style, matching the resolver's hint). The
/// boolean kernels live in `range.rs`.
pub(crate) fn eval_range_op(
    op: RangeOp,
    l: &Value,
    r: &Value,
    elem: ScalarType,
    iz: &InputZone,
) -> Result<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let result = match op {
        // `range @> element`: l is the range, r the element (coerced to the range's element type).
        RangeOp::ContainsElem => {
            let e = store_value(r.clone(), elem, Typmod::NONE, false, "range element", iz)?;
            crate::range::range_contains_elem(expect_range(l), &e)
        }
        // `element <@ range`: l is the element, r the range.
        RangeOp::ElemContainedBy => {
            let e = store_value(l.clone(), elem, Typmod::NONE, false, "range element", iz)?;
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
pub(crate) fn array_replace_value(
    arr: &Value,
    from: &Value,
    to: &Value,
    m: &mut Meter,
) -> Result<Value> {
    let a = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("array_replace: array operand"),
    };
    // An amplifier (cost.md §8.1): every match can become a large `to`, so charge the input scan,
    // then size, charge, and reserve the result before building it.
    let input = crate::memsize::payload_bytes(arr);
    m.charge(COSTS.scalar_byte * input);
    m.guard()?;
    let to_bytes = crate::memsize::value_bytes(to);
    let size = a
        .elements
        .iter()
        .filter(|e| not_distinct(e, from))
        .fold(input, |size, e| {
            size + to_bytes - crate::memsize::value_bytes(e)
        });
    m.charge(COSTS.scalar_byte * size);
    m.guard()?;
    m.reserve_scalar(size)?;
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

/// The common type of two operand types at one unification position — a set-operation or VALUES
/// output column, a CASE result arm, a COALESCE / GREATEST / LEAST argument (spec/design/grammar.md
/// §25 "Column types"). The ONE pairwise rule every unifying construct folds, so they cannot drift:
///
/// - a NULL-typed side takes the other side's type;
/// - integer widths promote to the widest, integer with `decimal` → `decimal`;
/// - float widths promote to the widest (`f32` with `f64` → `f64`; float never meets int/decimal);
/// - two arrays unify element-wise when the elements differ only in integer width (a value no-op —
///   every integer is one `i64`); any other element difference is `None`;
/// - two anonymous records (`ROW(…)` results) of the same arity unify field-wise by the same
///   integer-width rule (spec/design/composite.md §13);
/// - otherwise the two types must be IDENTICAL (structural equality: same element for an array or
///   range, same named composite or same anonymous shape) — identical types ALWAYS unify, whatever
///   the family (`json`, `jsonb`, `jsonpath`, `date`, `interval`, ranges, composites, …).
///
/// `None` is the caller's `42804`. Only the int→decimal and f32→f64 widenings change a value; the
/// callers apply them.
pub(crate) fn common_type(a: &ResolvedType, b: &ResolvedType) -> Option<ResolvedType> {
    use ResolvedType::*;
    match (a, b) {
        (Null, x) | (x, Null) => Some(x.clone()),
        (Int(_), Int(_)) => Some(Int(promote(a, b))),
        (Decimal, Decimal) | (Int(_), Decimal) | (Decimal, Int(_)) => Some(Decimal),
        (Float(x), Float(y)) => Some(Float(
            if *x == ScalarType::Float64 || *y == ScalarType::Float64 {
                ScalarType::Float64
            } else {
                ScalarType::Float32
            },
        )),
        (Array(x), Array(y)) => common_array_element(x, y).map(|e| Array(Box::new(e))),
        (Composite(x), Composite(y)) if x.name.is_none() && y.name.is_none() => {
            common_record(x, y).map(Composite)
        }
        _ if a == b => Some(a.clone()),
        _ => None,
    }
}

/// The common element type of two arrays (`common_type`'s array arm): integer widths promote (a
/// value no-op), anything else must be identical. An element-wise int→decimal or f32→f64 widening
/// would have to rewrite every element, so it is not unified (a deferred narrowing — grammar.md §25).
fn common_array_element(x: &ResolvedType, y: &ResolvedType) -> Option<ResolvedType> {
    use ResolvedType::*;
    match (x, y) {
        (Int(_), Int(_)) => Some(Int(promote(x, y))),
        (Array(a), Array(b)) => common_array_element(a, b).map(|e| Array(Box::new(e))),
        (Composite(a), Composite(b)) if a.name.is_none() && b.name.is_none() => {
            common_record(a, b).map(Composite)
        }
        _ if x == y => Some(x.clone()),
        _ => None,
    }
}

/// The common type of two anonymous records (spec/design/composite.md §13): the same arity and field
/// names, each field pair unified by the array-element rule — integer widths promote (a value no-op),
/// anything else must be identical.
fn common_record(x: &CompositeRType, y: &CompositeRType) -> Option<Box<CompositeRType>> {
    if x.fields.len() != y.fields.len() {
        return None;
    }
    let fields = x
        .fields
        .iter()
        .zip(&y.fields)
        .map(|((xn, xt), (yn, yt))| {
            (xn == yn)
                .then(|| common_array_element(xt, yt))
                .flatten()
                .map(|t| (xn.clone(), t))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Box::new(CompositeRType { name: None, fields }))
}

/// Unify a CASE's result-arm types — or a COALESCE's (grammar.md §51) or GREATEST/LEAST's (§52)
/// argument types — into one common type by folding `common_type`; NULL-typed arms are skipped, an
/// all-NULL list is `text` (PostgreSQL's unknown rule), and a pair with no common type is 42804 with
/// the caller's form-specific `mismatch` message.
pub(crate) fn unify_case_types(arms: &[ResolvedType], mismatch: &str) -> Result<ResolvedType> {
    let mut acc: Option<ResolvedType> = None;
    for t in arms.iter().filter(|t| **t != ResolvedType::Null) {
        acc = Some(match acc {
            None => t.clone(),
            Some(u) => common_type(&u, t).ok_or_else(|| type_error(mismatch))?,
        });
    }
    Ok(acc.unwrap_or(ResolvedType::Text))
}

/// The location of the expression a failed type unification names (spec/design/error-fields.md
/// §10.2): PostgreSQL's `select_common_type` scans the expressions in order and reports the first
/// whose type does not unify with those before it. `unifies` is the unifier that failed, run over
/// each prefix; `exprs` is in PostgreSQL's scan order.
pub(crate) fn mismatch_location(
    types: &[ResolvedType],
    exprs: &[&Expr],
    unifies: impl Fn(&[ResolvedType]) -> bool,
) -> SrcPos {
    (1..types.len())
        .find(|&i| !unifies(&types[..=i]))
        .map_or(SrcPos::NONE, |i| exprs[i].location())
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

/// Unify two row value types for the SAME VALUES-body column (spec/design/grammar.md §42) by the
/// set-operation rule (§25, `common_type`); a pair with no common type is 42804.
pub(crate) fn unify_values_column(a: &ResolvedType, b: &ResolvedType) -> Result<ResolvedType> {
    common_type(a, b).ok_or_else(|| {
        EngineError::new(
            SqlState::DatatypeMismatch,
            format!(
                "VALUES types {} and {} cannot be matched",
                a.type_name(),
                b.type_name()
            ),
        )
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

/// Unify one output column's type across the two operands of a set operation (spec/design/grammar.md
/// §25, `common_type`); a pair with no common type is 42804. An all-NULL column stays NULL (PostgreSQL
/// would call a top-level one `text`, but the type is never observed in output).
pub(crate) fn unify_setop_column(
    a: &ResolvedType,
    b: &ResolvedType,
    op: SetOpKind,
) -> Result<ResolvedType> {
    common_type(a, b).ok_or_else(|| {
        EngineError::new(
            SqlState::DatatypeMismatch,
            format!(
                "{} types {} and {} cannot be matched",
                setop_name(op),
                a.type_name(),
                b.type_name()
            ),
        )
    })
}

/// A set operation that MATCHES rows (every form but `UNION ALL`, and a `UNION` recursive CTE) needs
/// an equality operator on each output column (spec/design/grammar.md §25). `json` and `jsonpath`
/// ship none — nor does an array or composite containing one — so such a column is 42883, exactly
/// PostgreSQL's "could not identify an equality operator". The comparability matrix decides it.
pub(crate) fn require_setop_equality(t: &ResolvedType) -> Result<()> {
    crate::executor::resolve::classify_comparable(t, t).map_err(|_| {
        EngineError::new(
            SqlState::UndefinedFunction,
            format!(
                "could not identify an equality operator for type {}",
                t.type_name()
            ),
        )
        .no_hint()
    })
}

/// Widen one value to a unified column type — the value side of `common_type`: an integer becomes a
/// `decimal` (scale 0) and an `f32` becomes the exact `f64`; every other unification is a value
/// no-op (integer-width promotion is free — all integers are `i64`).
pub(crate) fn widen_to_unified(v: Value, to: &ResolvedType) -> Value {
    match (to, v) {
        (ResolvedType::Decimal, Value::Int(n)) => Value::Decimal(Decimal::from_i64(n)),
        (ResolvedType::Float(ScalarType::Float64), Value::Float32(f)) => Value::Float64(f as f64),
        (_, v) => v,
    }
}

/// Convert each row's values in place to the unified set-operation column types (`widen_to_unified`
/// — integer → decimal, f32 → f64; a NULL stays NULL). Only columns whose type actually changed are
/// touched.
pub(crate) fn coerce_setop_rows(
    rows: &mut [Vec<Value>],
    from: &[ResolvedType],
    to: &[ResolvedType],
) {
    for (i, (f, t)) in from.iter().zip(to.iter()).enumerate() {
        let widens = matches!(
            (f, t),
            (ResolvedType::Int(_), ResolvedType::Decimal)
                | (
                    ResolvedType::Float(ScalarType::Float32),
                    ResolvedType::Float(ScalarType::Float64)
                )
        );
        if widens {
            for row in rows.iter_mut() {
                let v = std::mem::replace(&mut row[i], Value::Null);
                row[i] = widen_to_unified(v, t);
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
    acct: &crate::cost::QueryAccount,
) -> Result<Vec<Vec<Value>>> {
    // The dedup / count tables are operator state (memory.md §6.2): one `entry_bytes` per distinct
    // right-arm row of INTERSECT/EXCEPT (built first), then per row a distinct UNION/INTERSECT/EXCEPT
    // keeps — all returned when the combine completes.
    let mut charge = crate::cost::StateCharge::new(acct.clone());
    let entry = |charge: &mut crate::cost::StateCharge, row: &[Value]| -> Result<()> {
        if charge.active() {
            charge.reserve_direct(crate::memsize::entry_bytes(row))?;
        }
        Ok(())
    };
    let out = match (op, all) {
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
                    entry(&mut charge, &row)?;
                    out.push(row);
                }
            }
            out
        }
        // INTERSECT ALL: min(m, n) copies — emit a left row while the right still has budget.
        (SetOpKind::Intersect, true) => {
            let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
            for row in right {
                if !counts.contains_key(&row) {
                    entry(&mut charge, &row)?;
                }
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
            let mut right_set: HashSet<Vec<Value>> = HashSet::new();
            for row in right {
                if !right_set.contains(&row) {
                    entry(&mut charge, &row)?;
                    right_set.insert(row);
                }
            }
            let mut emitted: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left {
                if right_set.contains(&row) && emitted.insert(row.clone()) {
                    entry(&mut charge, &row)?;
                    out.push(row);
                }
            }
            out
        }
        // EXCEPT ALL: max(0, m - n) copies — the right cancels the first n left occurrences.
        (SetOpKind::Except, true) => {
            let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
            for row in right {
                if !counts.contains_key(&row) {
                    entry(&mut charge, &row)?;
                }
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
            let mut right_set: HashSet<Vec<Value>> = HashSet::new();
            for row in right {
                if !right_set.contains(&row) {
                    entry(&mut charge, &row)?;
                    right_set.insert(row);
                }
            }
            let mut emitted: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left {
                if !right_set.contains(&row) && emitted.insert(row.clone()) {
                    entry(&mut charge, &row)?;
                    out.push(row);
                }
            }
            out
        }
    };
    charge.release_all();
    Ok(out)
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
        )
        .with_position(key.pos.0));
    }
    names.iter().position(|n| n == &key.column).ok_or_else(|| {
        EngineError::new(
            SqlState::UndefinedColumn,
            format!("column {} does not exist", key.column),
        )
        .with_position(key.pos.0)
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

/// Require the authoritative scalar storage-assignment predicate. Keeping the predicate on
/// [`ResolvedType::assignable_to`] makes UPDATE / ON CONFLICT DO UPDATE use the same family gate as
/// INSERT ... SELECT and expression defaults; [`store_value`] then performs the value coercion.
pub(crate) fn require_assignable(ty: &ResolvedType, col_ty: ScalarType, col: &str) -> Result<()> {
    if ty.assignable_to(col_ty) {
        Ok(())
    } else {
        Err(assignment_error(format!(
            "cannot assign a value to column {col} of type {}",
            col_ty.canonical_name()
        )))
    }
}

/// Resolve a scalar column's UPDATE / ON CONFLICT DO UPDATE value against the column's type and
/// apply the assignability gate. A bare string literal assigned to a float column adapts through
/// float's input parse at resolve (float.md §4) — the same adaptation INSERT VALUES applies at
/// store. Only a *literal* adapts: a text-typed expression stays text and fails the gate (42804).
pub(crate) fn resolve_scalar_assign(
    scope: &Scope,
    e: &Expr,
    target: ScalarType,
    col: &str,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    if let (true, Expr::Literal(Literal::Text(s), pos)) = (target.is_float(), e) {
        let iz = scope.catalog.input_zone();
        let (node, _) = coerce_string_literal(s, target, Typmod::NONE, &iz)
            .map_err(|err| err.with_position(pos.0))?;
        return Ok(node);
    }
    let (source, ty) = resolve(scope, e, Some(target), &mut AggCtx::Forbidden, params)?;
    require_assignable(&ty, target, col).map_err(|err| err.with_position(e.location().0))?;
    Ok(source)
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

/// Resolve a scalar type name + optional type modifier, returning the type and its typmod: the
/// decimal `numeric(p,s)` (when the type is `decimal`), the `varchar(n)` max length (when the type
/// is `text` — spec/design/types.md §15), or the timestamp precision / interval qualifier and
/// precision (spec/design/timestamp.md §8, interval.md §6.1). At most one part is ever `Some` (each
/// belongs to its own types). A typmod on any other type is `0A000`.
pub(crate) fn resolve_type_and_typmod(
    name: &str,
    type_mod: &Option<TypeMod>,
) -> Result<(ScalarType, Typmod)> {
    let ty = if let Some(ty) = ScalarType::from_name(name) {
        ty
    } else {
        return Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("type does not exist: {name}"),
        ));
    };
    match type_mod {
        None => Ok((ty, Typmod::NONE)),
        Some(tm) => {
            if ty.is_decimal() {
                Ok((
                    ty,
                    Typmod {
                        decimal: Some(validate_decimal_typmod(tm)?),
                        ..Typmod::NONE
                    },
                ))
            } else if ty.is_text() {
                Ok((
                    ty,
                    Typmod {
                        varchar_len: Some(validate_varchar_typmod(tm)?),
                        ..Typmod::NONE
                    },
                ))
            } else if ty.is_timestamp() || ty.is_timestamptz() || ty.is_interval() {
                Ok((
                    ty,
                    Typmod {
                        temporal: Some(validate_temporal_typmod(tm)?),
                        ..Typmod::NONE
                    },
                ))
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
    let n = tm.precision.unwrap_or(0);
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

/// Validate a `timestamp(p)` / `timestamptz(p)` / interval type modifier (spec/design/timestamp.md
/// §8, interval.md §6.1). A precision above 6 is reduced to 6, as PostgreSQL does (it also warns;
/// jed has no warning channel). A second modifier (`timestamp(3, 1)`) is 42601. The parser attaches
/// a field qualifier only to `interval`, and never a negative precision (42601 there).
pub(crate) fn validate_temporal_typmod(tm: &TypeMod) -> Result<TemporalTypmod> {
    if tm.scale.is_some() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "a timestamp or interval takes at most one precision".to_string(),
        ));
    }
    Ok(TemporalTypmod {
        fields: tm.interval_fields,
        precision: tm
            .precision
            .map(|p| p.min(u64::from(crate::timestamp::MAX_TIMESTAMP_PRECISION)) as u8),
    })
}

/// Validate a decimal `numeric(p[,s])` type modifier: `1 <= p <= 1000`, `0 <= s <= p`; else
/// trap 22023 (spec/design/decimal.md §2). `numeric(p)` means scale 0.
pub(crate) fn validate_decimal_typmod(tm: &TypeMod) -> Result<DecimalTypmod> {
    let p = tm.precision.unwrap_or(0);
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

/// A `42804` for a value of the wrong type for its column or default, with PostgreSQL's
/// assignment hint (spec/design/error-fields.md §11.7).
pub(crate) fn assignment_error(msg: impl Into<String>) -> EngineError {
    type_error(msg).hinted(error_hint::ASSIGNMENT_MISMATCH)
}

/// Give a store-time `42804` (a value of the wrong type for its column or default) PostgreSQL's
/// assignment hint, unless its raise site decided otherwise (spec/design/error-fields.md §11.7).
pub(crate) fn assignment_hint(e: EngineError) -> EngineError {
    if e.state == SqlState::DatatypeMismatch {
        e.default_hint(error_hint::ASSIGNMENT_MISMATCH)
    } else {
        e
    }
}

/// Decode a text value as a bytea value — PostgreSQL's `byteain` (types.md §13). A string that
/// begins with `\x` is the hex input form (`value::parse_bytea_hex`, PostgreSQL's
/// `hex_decode_safe`: whitespace between digit pairs is skipped; malformed hex is a `22P02` with
/// PostgreSQL's message — PostgreSQL's code is `22023`); any other string is the traditional escape
/// form, decoded exactly as `decode(s, 'escape')` (`kernels::decode_escape`: `\\` is one backslash, `\nnn` with three octal
/// digits ≤ `\377` is one byte, every other byte is itself, and any other backslash is `22P02`).
/// Used by a string literal adapting to a bytea context (types.md §6), where the trap is
/// deterministic and fires at resolve time before any scan, and by the per-row text → bytea cast.
pub(crate) fn decode_bytea_literal(s: &str) -> Result<Vec<u8>> {
    if !s.starts_with("\\x") {
        return kernels::decode_escape(s.as_bytes());
    }
    parse_bytea_hex(s).map_err(|msg| EngineError::new(SqlState::InvalidTextRepresentation, msg))
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
    iz: &InputZone,
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
                        coerce_string_to_composite(&s, nested, catalog, iz)?
                    }
                    Type::Scalar(scalar) => {
                        coerce_string_literal(&s, *scalar, Typmod::of_field(f), iz)?
                    }
                    // An array-typed field (spec/design/array.md §12): the token is an array text
                    // literal, coerced through `array_in` against the element type — the same path a
                    // bare `'{…}'::T[]` cast uses, one level down. Folds to a constant array.
                    Type::Array(elem_ty) => {
                        let elem_col = resolve_col_type(elem_ty, &catalog.read_snap().types);
                        let val = coerce_string_to_array(&s, &elem_col, iz)?;
                        let rt = resolved_type_of_col(&f.ty, catalog);
                        (value_to_rexpr(&val), rt)
                    }
                    // A range-typed field: the token is a range text literal, coerced through
                    // `range_in` — the same path a bare `'[1,5)'::i32range` cast uses.
                    Type::Range(elem_ty) => {
                        let desc = crate::range::range_for_element(elem_ty.scalar())
                            .expect("a range field's element always has a range type");
                        coerce_string_to_range_expr(&s, desc, iz)?
                    }
                    // A record is never a catalog composite's field type (composite.md §1).
                    Type::Record(_) => {
                        unreachable!("a composite field is never an anonymous record")
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
/// Resolve an UPDATE (or ON CONFLICT DO UPDATE, or INSERT … VALUES expression slot) assignment
/// RHS against a RANGE, ARRAY, or COMPOSITE column. Mirrors INSERT's value adaptation (ranges.md §5
/// / array.md §7 / composite.md §8): a bare string literal adapts to the container via range_in /
/// array_in / record_in, a bare NULL is the typed NULL, a `ROW(…)` constructor assigned to a
/// composite coerces field-by-field to the composite's field types (composite.md §12, PostgreSQL's
/// record → composite assignment coercion), and any other expression must resolve to the SAME
/// container type (matching element / the same named composite) else 42804 — except an anonymous
/// record that is not a `ROW(…)` constructor (a CASE, a subquery, a derived column), which is
/// PostgreSQL's 42846 "cannot cast type record to addr".
pub(crate) fn resolve_container_assign(
    scope: &Scope,
    col: &Column,
    e: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    resolve_container_assign_ty(scope, &col.ty, &col.name, e, agg, params)
}

/// How a `ROW(…)` constructor's fields coerce to a named composite's field types
/// (spec/design/composite.md §12): by the ASSIGNMENT rule of a stored column (each field must be
/// assignable to its field type, the value coerced at store), or by an EXPLICIT `::type` cast (each
/// field explicitly cast to its field type) — PostgreSQL's `coerce_record_to_complex` under the
/// assignment and explicit coercion contexts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowCoercion {
    Assign,
    Explicit,
}

/// The `42846` cannot_coerce "cannot cast type record to T" a record → named-composite coercion
/// raises, as PostgreSQL does (composite.md §8/§12).
pub(crate) fn record_cast_error(type_name: &str, detail: &str) -> EngineError {
    let msg = if detail.is_empty() {
        format!("cannot cast type record to {type_name}")
    } else {
        format!("cannot cast type record to {type_name}: {detail}")
    };
    EngineError::new(SqlState::CannotCoerce, msg).no_hint()
}

/// Coerce a `ROW(…)` (or bare `(a, b, …)`) constructor's field expressions to the named composite
/// `ct` (spec/design/composite.md §12), recursing for a nested composite field (a nested `ROW(…)`
/// coerces one level down). The field count must equal the type's (42846 otherwise). Under
/// [`RowCoercion::Assign`] each field resolves with its field type as the literal hint and must be
/// assignable to it — a scalar field by the column assignment rule, a container field by
/// [`resolve_container_assign_ty`] — and the value is coerced when stored (store_composite).
/// Under [`RowCoercion::Explicit`] each field is explicitly cast to its field type (the scalar /
/// container cast surface), so the row's values carry the target types at eval. Returns an
/// `RExpr::Row` typed as the named composite.
pub(crate) fn resolve_row_to_composite(
    scope: &Scope,
    items: &[Expr],
    ct: &CompositeType,
    mode: RowCoercion,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if items.len() != ct.fields.len() {
        let which = if items.len() < ct.fields.len() {
            "input has too few columns"
        } else {
            "input has too many columns"
        };
        return Err(record_cast_error(&ct.name, which));
    }
    let mut nodes = Vec::with_capacity(items.len());
    for (item, f) in items.iter().zip(ct.fields.iter()) {
        let node = match mode {
            RowCoercion::Explicit => {
                let (type_name, type_mod) = field_cast_target(f);
                let cast = Expr::Cast {
                    inner: Box::new(item.clone()),
                    type_name,
                    type_mod,
                    pos: SrcPos::NONE,
                    cast_pos: SrcPos::NONE,
                    type_pos: SrcPos::NONE,
                };
                resolve(scope, &cast, None, agg, params)?.0
            }
            RowCoercion::Assign => match &f.ty {
                Type::Scalar(target) => {
                    let (node, ty) = resolve(scope, item, Some(*target), agg, params)?;
                    if !ty.assignable_to(*target) {
                        return Err(record_cast_error(
                            &ct.name,
                            &format!(
                                "field {} is of type {} but expression is of type {}",
                                f.name,
                                f.ty.canonical_name(),
                                ty.type_name()
                            ),
                        )
                        .with_position(item.location().0));
                    }
                    node
                }
                // A container field that does not coerce fails the whole record → composite
                // coercion: PostgreSQL's 42846 cannot_coerce, not the column-level 42804.
                _ => resolve_container_assign_ty(scope, &f.ty, &f.name, item, agg, params)
                    .map_err(|mut e| {
                        if e.state == SqlState::DatatypeMismatch {
                            e.state = SqlState::CannotCoerce;
                        }
                        e
                    })?,
            },
        };
        nodes.push(node);
    }
    let rt = resolved_type_of_col(
        &Type::Composite(crate::types::CompositeRef {
            name: ct.name.clone(),
        }),
        scope.catalog,
    );
    Ok((RExpr::Row(nodes), rt))
}

/// The cast target (type name + modifier) naming a composite field's declared type — the
/// `Expr::Cast` an explicit `ROW(…)::type` cast applies to that field: a scalar's canonical name
/// (with its `numeric(p,s)`, `varchar(n)`, or timestamp/interval modifier), a composite's name, an array's `T[]`, a
/// range's name.
fn field_cast_target(f: &CompositeField) -> (String, Option<TypeMod>) {
    let type_mod = match (f.decimal, f.varchar_len, f.temporal) {
        (Some(t), _, _) if t.precision > 0 => Some(TypeMod {
            precision: Some(u64::from(t.precision)),
            scale: Some(u64::from(t.scale)),
            interval_fields: 0,
        }),
        (_, Some(n), _) => Some(TypeMod {
            precision: Some(u64::from(n)),
            scale: None,
            interval_fields: 0,
        }),
        (_, _, Some(t)) => Some(TypeMod {
            precision: t.precision.map(u64::from),
            scale: None,
            interval_fields: t.fields,
        }),
        _ => None,
    };
    (f.ty.canonical_name(), type_mod)
}

/// [`resolve_container_assign`] over a bare container type and the name its errors cite — shared by
/// a column (UPDATE / DO UPDATE / an INSERT expression slot) and a composite field one `ROW(…)` level
/// down ([`resolve_row_to_composite`]).
pub(crate) fn resolve_container_assign_ty(
    scope: &Scope,
    col_ty: &Type,
    col_name: &str,
    e: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    let col_rt = resolved_type_of_col(col_ty, scope.catalog);
    // A bare string literal adapts to the container context (the same string-adapts-to-context
    // rule the cast and INSERT VALUES paths use).
    if let Expr::Literal(Literal::Text(s), pos) = e {
        let iz = scope.catalog.input_zone();
        match col_ty {
            Type::Range(elem) => {
                let desc = crate::range::range_for_element(elem.scalar())
                    .expect("a range column's element always has a range type");
                let (node, _) = coerce_string_to_range_expr(s, desc, &iz)
                    .map_err(|e| e.with_position(pos.0))?;
                params.note_input_zone(&iz);
                return Ok(node);
            }
            Type::Array(elem) => {
                let elem_col = resolve_col_type(elem, &scope.catalog.read_snap().types);
                let val = coerce_string_to_array(s, &elem_col, &iz)
                    .map_err(|e| e.with_position(pos.0))?;
                params.note_input_zone(&iz);
                return Ok(value_to_rexpr(&val));
            }
            Type::Composite(r) => {
                let ct = scope
                    .catalog
                    .composite_type(&r.name)
                    .expect("a composite column's type exists in the catalog");
                let (node, _) = coerce_string_to_composite(s, ct, scope.catalog, &iz)
                    .map_err(|e| e.with_position(pos.0))?;
                params.note_input_zone(&iz);
                return Ok(node);
            }
            // PostgreSQL's record_in: "input of anonymous composite types is not implemented".
            Type::Record(_) => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "input of anonymous composite types is not implemented",
                )
                .with_position(pos.0));
            }
            Type::Scalar(_) => {
                unreachable!("resolve_container_assign is only called for containers")
            }
        }
    }
    if let Expr::Literal(Literal::Null, _) = e {
        return Ok(RExpr::ConstNull);
    }
    // A bind parameter assigned to a container column takes the column's type (api.md §5).
    if let Expr::Param(n1, _) = e {
        let ct = resolve_col_type(col_ty, &scope.catalog.read_snap().types);
        return Ok(resolve_param_node(scope, *n1, Some(ct), col_rt, params)?.0);
    }
    // A `ROW(…)` constructor assigned to a composite coerces field-by-field (composite.md §12).
    if let (Type::Composite(r), Expr::Row(items, _)) = (col_ty, e) {
        let ct = scope
            .catalog
            .composite_type(&r.name)
            .expect("a composite column's type exists in the catalog");
        return Ok(resolve_row_to_composite(scope, items, ct, RowCoercion::Assign, agg, params)?.0);
    }
    // For an array column over a SCALAR element, pass the element type as the hint so a bare
    // `ARRAY[1,2]` constructor adapts its literal elements to the column's element type (the same
    // adaptation `col = ARRAY[…]` uses — without it, bare int literals would type as i64 and miss a
    // narrower i32[]/i16[] column). A range gets no scalar hint (its bare-literal form was handled
    // above; other forms self-describe their element).
    let hint = col_ty.array_element().and_then(|t| t.as_scalar());
    let (node, ty) = resolve(scope, e, hint, agg, params)?;
    if matches!(ty, ResolvedType::Null) {
        return Ok(node); // a NULL-typed expression (e.g. a CASE that may be NULL)
    }
    // An anonymous record that is not a `ROW(…)` constructor (a CASE over rows, a scalar subquery,
    // a derived column) has no field expressions to coerce — PostgreSQL's "cannot cast type record
    // to addr" (composite.md §12).
    if let (Type::Composite(r), ResolvedType::Composite(c)) = (col_ty, &ty)
        && c.name.is_none()
    {
        return Err(record_cast_error(&r.name, "").with_position(e.location().0));
    }
    // Ranges/arrays compare equal only over equal element types (ResolvedType's derived Eq compares
    // the boxed element), matching the comparison rule (ranges.md §6 / array.md §5).
    if ty != col_rt {
        return Err(assignment_error(format!(
            "column {} is of type {} but expression is of type {}",
            col_name,
            col_ty.canonical_name(),
            ty.type_name()
        ))
        .with_position(e.location().0));
    }
    Ok(node)
}

pub(crate) fn coerce_string_to_range_expr(
    text: &str,
    desc: &crate::ranges_gen::RangeDesc,
    iz: &InputZone,
) -> Result<(RExpr, ResolvedType)> {
    let val = coerce_string_to_range(text, desc, iz)?;
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
    iz: &InputZone,
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
                let (node, _) = coerce_string_literal(s, elem, Typmod::NONE, iz)?;
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
    tm: Typmod,
    iz: &InputZone,
) -> Result<(RExpr, ResolvedType)> {
    let typmod = tm.decimal;
    let varchar_len = tm.varchar_len;
    Ok(match target {
        ScalarType::Bytea => (
            RExpr::ConstBytea(decode_bytea_literal(s)?),
            ResolvedType::Bytea,
        ),
        ScalarType::Uuid => (
            RExpr::ConstUuid(decode_uuid_literal(s)?),
            ResolvedType::Uuid,
        ),
        // A `timestamp(p)` target rounds the parsed value (timestamp.md §8); a qualified interval
        // target reads the text under its qualifier and cuts it (interval.md §6.1).
        ScalarType::Timestamp => (
            RExpr::ConstTimestamp(adjust_timestamp_typmod(parse_timestamp(s)?, tm.temporal)?),
            ResolvedType::Timestamp,
        ),
        ScalarType::Timestamptz => (
            RExpr::ConstTimestamptz(adjust_timestamp_typmod(
                iz.parse_timestamptz(s)?,
                tm.temporal,
            )?),
            ResolvedType::Timestamptz,
        ),
        ScalarType::Interval => (
            RExpr::ConstInterval(interval::parse_interval_typmod(s, tm.temporal)?),
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
            RExpr::ConstJsonPath(crate::jsonpath::JsonPath::compile(s, iz.casing())?.render()),
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

/// PostgreSQL's `isspace` in the C locale — space, tab, LF, VT, FF, CR — the whitespace
/// `pg_strtoint*` and `numeric_in` skip around a number.
fn is_pg_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The radix a `0x`/`0o`/`0b` prefix at `bytes[p..]` names (16 / 8 / 2), or 10 when none does.
fn radix_prefix(bytes: &[u8], p: usize) -> u32 {
    if bytes.get(p) != Some(&b'0') {
        return 10;
    }
    match bytes.get(p + 1) {
        Some(b'x' | b'X') => 16,
        Some(b'o' | b'O') => 8,
        Some(b'b' | b'B') => 2,
        _ => 10,
    }
}

/// Is `bytes[p]` a digit of radix `base`?
fn is_radix_digit(bytes: &[u8], p: usize, base: u32) -> bool {
    bytes.get(p).is_some_and(|&c| char::from(c).is_digit(base))
}

/// Parse a string's content as a signed integer of type `ty` — the text→integer coercion for
/// `INTEGER '42'` / `CAST('42' AS int)` and every other integer input (grammar.md §36). Exactly
/// PostgreSQL's `pg_strtoint16/32/64`: surrounding whitespace skipped, an optional `+`/`-`, then
/// decimal digits or a `0x`/`0o`/`0b` radix prefix and its digits, with single `_` separators
/// between digits (`1_000`, `0x_ff`; never leading in decimal, trailing, or doubled). A
/// magnitude that overflows while the digits are read traps `22003` at once (so `'99999999999x'`
/// is out of range, as in PostgreSQL); a malformed string is `22P02`; a value read in full that
/// is outside `ty`'s range is `22003`.
pub(crate) fn parse_int_literal(s: &str, ty: ScalarType) -> Result<i64> {
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!(
                "invalid input syntax for type {}: \"{s}\"",
                ty.canonical_name()
            ),
        )
    };
    let bytes = s.as_bytes();
    let mut p = 0;
    while p < bytes.len() && is_pg_space(bytes[p]) {
        p += 1;
    }
    let neg = bytes.get(p) == Some(&b'-');
    if neg || bytes.get(p) == Some(&b'+') {
        p += 1;
    }
    let base = radix_prefix(bytes, p);
    if base != 10 {
        p += 2;
    }
    // The unsigned magnitude of `ty`'s minimum, 2^(bits-1): the largest magnitude any sign admits.
    let limit = ty.min().unsigned_abs();
    let first = p;
    let mut mag: u64 = 0;
    loop {
        if is_radix_digit(bytes, p, base) {
            if mag > limit / u64::from(base) {
                return Err(overflow(ty));
            }
            let d = char::from(bytes[p]).to_digit(base).expect("a radix digit");
            mag = mag * u64::from(base) + u64::from(d);
            p += 1;
        } else if bytes.get(p) == Some(&b'_') {
            // A decimal number may not start with `_`; every `_` must be followed by a digit.
            if base == 10 && p == first {
                return Err(invalid());
            }
            p += 1;
            if !is_radix_digit(bytes, p, base) {
                return Err(invalid());
            }
        } else {
            break;
        }
    }
    if p == first {
        return Err(invalid());
    }
    while p < bytes.len() && is_pg_space(bytes[p]) {
        p += 1;
    }
    if p != bytes.len() {
        return Err(invalid());
    }
    if neg {
        if mag > limit {
            return Err(overflow(ty));
        }
        Ok((-i128::from(mag)) as i64)
    } else {
        if mag > limit - 1 {
            return Err(overflow(ty));
        }
        Ok(mag as i64)
    }
}

/// Parse a string's content as a decimal — the text→decimal coercion for `NUMERIC '1.5'` /
/// `CAST('1.5' AS numeric)` and every other decimal input (grammar.md §36). PostgreSQL's
/// `numeric_in`: surrounding whitespace skipped, an optional sign, then either a `0x`/`0o`/`0b`
/// radix integer (`numeric '0x10'` is `16`) or ASCII digits with at most one `.` (a digit after a
/// leading `.`), optional scientific `e`-notation (`numeric '1.5e3'` → `1500`), and single `_`
/// separators between digits (`1_000.000_1`; not next to the `.`, leading, trailing, or doubled).
/// The decimal form builds the SAME `(digits, scale)` the lexer feeds `from_digits_scale` (via the
/// shared `decimal_from_parts`), so a `NUMERIC 'x'` is byte-identical to writing `x`. NO `NaN` /
/// `Infinity` (`22P02` — jed's decimal is always finite, a documented PG divergence). The caller
/// applies the typmod / cap-check.
pub(crate) fn parse_decimal_literal(s: &str) -> Result<Decimal> {
    let invalid = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type numeric: \"{s}\""),
        )
    };
    let bytes = s.as_bytes();
    let at = |k: usize| bytes.get(k).copied().unwrap_or(0);
    let mut p = 0;
    while p < bytes.len() && is_pg_space(bytes[p]) {
        p += 1;
    }
    let neg = at(p) == b'-';
    if neg || at(p) == b'+' {
        p += 1;
    }
    // Every finite numeric starts with a digit or a `.` (anything else is PostgreSQL's NaN /
    // Infinity, which jed's decimal does not have).
    if !(at(p).is_ascii_digit() || at(p) == b'.') {
        return Err(invalid());
    }
    let base = radix_prefix(bytes, p);
    let value = if base != 10 {
        p += 2;
        let first = p;
        let mut digits = String::new();
        loop {
            if is_radix_digit(bytes, p, base) {
                digits.push(char::from(bytes[p]));
                p += 1;
            } else if at(p) == b'_' {
                p += 1;
                if !is_radix_digit(bytes, p, base) {
                    return Err(invalid());
                }
            } else {
                break;
            }
        }
        if p == first {
            return Err(invalid());
        }
        Decimal::from_radix_digits(neg, &digits, base)?
    } else {
        let mut int_part = String::new();
        let mut frac = String::new();
        let mut have_dp = false;
        if at(p) == b'.' {
            have_dp = true;
            p += 1;
        }
        if !at(p).is_ascii_digit() {
            return Err(invalid());
        }
        loop {
            let c = at(p);
            if c.is_ascii_digit() {
                if have_dp {
                    frac.push(char::from(c));
                } else {
                    int_part.push(char::from(c));
                }
                p += 1;
            } else if c == b'.' {
                // A second `.`, or a `_` right after the `.`, is malformed.
                if have_dp || at(p + 1) == b'_' {
                    return Err(invalid());
                }
                have_dp = true;
                p += 1;
            } else if c == b'_' {
                p += 1;
                if !at(p).is_ascii_digit() {
                    return Err(invalid());
                }
            } else {
                break;
            }
        }
        let mut exp = None;
        if at(p) == b'e' || at(p) == b'E' {
            p += 1;
            let eneg = at(p) == b'-';
            if eneg || at(p) == b'+' {
                p += 1;
            }
            if !at(p).is_ascii_digit() {
                return Err(invalid());
            }
            // Clamp the magnitude to `EXP_LIMIT` while accumulating (keeps it in `i64` and
            // bounds the coefficient the shared builder may materialize).
            let mut v: i64 = 0;
            loop {
                let c = at(p);
                if c.is_ascii_digit() {
                    if v < decimal::EXP_LIMIT {
                        v = (v * 10 + i64::from(c - b'0')).min(decimal::EXP_LIMIT);
                    }
                    p += 1;
                } else if c == b'_' {
                    p += 1;
                    if !at(p).is_ascii_digit() {
                        return Err(invalid());
                    }
                } else {
                    break;
                }
            }
            exp = Some(if eneg { -v } else { v });
        }
        let (digits, scale) = decimal::decimal_from_parts(&int_part, &frac, exp);
        Decimal::from_digits_scale(neg, &digits, scale)
    };
    while p < bytes.len() && is_pg_space(bytes[p]) {
        p += 1;
    }
    if p != bytes.len() {
        return Err(invalid());
    }
    Ok(value)
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
    /// An `EXCLUDE` constraint, by position in the table's `exclusions` list — named by
    /// `ON CONSTRAINT`, and legal only with `DO NOTHING` (spec/design/upsert.md §2, PostgreSQL's
    /// `42809` for `DO UPDATE`).
    Exclusion(usize),
}

/// The resolved `ON CONFLICT` action (spec/design/upsert.md §5).
pub(crate) enum ConflictActionPlan {
    DoNothing,
    DoUpdate {
        assignments: Vec<AssignPlan>,
        filter: Option<RExpr>,
    },
}

/// The written-ordering check on one `ON CONFLICT` inference element (spec/design/upsert.md §2),
/// raised before the element's column is looked up — PostgreSQL's order: `ASC`/`DESC`, then
/// `NULLS FIRST|LAST`, each 42P10.
pub(crate) fn check_conflict_ordering(c: &ConflictColumn) -> Result<()> {
    if c.ordering {
        return Err(EngineError::new(
            SqlState::InvalidColumnReference,
            "ASC/DESC is not allowed in ON CONFLICT clause",
        ));
    }
    if c.nulls {
        return Err(EngineError::new(
            SqlState::InvalidColumnReference,
            "NULLS FIRST/LAST is not allowed in ON CONFLICT clause",
        ));
    }
    Ok(())
}

/// The decoration-name check on one `ON CONFLICT` inference element (spec/design/upsert.md §2),
/// after its column is found: the collation must be `C` or loaded, the operator class one of
/// jed's B-tree classes — each 42704.
pub(crate) fn check_conflict_names(catalog: &Engine, c: &ConflictColumn) -> Result<()> {
    if let Some(coll) = &c.collation {
        resolve_collation_name(catalog, coll)?;
    }
    if let Some(op) = &c.opclass
        && canonical_btree_opclass(op).is_none()
    {
        return Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("operator class \"{op}\" does not exist for access method \"btree\""),
        ));
    }
    Ok(())
}

/// Every scalar type, for the operator-class name set.
const ALL_SCALARS: [ScalarType; 17] = [
    ScalarType::Int16,
    ScalarType::Int32,
    ScalarType::Int64,
    ScalarType::Text,
    ScalarType::Bool,
    ScalarType::Decimal,
    ScalarType::Bytea,
    ScalarType::Uuid,
    ScalarType::Timestamp,
    ScalarType::Timestamptz,
    ScalarType::Interval,
    ScalarType::Float32,
    ScalarType::Float64,
    ScalarType::Date,
    ScalarType::Json,
    ScalarType::Jsonb,
    ScalarType::JsonPath,
];

/// A B-tree operator-class name resolved to the class jed keys with (spec/design/upsert.md §2):
/// a scalar's default class, a container's polymorphic class, or `varchar_ops` — PostgreSQL's
/// alias of `text_ops` (same family and input type). `None` names no jed class.
pub(crate) fn canonical_btree_opclass(name: &str) -> Option<&'static str> {
    if name == "varchar_ops" {
        return Some("text_ops");
    }
    ["array_ops", "range_ops", "record_ops"]
        .into_iter()
        .chain(ALL_SCALARS.iter().filter_map(|s| s.btree_opclass()))
        .find(|o| *o == name)
}

/// The B-tree operator class a column's keys use: its type's default class (upsert.md §2).
fn column_btree_opclass(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::Scalar(s) => s.btree_opclass(),
        Type::Array(_) => Some("array_ops"),
        Type::Range(_) => Some("range_ops"),
        Type::Composite(_) => Some("record_ops"),
        Type::Record(_) => None,
    }
}

/// Whether a column's index keys satisfy an inference element's decorations (upsert.md §2): a
/// `COLLATE` needs a collatable column (text / text[]) frozen under that collation, an operator
/// class needs the column type's default class. Every jed key over a column shares the column's
/// collation and class, so this depends on the column alone.
fn conflict_decorations_hold(col: &Column, c: &ConflictColumn) -> bool {
    if let Some(coll) = &c.collation {
        let collatable = match &col.ty {
            Type::Scalar(ScalarType::Text) => true,
            Type::Array(e) => matches!(**e, Type::Scalar(ScalarType::Text)),
            _ => false,
        };
        if !collatable || col.collation.as_deref().unwrap_or("C") != coll.as_str() {
            return false;
        }
    }
    if let Some(op) = &c.opclass
        && canonical_btree_opclass(op) != column_btree_opclass(&col.ty)
    {
        return false;
    }
    true
}

/// Resolve an `ON CONFLICT` target into an `Arbiter` (spec/design/upsert.md §2): a column list is
/// matched as an order-independent SET against a unique index / the primary key (no match →
/// 42P10), each element first checked in order (ASC/DESC 42P10, column 42703, collation /
/// operator class 42704) and its decorations then narrowing the match; `ON CONSTRAINT name` names
/// a unique index or the synthesized `<table>_pkey` (miss → 42704). `None` target → `None`
/// arbiter (legal only with `DO NOTHING`).
///
/// `index_predicate` is the resolved `WHERE index_predicate` of a column target (table-local
/// columns): with it, a PARTIAL unique index whose predicate it implies (indexes.md §9.1) is also
/// inferable, after the PK and every non-partial unique index (each in name byte order). Without it
/// no partial index is inferred, as in PostgreSQL. The decorations narrow partial candidates too.
pub(crate) fn resolve_arbiter(
    catalog: &Engine,
    tdef: &Table,
    target: Option<&ConflictTarget>,
    index_predicate: Option<&RExpr>,
) -> Result<Option<Arbiter>> {
    let target = match target {
        None => return Ok(None),
        Some(t) => t,
    };
    let pk = tdef.pk_indices();
    match target {
        ConflictTarget::Columns(cols) => {
            let mut want = std::collections::BTreeSet::new();
            let mut decorations_hold = true;
            for c in cols {
                check_conflict_ordering(c)?;
                let idx = col_idx(tdef, &c.name)?; // unknown column → 42703
                check_conflict_names(catalog, c)?;
                decorations_hold &= conflict_decorations_hold(&tdef.columns[idx], c);
                want.insert(idx);
            }
            // A decoration its column's keys do not satisfy rules out every index (upsert.md §2).
            if decorations_hold
                && !pk.is_empty()
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
                // A PARTIAL unique index is matched only through an implying `WHERE
                // index_predicate` (below), as in PostgreSQL.
                if decorations_hold
                    && def.unique
                    && def.predicate.is_none()
                    && def.column_ordinals().is_some_and(|c| {
                        c.into_iter().collect::<std::collections::BTreeSet<_>>() == want
                    })
                {
                    return Ok(Some(Arbiter::Index(i)));
                }
            }
            if let Some(filter) = index_predicate.filter(|_| decorations_hold) {
                for (i, def) in tdef.indexes.iter().enumerate() {
                    if def.unique
                        && def.predicate.is_some()
                        && def.column_ordinals().is_some_and(|c| {
                            c.into_iter().collect::<std::collections::BTreeSet<_>>() == want
                        })
                        && partial_index_implied(filter, tdef, 0, def, catalog)
                    {
                        return Ok(Some(Arbiter::Index(i)));
                    }
                }
            }
            Err(EngineError::new(
                SqlState::InvalidColumnReference,
                "there is no unique or exclusion constraint matching the ON CONFLICT specification",
            ))
        }
        ConflictTarget::Constraint(name) => {
            let pkey = pkey_name(&tdef.name);
            if !pk.is_empty() && name == &pkey {
                return Ok(Some(Arbiter::PrimaryKey));
            }
            if let Some(i) = tdef
                .indexes
                .iter()
                .position(|d| d.is_unique_constraint() && d.name == *name)
            {
                return Ok(Some(Arbiter::Index(i)));
            }
            if let Some(i) = tdef.exclusions.iter().position(|e| e.name == *name) {
                return Ok(Some(Arbiter::Exclusion(i)));
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
    col_types: &[ColType],
    colls: &[Option<std::sync::Arc<Collation>>],
    rindexes: &[ResolvedIndex],
    row: &Row,
    env: &EvalEnv,
) -> Result<Option<Vec<u8>>> {
    match arb {
        Arbiter::PrimaryKey => Ok(Some(encode_pk_key(pk, col_types, colls, row)?)),
        Arbiter::Index(i) => index_prefix_key(col_types, colls, &rindexes[*i], row, env),
        Arbiter::Exclusion(_) => unreachable!("an exclusion arbiter has no arbiter key"),
    }
}

/// A resolved UPDATE assignment: which column to write, the target type/nullability so
/// the new value is re-checked exactly like INSERT, and the resolved RHS expression
/// (evaluated against the *old* row).
pub(crate) struct AssignPlan {
    pub(crate) idx: usize,
    pub(crate) name: String,
    pub(crate) target: ScalarType,
    /// The column's typmod — UPDATE re-coerces the new value exactly like INSERT (a `varchar(n)`
    /// over-length 22001 / trailing-space truncate, a `numeric(p,s)` rescale, a `timestamp(p)`
    /// rounding, an interval qualifier cut).
    pub(crate) typmod: Typmod,
    pub(crate) not_null: bool,
    pub(crate) source: RExpr,
    /// The resolved `ColType` for a NON-scalar column — `Some` ⇒ `check` stores through
    /// `coerce_for_store`; `None` for a scalar column, which stays on the `store_value` fast path.
    /// A composite column's value (a same-typed expression, a `ROW(…)` coerced field-by-field, or
    /// its `DEFAULT`) is re-coerced field-by-field here (composite.md §12).
    pub(crate) col_type: Option<ColType>,
}
