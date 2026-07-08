//! Value storage coercion — coercing a Value to a column's declared type before it is written (mirrors
//! part of impl/go store_encode.go): the AssignPlan re-check and store_value with its container helpers
//! (store_range/store_array/store_composite) and the coerce/materialize-insert-value path.

use super::*;

impl AssignPlan {
    /// Type-check + coerce a candidate value against this column — the same store path INSERT
    /// uses (NULL into NOT NULL → 23502; an integer outside range → 22003; an integer into a
    /// decimal column widens and coerces to the typmod; a decimal into a decimal column rounds
    /// to its scale; a boolean into a boolean column is accepted as-is; a range/array re-coerces
    /// its elements). The resolver already proved the value's family is assignable (never
    /// decimal→int implicitly).
    pub(crate) fn check(&self, v: Value) -> Result<Value> {
        match &self.col_type {
            Some(ct) => coerce_for_store(
                v,
                ct,
                self.decimal,
                self.varchar_len,
                self.not_null,
                &self.name,
            ),
            None => store_value(
                v,
                self.target,
                self.decimal,
                self.varchar_len,
                self.not_null,
                &self.name,
            ),
        }
    }
}

/// Coerce a value into a column for storage (shared by INSERT and UPDATE). NULL honours NOT
/// NULL (23502); an integer into an integer column is range-checked (22003); an integer into
/// a decimal column widens (int→decimal) then coerces to the typmod; a decimal into a decimal
/// column coerces to the typmod (rounds to scale, precision-checks → 22003); a cross-family
/// value (decimal→int, text→int, etc.) is a 42804 (decimal→int is explicit-CAST only).
pub(crate) fn store_value(
    v: Value,
    col_ty: ScalarType,
    typmod: Option<DecimalTypmod>,
    varchar_len: Option<u32>,
    not_null: bool,
    col_name: &str,
) -> Result<Value> {
    match v {
        Value::Null => {
            if not_null {
                return Err(EngineError::not_null_violation(col_name));
            }
            Ok(Value::Null)
        }
        Value::Int(n) => {
            if col_ty.is_integer() {
                if col_ty.in_range(n) {
                    Ok(Value::Int(n))
                } else {
                    Err(overflow(col_ty))
                }
            } else if col_ty.is_decimal() {
                Ok(Value::Decimal(coerce_decimal(
                    Decimal::from_i64(n),
                    typmod,
                )?))
            } else {
                Err(type_error(format!(
                    "cannot store an integer value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Decimal(d) => {
            if col_ty.is_decimal() {
                Ok(Value::Decimal(coerce_decimal(d, typmod)?))
            } else {
                Err(type_error(format!(
                    "cannot store a decimal value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Text(s) => {
            if col_ty.is_text() {
                // A `varchar(n)` column enforces its length on store (assignment semantics):
                // over-length traps 22001, unless the excess is all spaces (truncate) — §15.
                Ok(Value::Text(coerce_varchar_store(s, varchar_len, col_name)?))
            } else if col_ty.is_bytea() {
                // A string literal adapts to a bytea column, decoding the hex input form
                // (types.md §6/§13); malformed hex traps 22P02.
                Ok(Value::Bytea(decode_bytea_literal(&s)?))
            } else if col_ty.is_uuid() {
                // A string literal adapts to a uuid column via the PG-flexible input
                // (types.md §6/§14); malformed input traps 22P02.
                Ok(Value::Uuid(decode_uuid_literal(&s)?))
            } else if col_ty.is_timestamp() {
                // A string literal adapts to a timestamp column (spec/design/timestamp.md);
                // malformed input traps 22007, an out-of-range field 22008.
                Ok(Value::Timestamp(parse_timestamp(&s)?))
            } else if col_ty.is_timestamptz() {
                Ok(Value::Timestamptz(parse_timestamptz(&s)?))
            } else if col_ty.is_interval() {
                // A string literal adapts to an interval column (spec/design/interval.md);
                // malformed input traps 22007, an out-of-range field 22008.
                Ok(Value::Interval(parse_interval(&s)?))
            } else if col_ty.is_date() {
                // A string literal adapts to a date column (spec/design/date.md); malformed
                // input traps 22007, an out-of-range field 22008.
                Ok(Value::Date(parse_date(&s)?))
            } else if col_ty.is_json() {
                // A string literal adapts to a json column (spec/design/json.md §4): validate,
                // store verbatim; malformed → 22P02.
                json::validate_json(&s)?;
                Ok(Value::Json(s))
            } else if col_ty.is_jsonb() {
                // A string literal adapts to a jsonb column (§2): parse + canonicalize; → 22P02.
                Ok(Value::Jsonb(json::jsonb_in(&s)?))
            } else {
                Err(type_error(format!(
                    "cannot store a text value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Bytea(b) => {
            if col_ty.is_bytea() {
                Ok(Value::Bytea(b))
            } else {
                Err(type_error(format!(
                    "cannot store a bytea value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Uuid(u) => {
            if col_ty.is_uuid() {
                Ok(Value::Uuid(u))
            } else {
                Err(type_error(format!(
                    "cannot store a uuid value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Timestamp(m) => {
            if col_ty.is_timestamp() {
                Ok(Value::Timestamp(m))
            } else {
                Err(type_error(format!(
                    "cannot store a timestamp value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Timestamptz(m) => {
            if col_ty.is_timestamptz() {
                Ok(Value::Timestamptz(m))
            } else {
                Err(type_error(format!(
                    "cannot store a timestamptz value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Date(d) => {
            if col_ty.is_date() {
                Ok(Value::Date(d))
            } else {
                Err(type_error(format!(
                    "cannot store a date value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Interval(iv) => {
            if col_ty.is_interval() {
                Ok(Value::Interval(iv))
            } else {
                Err(type_error(format!(
                    "cannot store an interval value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Bool(b) => {
            if col_ty.is_bool() {
                Ok(Value::Bool(b))
            } else {
                Err(type_error(format!(
                    "cannot store a boolean value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        // A f32 stores into a f32 column verbatim, or WIDENS losslessly into a f64
        // column (the implicit f32 → f64 cast, spec/types/casts.toml). Other targets 42804.
        Value::Float32(f) => {
            if col_ty.is_float32() {
                Ok(Value::Float32(f))
            } else if col_ty.is_float64() {
                Ok(Value::Float64(f as f64))
            } else {
                Err(type_error(format!(
                    "cannot store a f32 value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        // A f64 stores into a f64 column verbatim. f64 → f32 is an EXPLICIT cast
        // (lossy), so it never reaches store_value as an implicit assignment — any other target 42804.
        Value::Float64(f) => {
            if col_ty.is_float64() {
                Ok(Value::Float64(f))
            } else {
                Err(type_error(format!(
                    "cannot store a f64 value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        // A composite value into a scalar column is a type mismatch (a composite column routes
        // through `coerce_for_store`/`store_composite`, never the scalar `store_value` — composite.md §4).
        Value::Composite(_) => Err(type_error(format!(
            "cannot store a record value in {} column {col_name}",
            col_ty.canonical_name()
        ))),
        Value::Array(_) => Err(type_error(format!(
            "cannot store an array value in {} column {col_name}",
            col_ty.canonical_name()
        ))),
        // Range columns are not storable yet (R2); this scalar-store path is reached only for a
        // scalar column, so a range value here is a 42804 type mismatch (never a stored range).
        Value::Range(_) => Err(type_error(format!(
            "cannot store a range value in {} column {col_name}",
            col_ty.canonical_name()
        ))),
        Value::JsonPath(_) => Err(type_error(format!(
            "cannot store a jsonpath value in {} column {col_name}",
            col_ty.canonical_name()
        ))),
        // A json/jsonb value stores into a json/jsonb column verbatim (J1); any other target is a
        // 42804 type mismatch. In J0 no json/jsonb column exists, so this always errors.
        Value::Json(s) => {
            if col_ty.is_json() {
                Ok(Value::Json(s))
            } else {
                Err(type_error(format!(
                    "cannot store a json value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Jsonb(n) => {
            if col_ty.is_jsonb() {
                Ok(Value::Jsonb(n))
            } else {
                Err(type_error(format!(
                    "cannot store a jsonb value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        // Poisoned (large-values.md §14): a stored value is an evaluated expression result.
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    }
}

/// Coerce a value into a column for storage, handling **composite** columns (the recursive,
/// field-by-field coercion) as well as scalars (delegating to [`store_value`]). The column's
/// resolved [`ColType`] decides: a scalar column type-checks/range-checks the value as before; a
/// composite column requires a `Value::Composite` of matching arity, coercing each field to its
/// declared field type (recursing for nested composites) — spec/design/composite.md §4.
pub(crate) fn coerce_for_store(
    v: Value,
    ty: &ColType,
    typmod: Option<DecimalTypmod>,
    varchar_len: Option<u32>,
    not_null: bool,
    col_name: &str,
) -> Result<Value> {
    match ty {
        ColType::Scalar(s) => store_value(v, *s, typmod, varchar_len, not_null, col_name),
        ColType::Composite { name, fields } => store_composite(v, name, fields, not_null, col_name),
        ColType::Array(elem) => store_array(v, elem, not_null, col_name),
        ColType::Range(elem) => store_range(v, elem, not_null, col_name),
    }
}

/// Coerce a value into a **range** column (spec/design/ranges.md §4): NULL honours NOT NULL
/// (23502); a `Value::Range` is already canonical + element-typed by the resolver (the literal/cast
/// path canonicalized it), so each present bound is re-coerced to the element type as a belt-and-
/// suspenders identity (an unconstrained scalar coercion — no typmod, NULL-tolerant) and the value
/// passes through; any other value is a 42804.
pub(crate) fn store_range(
    v: Value,
    elem: &ColType,
    not_null: bool,
    col_name: &str,
) -> Result<Value> {
    match v {
        Value::Null => {
            if not_null {
                return Err(EngineError::not_null_violation(col_name));
            }
            Ok(Value::Null)
        }
        Value::Range(rv) => {
            if rv.empty {
                return Ok(Value::Range(rv));
            }
            // Coerce each finite bound to the element type (identity for an already-typed bound;
            // an infinite bound is None and skipped). Bounds are never NULL here — a None bound is
            // infinite, not NULL — so the element store is never NOT NULL.
            let coerce = |b: Option<Box<Value>>| -> Result<Option<Box<Value>>> {
                match b {
                    None => Ok(None),
                    Some(val) => Ok(Some(Box::new(coerce_for_store(
                        *val, elem, None, None, false, col_name,
                    )?))),
                }
            };
            Ok(Value::Range(RangeVal {
                empty: false,
                lower: coerce(rv.lower)?,
                upper: coerce(rv.upper)?,
                lower_inc: rv.lower_inc,
                upper_inc: rv.upper_inc,
            }))
        }
        _ => Err(type_error(format!(
            "cannot store a non-range value in range column {col_name}"
        ))),
    }
}

/// Coerce a value into an **array** column (spec/design/array.md §4): NULL honours NOT NULL
/// (23502); a `Value::Array` coerces each element to the declared element type via
/// [`coerce_for_store`] (a NULL element is allowed — array elements are nullable, so the element
/// store is never NOT NULL); any other value is a 42804.
pub(crate) fn store_array(
    v: Value,
    elem: &ColType,
    not_null: bool,
    col_name: &str,
) -> Result<Value> {
    match v {
        Value::Null => {
            if not_null {
                return Err(EngineError::not_null_violation(col_name));
            }
            Ok(Value::Null)
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.elements.len());
            for val in arr.elements {
                // Elements are nullable (not_null = false); the element typmod is unconstrained
                // this slice (numeric(p,s)[] and varchar(n)[] are deferred — §12, types.md §15).
                out.push(coerce_for_store(val, elem, None, None, false, col_name)?);
            }
            Ok(Value::Array(ArrayVal {
                dims: arr.dims,
                lbounds: arr.lbounds,
                elements: out,
            }))
        }
        _ => Err(type_error(format!(
            "cannot store a non-array value in array column {col_name}"
        ))),
    }
}

/// Coerce a value into a **composite** column (spec/design/composite.md §4): NULL honours NOT NULL
/// (23502); a `Value::Composite` must have exactly the declared field count (42804) and each field
/// is coerced to its declared field type via [`coerce_for_store`] (recursing); any other value is a
/// 42804. A NULL field of a NOT NULL composite field traps 23502.
pub(crate) fn store_composite(
    v: Value,
    type_name: &str,
    fields: &[ColField],
    not_null: bool,
    col_name: &str,
) -> Result<Value> {
    match v {
        Value::Null => {
            if not_null {
                return Err(EngineError::not_null_violation(col_name));
            }
            Ok(Value::Null)
        }
        Value::Composite(vals) => {
            if vals.len() != fields.len() {
                return Err(type_error(format!(
                    "row has {} fields but composite type {type_name} has {}",
                    vals.len(),
                    fields.len()
                )));
            }
            let mut out = Vec::with_capacity(vals.len());
            for (val, f) in vals.into_iter().zip(fields.iter()) {
                out.push(coerce_for_store(
                    val,
                    &f.ty,
                    f.typmod,
                    f.varchar_len,
                    f.not_null,
                    &f.name,
                )?);
            }
            Ok(Value::Composite(out))
        }
        _ => Err(type_error(format!(
            "cannot store a non-record value in composite column {col_name} (type {type_name})"
        ))),
    }
}

/// Coerce a decimal into a column's typmod: round to the declared scale and precision-check
/// (22003) for `numeric(p,s)`; for an unconstrained `numeric` column just cap-check
/// (spec/design/decimal.md §2).
pub(crate) fn coerce_decimal(d: Decimal, typmod: Option<DecimalTypmod>) -> Result<Decimal> {
    match typmod {
        Some(t) => d.coerce_to_typmod(t.precision as u32, t.scale as u32),
        None => d.check_cap(),
    }
}

/// Truncate a text value to at most `n` code points (the explicit `varchar(n)` cast rule —
/// spec/design/types.md §15). Cuts on a code-point boundary, never mid-byte; a string already
/// within `n` is returned unchanged.
pub(crate) fn truncate_to_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

/// Coerce a text value into a `varchar(n)` column/field for STORAGE (the assignment rule —
/// spec/design/types.md §15): a value longer than `n` code points traps `22001`, UNLESS every
/// excess code point is a space (U+0020), in which case it is silently truncated to `n` (the
/// SQL-standard trailing-space exception PostgreSQL implements). `varchar_len` of `None` (an
/// unbounded `text` column) passes the value through unchanged.
pub(crate) fn coerce_varchar_store(
    s: String,
    varchar_len: Option<u32>,
    col_name: &str,
) -> Result<String> {
    let Some(n) = varchar_len else {
        return Ok(s);
    };
    let n = n as usize;
    // Find the byte offset of the (n+1)-th code point, if any; if there is none the value
    // fits within `n` and is stored verbatim.
    let Some((cut, _)) = s.char_indices().nth(n) else {
        return Ok(s);
    };
    if s[cut..].chars().all(|c| c == ' ') {
        Ok(s[..cut].to_string())
    } else {
        Err(EngineError::new(
            SqlState::StringDataRightTruncation,
            format!("value too long for type varchar({n}) in column {col_name}"),
        )
        .with_data_type(format!("varchar({n})"))
        .with_column(col_name))
    }
}

/// Wrap a parsed literal as a runtime value (the type-check/coercion is `store_value`).
pub(crate) fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Int(n) => Value::Int(*n),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Decimal(d) => Value::Decimal(d.clone()),
    }
}

/// Wrap a literal as a runtime value for a given target column type — like [`literal_to_value`],
/// but an integer or decimal literal ADAPTS to a float column (decimal/int → float at the column's
/// width, nearest, round-ties-to-even — spec/design/float.md §4), so `INSERT INTO t(f) VALUES (1.5)`
/// and a `DEFAULT 1.5` on a float column land as floats. An out-of-range magnitude traps 22003 at
/// resolve. Every other literal/target pair falls through unchanged (store_value then type-checks).
pub(crate) fn literal_to_value_for(lit: &Literal, col_ty: ScalarType) -> Result<Value> {
    if col_ty.is_float() {
        match lit {
            Literal::Int(n) => return Ok(int_to_float(*n, col_ty)),
            Literal::Decimal(d) => return decimal_to_float(d, col_ty),
            _ => {}
        }
    }
    Ok(literal_to_value(lit))
}

/// Materialize one INSERT VALUES slot into a `Value` against the column's resolved `ColType`
/// (spec/design/composite.md §1/§4): a scalar slot is a literal (adapted to the type) or a bound
/// `$N`; a composite slot is a `ROW(…)` whose fields recurse against the composite's field types,
/// or a bound `$N`. The result is then fully coerced/range-checked by `coerce_for_store`. `DEFAULT`
/// is handled by the caller at the top level (it is not a valid field inside a `ROW(…)`).
pub(crate) fn materialize_insert_value(
    iv: &InsertValue,
    ty: &ColType,
    bound: &[Value],
) -> Result<Value> {
    match ty {
        ColType::Scalar(s) => match iv {
            InsertValue::Lit(lit) => literal_to_value_for(lit, *s),
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            InsertValue::Row(_) => Err(type_error(format!(
                "cannot assign a record value to a {} field",
                s.canonical_name()
            ))),
            InsertValue::Array(_) => Err(type_error(format!(
                "cannot assign an array value to a {} field",
                s.canonical_name()
            ))),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ROW(...)",
            )),
        },
        ColType::Composite { name, fields } => match iv {
            InsertValue::Row(field_ivs) => {
                if field_ivs.len() != fields.len() {
                    return Err(type_error(format!(
                        "ROW has {} fields but composite type {name} has {}",
                        field_ivs.len(),
                        fields.len()
                    )));
                }
                let mut vals = Vec::with_capacity(fields.len());
                for (fiv, f) in field_ivs.iter().zip(fields.iter()) {
                    vals.push(materialize_insert_value(fiv, &f.ty, bound)?);
                }
                Ok(Value::Composite(vals))
            }
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            InsertValue::Lit(_) => Err(type_error(format!(
                "cannot assign a scalar value to composite column (type {name})"
            ))),
            InsertValue::Array(_) => Err(type_error(format!(
                "cannot assign an array value to composite column (type {name})"
            ))),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ROW(...)",
            )),
        },
        ColType::Array(elem) => match iv {
            // ARRAY[e, …]: a nested constructor (an element is itself `ARRAY[…]`) stacks the
            // sub-arrays into a higher dimension (mirrors the evaluator's `build_nested_array`,
            // spec/design/array.md §4); otherwise each element materializes against the element type
            // into a flat 1-D array. A scalar mixed with an array sub-element errors 42804 (the
            // scalar materialized against the array type), matching PG.
            InsertValue::Array(elem_ivs) => {
                if elem_ivs.iter().any(|e| matches!(e, InsertValue::Array(_))) {
                    let mut subs = Vec::with_capacity(elem_ivs.len());
                    for eiv in elem_ivs {
                        subs.push(materialize_insert_value(eiv, ty, bound)?);
                    }
                    build_nested_array(subs)
                } else {
                    let mut vals = Vec::with_capacity(elem_ivs.len());
                    for eiv in elem_ivs {
                        vals.push(materialize_insert_value(eiv, elem, bound)?);
                    }
                    Ok(Value::Array(ArrayVal::one_dim(vals)))
                }
            }
            // A bare string literal adapts to the array context via `array_in` (the same
            // string-adapts-to-context rule bytea/uuid use — types.md §6; spec/design/array.md §7).
            InsertValue::Lit(Literal::Text(s)) => coerce_string_to_array(s, elem),
            InsertValue::Lit(Literal::Null) => Ok(Value::Null),
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            InsertValue::Lit(_) => Err(type_error(
                "cannot assign a scalar value to an array column".to_string(),
            )),
            InsertValue::Row(_) => Err(type_error(
                "cannot assign a record value to an array column".to_string(),
            )),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ARRAY[...]",
            )),
        },
        ColType::Range(elem) => {
            // A range column's element is always a scalar; the descriptor (for canonicalization)
            // is re-derived from it (spec/design/ranges.md §3/§4).
            let ColType::Scalar(es) = elem.as_ref() else {
                unreachable!("a range element is always a scalar (ranges.md §2)")
            };
            let desc = crate::range::range_for_element(*es)
                .expect("a range column's element always has a range type");
            match iv {
                // A bare string literal adapts to the range context via `range_in` (the same
                // string-adapts-to-context rule array/bytea/uuid use — spec/design/ranges.md §5).
                InsertValue::Lit(Literal::Text(s)) => {
                    Ok(Value::Range(coerce_string_to_range(s, desc)?))
                }
                InsertValue::Lit(Literal::Null) => Ok(Value::Null),
                InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
                InsertValue::Lit(_) => Err(type_error(
                    "cannot assign a scalar value to a range column".to_string(),
                )),
                InsertValue::Array(_) => Err(type_error(
                    "cannot assign an array value to a range column".to_string(),
                )),
                InsertValue::Row(_) => Err(type_error(
                    "cannot assign a record value to a range column".to_string(),
                )),
                InsertValue::Default => Err(EngineError::new(
                    SqlState::SyntaxError,
                    "DEFAULT is not allowed inside ROW(...)",
                )),
            }
        }
    }
}

/// Parse a text array literal into a `Value::Array` against the element `ColType` via `array_in`
/// (spec/design/array.md §7): each token is coerced to the element type (an unquoted `NULL` token
/// → NULL element). A malformed literal is `22P02`. Used by INSERT (a bare string adapting to an
/// array column) and by the runtime string-literal → array cast.
/// Cast one **non-null array element** value to the target element scalar — the eval kernel of the
/// element-wise `array → other-element-array` cast (spec/design/array.md §7). It runs the *same*
/// per-value conversions the scalar `RExpr::Cast` node does, for the pairs [`scalar_pair_castable`]
/// admits at resolve (numeric↔numeric, text→numeric/bool/uuid, bool↔i32, uuid↔text/bytea,
/// bytea→uuid); an array element has no `numeric(p,s)` typmod (an array type takes no modifier), so
/// every decimal target is the unconstrained form. Overflow traps `22003`, a malformed text element
/// `22P02` — per element, exactly like the scalar cast. The resolver gate guarantees only the
/// admitted `(source, target)` pairs reach here, so the value/target combinations are exhaustive.
pub(crate) fn cast_array_element(v: Value, target: ScalarType) -> Result<Value> {
    match v {
        Value::Int(n) => {
            if target.is_bool() {
                Ok(Value::Bool(n != 0))
            } else if target.is_decimal() {
                Ok(Value::Decimal(Decimal::from_i64(n)))
            } else if target.is_float() {
                Ok(int_to_float(n, target))
            } else if target.in_range(n) {
                Ok(Value::Int(n))
            } else {
                Err(overflow(target))
            }
        }
        Value::Decimal(d) => {
            if target.is_decimal() {
                Ok(Value::Decimal(d))
            } else if target.is_float() {
                decimal_to_float(&d, target)
            } else {
                let v = d.to_i64_round().ok_or_else(|| overflow(target))?;
                if target.in_range(v) {
                    Ok(Value::Int(v))
                } else {
                    Err(overflow(target))
                }
            }
        }
        Value::Float32(f) => cast_from_float(f as f64, target, None),
        Value::Float64(f) => cast_from_float(f, target, None),
        Value::Bool(b) => {
            if target.is_bool() {
                Ok(Value::Bool(b))
            } else {
                Ok(Value::Int(i64::from(b)))
            }
        }
        Value::Text(s) if target.is_uuid() => Ok(Value::Uuid(decode_uuid_literal(&s)?)),
        Value::Text(s) if target.is_bool() => Ok(Value::Bool(parse_bool_literal(&s)?)),
        Value::Text(s) if target.is_decimal() => Ok(Value::Decimal(parse_decimal_literal(&s)?)),
        Value::Text(s) if target == ScalarType::Float32 => {
            Ok(Value::Float32(parse_f32_literal(&s)?))
        }
        Value::Text(s) if target.is_float() => Ok(Value::Float64(parse_f64_literal(&s)?)),
        Value::Text(s) => Ok(Value::Int(parse_int_literal(&s, target)?)),
        Value::Uuid(u) if target.is_text() => Ok(Value::Text(render_uuid(&u))),
        Value::Uuid(u) if target.is_bytea() => Ok(Value::Bytea(u.to_vec())),
        Value::Bytea(b) if target.is_uuid() => {
            let len = b.len();
            let arr: [u8; 16] = b.try_into().map_err(|_| {
                EngineError::new(
                    SqlState::InvalidTextRepresentation,
                    format!("invalid length for type uuid: {len} bytes (expected 16)"),
                )
            })?;
            Ok(Value::Uuid(arr))
        }
        _ => unreachable!("resolver admits only the scalar_pair_castable element pairs (§7)"),
    }
}

/// Whether jed admits an element-wise `array → other-element-array` cast from source element scalar
/// `from` to target element scalar `to` (spec/design/array.md §7). Mirrors the scalar cast matrix
/// (spec/types/casts.toml) for the pairs an array element can take: numeric↔numeric, text→numeric/
/// boolean/uuid, boolean⇄i32, uuid⇄text, uuid⇄bytea. The identity (`from == to`) is handled by the
/// caller (it needs no cast node). A pair outside this set is rejected `0A000` at resolve.
pub(crate) fn scalar_pair_castable(from: ScalarType, to: ScalarType) -> bool {
    let numeric = |t: ScalarType| t.is_integer() || t.is_decimal() || t.is_float();
    if numeric(from) && numeric(to) {
        return true; // numeric ↔ numeric (int/decimal/float in any combination)
    }
    if from.is_text() && (numeric(to) || to.is_bool() || to.is_uuid()) {
        return true; // text → numeric / boolean / uuid (the runtime text + uuid cast slices)
    }
    if from.is_bool() && to == ScalarType::Int32 {
        return true; // boolean → i32 (the boolean cast slice — i32 only)
    }
    if from == ScalarType::Int32 && to.is_bool() {
        return true; // i32 → boolean
    }
    if from.is_uuid() && (to.is_text() || to.is_bytea()) {
        return true; // uuid → text / bytea (the uuid cast slice)
    }
    if from.is_bytea() && to.is_uuid() {
        return true; // bytea → uuid (the jed-only uuid cast)
    }
    false
}

pub(crate) fn coerce_string_to_array(s: &str, elem: &ColType) -> Result<Value> {
    let parsed = crate::value::parse_array_literal(s).map_err(|e| match e {
        crate::value::ArrayInError::Malformed => EngineError::new(
            SqlState::InvalidTextRepresentation,
            "malformed array literal".to_string(),
        ),
        // An inverted [l:u] bound (`u < l`) — PG `2202E`.
        crate::value::ArrayInError::BoundFlip => {
            array_subscript_err("upper bound cannot be less than lower bound")
        }
    })?;
    let mut elements = Vec::with_capacity(parsed.tokens.len());
    for tok in parsed.tokens {
        match tok {
            None => elements.push(Value::Null),
            // Coerce the token to the element type (a scalar via the string-literal coercion, a
            // composite via record_in — array-of-composite, spec/design/array.md §12 AC1).
            Some(t) => elements.push(coerce_array_element_text(&t, elem)?),
        }
    }
    Ok(Value::Array(ArrayVal {
        dims: parsed.dims,
        lbounds: parsed.lbounds,
        elements,
    }))
}

/// Coerce one array-element text token to a `Value` against the element `ColType` (the `array_in`
/// per-element step, spec/design/array.md §7): a scalar via the same string-literal coercion the
/// scalar typed-literal path uses; a **composite** element via `record_in` (recursive — the
/// array-of-composite quoting nests, §12 AC1 / §7). Self-contained over the resolved `ColType`, so
/// no catalog re-walk (the [`ColType`] design intent). A nested-array element token would recurse,
/// but array-of-array is not a jed type, so it is unreachable in v1.
pub(crate) fn coerce_array_element_text(tok: &str, elem: &ColType) -> Result<Value> {
    match elem {
        ColType::Scalar(s) => {
            let (node, _) = coerce_string_literal(tok, *s, None, None)?;
            rexpr_const_to_value(&node)
        }
        ColType::Composite { name, fields } => coerce_record_text(tok, name, fields),
        ColType::Array(inner) => coerce_string_to_array(tok, inner),
        // A range element token is unreachable: array-of-range is not a storable jed type (R2),
        // so an array element ColType is never a range.
        ColType::Range(_) => {
            unreachable!("array-of-range is not a storable type (ranges.md §2)")
        }
    }
}

/// `record_in` over a self-contained composite `ColType` (the inverse of `record_out`): the token is
/// the composite's own `(f1,f2,…)` text, tokenized by the shared `value::parse_record_tokens` and
/// recursively coerced per field. Mirrors [`coerce_string_to_composite`] but produces a `Value`
/// directly and walks `ColType` (so it needs no `Engine`). A bad shape / field count is `22P02`.
pub(crate) fn coerce_record_text(
    text: &str,
    name: &str,
    fields: &[crate::catalog::ColField],
) -> Result<Value> {
    let malformed = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("malformed record literal: \"{text}\" for type {name}"),
        )
    };
    let tokens = crate::value::parse_record_tokens(text).ok_or_else(malformed)?;
    if tokens.len() != fields.len() {
        return Err(malformed());
    }
    let mut vals = Vec::with_capacity(tokens.len());
    for (tok, f) in tokens.into_iter().zip(fields.iter()) {
        match tok {
            None => vals.push(Value::Null),
            Some(s) => vals.push(match &f.ty {
                ColType::Scalar(sc) => {
                    let (node, _) = coerce_string_literal(&s, *sc, f.typmod, f.varchar_len)?;
                    rexpr_const_to_value(&node)?
                }
                ColType::Composite {
                    name: n2,
                    fields: f2,
                } => coerce_record_text(&s, n2, f2)?,
                ColType::Array(inner) => coerce_string_to_array(&s, inner)?,
                // A composite range field is unreachable: CREATE TYPE rejects a range field (R2).
                ColType::Range(_) => {
                    unreachable!("a composite range field is rejected at CREATE TYPE (R2)")
                }
            }),
        }
    }
    Ok(Value::Composite(vals))
}

/// Extract the `Value` from a constant `RExpr` (the const nodes `coerce_string_literal` produces).
pub(crate) fn rexpr_const_to_value(node: &RExpr) -> Result<Value> {
    Ok(match node {
        RExpr::ConstNull => Value::Null,
        RExpr::ConstInt(n) => Value::Int(*n),
        RExpr::ConstBool(b) => Value::Bool(*b),
        RExpr::ConstText(s) => Value::Text(s.clone()),
        RExpr::ConstDecimal(d) => Value::Decimal(d.clone()),
        RExpr::ConstFloat32(f) => Value::Float32(*f),
        RExpr::ConstFloat64(f) => Value::Float64(*f),
        RExpr::ConstBytea(b) => Value::Bytea(b.clone()),
        RExpr::ConstUuid(u) => Value::Uuid(*u),
        RExpr::ConstTimestamp(m) => Value::Timestamp(*m),
        RExpr::ConstTimestamptz(m) => Value::Timestamptz(*m),
        RExpr::ConstDate(d) => Value::Date(*d),
        RExpr::ConstInterval(iv) => Value::Interval(*iv),
        RExpr::ConstJson(s) => Value::Json(s.clone()),
        RExpr::ConstJsonPath(s) => Value::JsonPath(s.clone()),
        RExpr::ConstJsonb(n) => Value::Jsonb((**n).clone()),
        _ => return Err(type_error("non-constant array element literal".to_string())),
    })
}

/// Evaluate a cross-family datetime cast (timezones.md §9.3) of the non-NULL value `v` to `to`
/// (`Timestamp`/`Timestamptz`/`Date`). The casts crossing the `timestamptz` boundary consult the
/// session zone (charging `timezone`); the others are zone-free. `±infinity` maps to the target's
/// own sentinel. The `(source family, to)` pair is guaranteed cross-family by the resolver.
pub(crate) fn eval_date_convert(
    v: Value,
    to: ScalarType,
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Value> {
    use crate::timestamp::{NEG_INFINITY as TS_NEG, POS_INFINITY as TS_POS};
    const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;
    // Map a finite/infinite timestamp-micros to a date (days), preserving the ±inf sentinels.
    let micros_to_date = |mc: i64| -> Value {
        if mc == TS_POS {
            Value::Date(crate::date::POS_INFINITY)
        } else if mc == TS_NEG {
            Value::Date(crate::date::NEG_INFINITY)
        } else {
            Value::Date(mc.div_euclid(MICROS_PER_DAY) as i32)
        }
    };
    // Midnight-of-a-date as timestamp-micros, preserving the ±inf sentinels.
    let date_to_micros = |d: i32| -> i64 {
        if d == crate::date::POS_INFINITY {
            TS_POS
        } else if d == crate::date::NEG_INFINITY {
            TS_NEG
        } else {
            d as i64 * MICROS_PER_DAY
        }
    };
    let is_inf = |mc: i64| mc == TS_POS || mc == TS_NEG;
    match (v, to) {
        // timestamp -> date (zone-free): the date part.
        (Value::Timestamp(mc), ScalarType::Date) => Ok(micros_to_date(mc)),
        // date -> timestamp (zone-free): midnight.
        (Value::Date(d), ScalarType::Timestamp) => Ok(Value::Timestamp(date_to_micros(d))),
        // timestamptz -> timestamp: render the instant in the session zone.
        (Value::Timestamptz(mc), ScalarType::Timestamp) => {
            if is_inf(mc) {
                return Ok(Value::Timestamp(mc));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            Ok(Value::Timestamp(crate::timezone::instant_to_local_micros(
                &zr, mc,
            )))
        }
        // timestamp -> timestamptz: interpret the wall clock in the session zone.
        (Value::Timestamp(mc), ScalarType::Timestamptz) => {
            if is_inf(mc) {
                return Ok(Value::Timestamptz(mc));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            Ok(Value::Timestamptz(
                crate::timezone::local_to_instant_micros(&zr, mc),
            ))
        }
        // timestamptz -> date: the date of the session-zone wall clock.
        (Value::Timestamptz(mc), ScalarType::Date) => {
            if is_inf(mc) {
                return Ok(micros_to_date(mc));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            Ok(micros_to_date(crate::timezone::instant_to_local_micros(
                &zr, mc,
            )))
        }
        // date -> timestamptz: midnight in the session zone -> the instant.
        (Value::Date(d), ScalarType::Timestamptz) => {
            let mid = date_to_micros(d);
            if is_inf(mid) {
                return Ok(Value::Timestamptz(mid));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            Ok(Value::Timestamptz(
                crate::timezone::local_to_instant_micros(&zr, mid),
            ))
        }
        _ => unreachable!("resolver restricts DateConvert to cross-family datetime casts"),
    }
}

/// Midnight (00:00:00) of a `date` as timestamp microseconds, preserving the ±infinity sentinels.
/// A finite date whose midnight instant overflows the i64-µs timestamp range traps `22008` (jed's
/// date range is wider than the timestamp range — date.md §1). A finite day count cannot land on a
/// timestamp sentinel (`i64::MIN`/`MAX` are not multiples of a day's micros), so no sentinel-
/// collision check is needed here; `ts_shift` re-checks the shifted result anyway.
pub(crate) fn date_midnight_micros(d: i32) -> Result<i64> {
    use crate::timestamp::{NEG_INFINITY as TS_NEG, POS_INFINITY as TS_POS};
    const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;
    if d == crate::date::POS_INFINITY {
        return Ok(TS_POS);
    }
    if d == crate::date::NEG_INFINITY {
        return Ok(TS_NEG);
    }
    i64::from(d)
        .checked_mul(MICROS_PER_DAY)
        .ok_or_else(|| EngineError::new(SqlState::DatetimeFieldOverflow, "date out of range"))
}

/// Evaluate a `date` arithmetic node (spec/design/date.md §6): `date ± int → date` (shift the i32
/// day count; ±infinity is returned unchanged; a finite result beyond the i32 day range or onto a
/// reserved sentinel traps `22008`), `date − date → i32` (days between; an ±infinity operand traps
/// `22008`, "cannot subtract infinite dates"; a difference beyond i32 traps `22008`), and
/// `date ± interval → timestamp` (the date widens to midnight, then the timestamp ± interval
/// calendar shift). The resolver guarantees a Date operand is present and settled `result`.
pub(crate) fn eval_date_arith(
    op: ArithOp,
    a: Value,
    b: Value,
    result: ScalarType,
) -> Result<Value> {
    use crate::date::{NEG_INFINITY as D_NEG, POS_INFINITY as D_POS};
    let dt_oflow = |msg: &'static str| EngineError::new(SqlState::DatetimeFieldOverflow, msg);

    // date ± interval → timestamp: widen the date to midnight micros, then the calendar shift.
    if matches!(result, ScalarType::Timestamp) {
        let (d, iv) = match (a, b) {
            (Value::Date(d), Value::Interval(iv)) | (Value::Interval(iv), Value::Date(d)) => {
                (d, iv)
            }
            _ => unreachable!("resolver guarantees a date ± interval pair"),
        };
        let mid = date_midnight_micros(d)?;
        let r = crate::interval::ts_shift(mid, &iv, matches!(op, ArithOp::Sub))?;
        return Ok(Value::Timestamp(r));
    }

    // date − date → i32 (days between); an ±infinity operand traps 22008.
    if let (Value::Date(x), Value::Date(y)) = (&a, &b) {
        if *x == D_NEG || *x == D_POS || *y == D_NEG || *y == D_POS {
            return Err(dt_oflow("cannot subtract infinite dates"));
        }
        let diff = i64::from(*x) - i64::from(*y);
        let diff = i32::try_from(diff).map_err(|_| dt_oflow("date out of range"))?;
        return Ok(Value::Int(i64::from(diff)));
    }

    // date ± int → date: shift the day count; a ±infinity date stays the same sentinel.
    let (d, n) = match (a, b) {
        (Value::Date(d), Value::Int(n)) | (Value::Int(n), Value::Date(d)) => (d, n),
        _ => unreachable!("resolver guarantees a date ± int pair"),
    };
    if d == D_NEG || d == D_POS {
        return Ok(Value::Date(d));
    }
    let shifted = if matches!(op, ArithOp::Sub) {
        i64::from(d).checked_sub(n)
    } else {
        i64::from(d).checked_add(n)
    }
    .ok_or_else(|| dt_oflow("date out of range"))?;
    let days = i32::try_from(shifted).map_err(|_| dt_oflow("date out of range"))?;
    // A finite result that lands on a reserved sentinel value is out of range.
    if days == D_NEG || days == D_POS {
        return Err(dt_oflow("date out of range"));
    }
    Ok(Value::Date(days))
}

/// The three-valued membership fold for `lhs op ANY/ALL(array)` (array-functions.md §11), the
/// generalization of `in_membership` to all five comparison operators and both quantifiers. A NULL
/// array → NULL; otherwise, over the flattened elements, `ANY`/`SOME` (all=false) is the OR-fold
/// (TRUE if any `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty → FALSE) and `ALL`
/// (all=true) is the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE; empty →
/// TRUE). Each element comparison charges one `operator_eval` (+ size-scaled `decimal_work`),
/// exactly like `in_membership`, so `max_cost` bounds the walk (54P01, CLAUDE.md §13).
pub(crate) fn quantified_membership(
    op: CmpOp,
    all: bool,
    lv: &Value,
    av: &Value,
    m: &mut Meter,
) -> Result<Value> {
    let arr = match av {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        _ => unreachable!("the resolver requires an array right operand"),
    };
    let mut any_null = false;
    for e in &arr.elements {
        m.charge(COSTS.operator_eval);
        m.charge(COSTS.decimal_work * ((decimal_cmp_work(lv, e) - 1) as i64));
        m.guard()?;
        match quantified_cmp3(op, lv, e) {
            ThreeValued::True => {
                // ANY short-circuits TRUE; ALL keeps going (TRUE is its neutral element).
                if !all {
                    return Ok(Value::Bool(true));
                }
            }
            ThreeValued::False => {
                // ALL short-circuits FALSE; ANY keeps going (FALSE is its neutral element).
                if all {
                    return Ok(Value::Bool(false));
                }
            }
            ThreeValued::Unknown => any_null = true,
        }
    }
    // Drained without a short-circuit: a NULL seen → UNKNOWN; else the quantifier's identity
    // (ALL → TRUE, ANY → FALSE — also the empty-array result).
    Ok(if any_null {
        Value::Null
    } else {
        Value::Bool(all)
    })
}

/// The per-element three-valued comparison `lhs op e` for a quantified node, normalizing a
/// mixed-width float pair to `f64` first (the resolver admits `f32` vs `f64`, matching
/// `RExpr::Compare`'s promote — here the array elements are runtime values, so the widen happens per
/// element). Bottoms out in the value module's `eq3`/`lt3`/`gt3` kernels.
///
/// A **composite** operand pair routes through the composite **total order** (`value_cmp`), NOT the
/// bare-`ROW` 3VL `eq3`/`lt3`/`gt3` (array-functions.md §13): PostgreSQL's `= ANY(addr[])` dispatches
/// on the composite `=` *operator* = `record_eq`, which is **definite with NULL fields comparable**
/// (`ROW('a',NULL)::addr = ANY(ARRAY[ROW('a',NULL)::addr])` is TRUE), the same total order
/// `array_eq` / `@>` already use for composite elements (array.md §5). A **whole-element NULL** is
/// still UNKNOWN — the operator stays strict at the value level — so the resolver-guaranteed
/// same-type pair is composite-vs-composite or composite-vs-NULL.
pub(crate) fn quantified_cmp3(op: CmpOp, x: &Value, e: &Value) -> ThreeValued {
    if matches!(x, Value::Composite(_)) || matches!(e, Value::Composite(_)) {
        // A whole-element NULL → UNKNOWN (3VL at the value level); else the definite total order.
        if matches!(x, Value::Null) || matches!(e, Value::Null) {
            return ThreeValued::Unknown;
        }
        let ord = value_cmp(x, e);
        let matched = match op {
            CmpOp::Eq => ord == std::cmp::Ordering::Equal,
            CmpOp::Ne => ord != std::cmp::Ordering::Equal,
            CmpOp::Lt => ord == std::cmp::Ordering::Less,
            CmpOp::Gt => ord == std::cmp::Ordering::Greater,
            CmpOp::Le => ord != std::cmp::Ordering::Greater,
            CmpOp::Ge => ord != std::cmp::Ordering::Less,
        };
        return if matched {
            ThreeValued::True
        } else {
            ThreeValued::False
        };
    }
    let (xw, ew);
    let (a, b): (&Value, &Value) = match (x, e) {
        (Value::Float32(v), Value::Float64(_)) => {
            xw = Value::Float64(*v as f64);
            (&xw, e)
        }
        (Value::Float64(_), Value::Float32(v)) => {
            ew = Value::Float64(*v as f64);
            (x, &ew)
        }
        _ => (x, e),
    };
    match op {
        CmpOp::Eq => a.eq3(b),
        CmpOp::Ne => a.eq3(b).not(),
        CmpOp::Lt => a.lt3(b),
        CmpOp::Gt => a.gt3(b),
        CmpOp::Le => a.lt3(b).or(a.eq3(b)),
        CmpOp::Ge => a.gt3(b).or(a.eq3(b)),
    }
}

/// The SQL `LIKE` matcher (spec/design/grammar.md §22): `%` matches any (possibly empty) run
/// of characters, `_` matches exactly one character, and `\` (the default escape) makes the
/// next pattern character literal. It iterates by Unicode **code point** (so astral characters
/// match `_` correctly — a CLAUDE.md §8 determinism surface), via a two-pointer greedy
/// backtracking walk identical across the cores. It returns `Err(22025)` when the escape
/// character is the **last** pattern character *reached during matching* (PostgreSQL's "LIKE
/// pattern must not end with escape character") — data-dependent, since an earlier mismatch
/// returns `false` before the escape is reached.
pub(crate) fn like_match(subject: &str, pattern: &str) -> Result<bool> {
    let s: Vec<char> = subject.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let mut si = 0usize;
    let mut pi = 0usize;
    // The last '%' position in the pattern (a backtrack point) and the subject index when it
    // was taken; `None` until a '%' has been seen.
    let mut star_pi: Option<usize> = None;
    let mut star_si = 0usize;
    while si < s.len() {
        if pi < p.len() && p[pi] == '\\' {
            // Escape: the next pattern character must match the subject literally.
            if pi + 1 >= p.len() {
                return Err(EngineError::new(
                    SqlState::InvalidEscapeSequence,
                    "LIKE pattern must not end with escape character",
                ));
            }
            if s[si] == p[pi + 1] {
                si += 1;
                pi += 2;
                continue;
            }
            // literal mismatch → fall through to backtrack
        } else if pi < p.len() && p[pi] == '_' {
            si += 1;
            pi += 1;
            continue;
        } else if pi < p.len() && p[pi] == '%' {
            star_pi = Some(pi);
            star_si = si;
            pi += 1;
            continue;
        } else if pi < p.len() && p[pi] == s[si] {
            si += 1;
            pi += 1;
            continue;
        }
        // Mismatch: backtrack to the last '%' (it absorbs one more subject character), else no.
        if let Some(sp) = star_pi {
            pi = sp + 1;
            star_si += 1;
            si = star_si;
            continue;
        }
        return Ok(false);
    }
    // Subject consumed: any pattern remainder must be all '%' to match.
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    Ok(pi == p.len())
}

/// Evaluate an integer arithmetic op in 64-bit, trapping 22012 on a zero divisor and
/// 22003 if the 64-bit op overflows OR the in-range result falls outside the declared
/// result type (the i16+i16 → i16 boundary — spec/design/functions.md §7).
pub(crate) fn eval_arith(op: ArithOp, x: i64, y: i64, result: ScalarType) -> Result<Value> {
    let computed = match op {
        ArithOp::Add => x.checked_add(y),
        ArithOp::Sub => x.checked_sub(y),
        ArithOp::Mul => x.checked_mul(y),
        ArithOp::Div => {
            if y == 0 {
                return Err(EngineError::new(
                    SqlState::DivisionByZero,
                    "division by zero",
                ));
            }
            x.checked_div(y)
        }
        ArithOp::Mod => {
            if y == 0 {
                return Err(EngineError::new(
                    SqlState::DivisionByZero,
                    "division by zero",
                ));
            }
            // `x % -1` is mathematically 0 for every x. Special-cased so i64::MIN % -1
            // returns 0 instead of trapping on the i64 IDIV overflow (which `checked_rem`
            // reports as None) — matching PostgreSQL and the i16/i32 widths, which
            // already compute 0 cleanly in 64-bit (spec/design/types.md §3).
            if y == -1 { Some(0) } else { x.checked_rem(y) }
        }
    };
    let v = computed.ok_or_else(|| overflow(result))?;
    if result.in_range(v) {
        Ok(Value::Int(v))
    } else {
        Err(overflow(result))
    }
}

/// Evaluate `f64 ⊕ f64` for one node (spec/design/float.md §5): the IEEE correctly-rounded
/// op (round-ties-to-even — Rust's default). The PG TRAP model: a FINITE pair whose result
/// overflows to ±Inf traps 22003 (finite arithmetic never PRODUCES non-finite values); `x / 0`
/// (or `x % 0`) traps 22012 for EVERY numerator except NaN (`Inf/0` and `0/0` trap; only `NaN/0`
/// propagates to NaN — matching PG). An operand already Inf/NaN otherwise PROPAGATES (no trap).
pub(crate) fn eval_float64_arith(op: ArithOp, x: f64, y: f64) -> Result<Value> {
    // Division/modulus by a zero divisor traps 22012 for every numerator EXCEPT NaN, which
    // propagates (NaN/0 = NaN, matching PG). `Inf/0` and `0/0` are genuine division by zero.
    if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0.0 && !x.is_nan() {
        return Err(EngineError::new(
            SqlState::DivisionByZero,
            "division by zero",
        ));
    }
    let r = match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y, // IEEE fmod (Rust `%` on f64 is fmod)
    };
    // Finite-overflow trap (§3): a result that became ±Inf from two FINITE operands overflowed.
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(overflow(ScalarType::Float64));
    }
    Ok(Value::Float64(r))
}

/// As [`eval_float64_arith`], at binary32 (`f32`). Each op rounds to binary32 (native `f32`
/// arithmetic), so a finite overflow to ±Inf at the f32 range traps 22003.
pub(crate) fn eval_float32_arith(op: ArithOp, x: f32, y: f32) -> Result<Value> {
    // Same zero-divisor rule as f64: traps for every numerator except NaN (Inf/0 traps).
    if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0.0 && !x.is_nan() {
        return Err(EngineError::new(
            SqlState::DivisionByZero,
            "division by zero",
        ));
    }
    let r = match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y,
    };
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(overflow(ScalarType::Float32));
    }
    Ok(Value::Float32(r))
}

/// Cast an integer to a float of `target` width (spec/design/float.md §6): nearest, round-ties-to-
/// even (Rust `as`), never overflows (i64::MAX < f32::MAX). `target` is a float type.
pub(crate) fn int_to_float(n: i64, target: ScalarType) -> Value {
    if target.is_float32() {
        Value::Float32(n as f32)
    } else {
        Value::Float64(n as f64)
    }
}

/// An integer literal adapted to a float context as a constant `RExpr` (spec/design/float.md §4),
/// at the context width.
pub(crate) fn int_to_const_float(n: i64, target: ScalarType) -> RExpr {
    if target.is_float32() {
        RExpr::ConstFloat32(n as f32)
    } else {
        RExpr::ConstFloat64(n as f64)
    }
}

/// Cast a decimal to a float of `target` width (spec/design/float.md §6): the nearest binary value
/// to the decimal's exact value. A magnitude that overflows the float range traps 22003 (not ±Inf
/// — the §3 finite rule; decimal is always finite, so the result can only be finite or trap).
pub(crate) fn decimal_to_float(d: &Decimal, target: ScalarType) -> Result<Value> {
    // The decimal's canonical string parses to the nearest binary value (Rust's float parser is
    // correctly rounded). A huge decimal parses to ±Inf, which is the overflow case.
    let s = d.render();
    if target.is_float32() {
        let f: f32 = s.parse().map_err(|_| overflow(ScalarType::Float32))?;
        if f.is_finite() {
            Ok(Value::Float32(f))
        } else {
            Err(overflow(ScalarType::Float32))
        }
    } else {
        let f: f64 = s.parse().map_err(|_| overflow(ScalarType::Float64))?;
        if f.is_finite() {
            Ok(Value::Float64(f))
        } else {
            Err(overflow(ScalarType::Float64))
        }
    }
}

/// Cast a float value (already widened to f64) to a non-float `target` — int / decimal — or to a
/// narrower float width (spec/design/float.md §6). NaN/±Inf → 22003 for every non-float target
/// (and for f64 → f32 overflow). Float → int rounds HALF AWAY FROM ZERO (jed's one mode)
/// then range-checks. Float → decimal is the exact decimal of the binary value, then the typmod.
pub(crate) fn cast_from_float(
    f: f64,
    target: ScalarType,
    typmod: Option<DecimalTypmod>,
) -> Result<Value> {
    if target.is_float64() {
        // float → f64: widening (lossless from f32, identity from f64).
        return Ok(Value::Float64(f));
    }
    if target.is_float32() {
        // f64 → f32: nearest (round-ties-to-even). A finite value beyond the binary32
        // range traps 22003 (not ±Inf); NaN/±Inf convert across widths unchanged (propagate).
        let n = f as f32;
        if n.is_infinite() && f.is_finite() {
            return Err(overflow(ScalarType::Float32));
        }
        return Ok(Value::Float32(n));
    }
    // Non-float targets reject NaN/±Inf (they have no finite representation).
    if !f.is_finite() {
        return Err(overflow(target));
    }
    if target.is_decimal() {
        // float → decimal: the EXACT decimal of the binary value (spec/design/float.md §6), then
        // the typmod's scale coercion. `f` is finite (checked above); a f32 reaches here
        // already losslessly widened to f64, so the exact decimal IS the binary32 value's.
        let d = Decimal::from_float64(f);
        return Ok(Value::Decimal(coerce_decimal(d, typmod)?));
    }
    // float → int: round HALF AWAY FROM ZERO, then range-check the target integer (22003).
    let rounded = f.round(); // Rust `f64::round` is round-half-away-from-zero
    if rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
        return Err(overflow(target));
    }
    let v = rounded as i64;
    if target.in_range(v) {
        Ok(Value::Int(v))
    } else {
        Err(overflow(target))
    }
}

/// Finalize a float SUM/AVG from the STREAMING scan-order running total (spec/design/float.md §7).
/// The fold order is ledgered non-deterministic (`float-sum-order`); the steps here are all
/// order-independent given the accumulated `total`:
/// 1. Special values FIRST: empty/all-NULL group → NULL; any NaN → NaN; both +Inf and -Inf → NaN;
///    else +Inf → +Inf; else -Inf → -Inf; else all-finite → step 2.
/// 2. The running `total` IS the sum (already width-correct — f32 was re-rounded each add). If it
///    reached ±Inf from finite inputs it is a finite-overflow 22003 (a final `is_infinite` test is
///    equivalent to a per-add one — finite + ±Inf cannot recover to finite).
///
/// AVG = SUM / count, ONE final rounding at the input width.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_float_fold(
    width: ScalarType,
    is_avg: bool,
    total: f64,
    count: i64,
    any_nan: bool,
    pos_inf: bool,
    neg_inf: bool,
) -> Result<Value> {
    let is_f32 = width.is_float32();
    let wrap = |f: f64| -> Value {
        if is_f32 {
            Value::Float32(f as f32)
        } else {
            Value::Float64(f)
        }
    };
    // Step 1 — empty group → NULL (no non-NULL inputs).
    if count == 0 {
        return Ok(Value::Null);
    }
    // Step 1 — special values, resolved before the finite sum (order-independent).
    if any_nan {
        return Ok(wrap(f64::NAN));
    }
    if pos_inf && neg_inf {
        return Ok(wrap(f64::NAN));
    }
    if pos_inf {
        return Ok(wrap(f64::INFINITY));
    }
    if neg_inf {
        return Ok(wrap(f64::NEG_INFINITY));
    }
    // Step 2 — the running total is the sum; f32 already re-rounded each add so `total as f32` is
    // exact. A ±Inf total is a finite-overflow 22003 (the §3 finite-overflow rule).
    let sum = if is_f32 {
        let acc = total as f32;
        if acc.is_infinite() {
            return Err(overflow(ScalarType::Float32));
        }
        acc as f64
    } else {
        if total.is_infinite() {
            return Err(overflow(ScalarType::Float64));
        }
        total
    };
    if !is_avg {
        return Ok(wrap(sum));
    }
    // AVG = SUM / count, one rounding at the input width.
    if is_f32 {
        let avg = (sum as f32) / (count as f32);
        Ok(Value::Float32(avg))
    } else {
        Ok(Value::Float64(sum / count as f64))
    }
}

/// `round(f64, places)` — round half away from zero to `places` decimal digits (the engine's
/// one mode — spec/design/float.md §8). A NaN/±Inf operand passes through. `places` may be
/// negative (round to the left of the point). Done by scaling by 10^places, `round()` (half-away),
/// then unscaling — the approximate float path (binary, so itself inexact, which the `R` tag
/// absorbs).
pub(crate) fn round_f64_places(f: f64, places: i64) -> f64 {
    if !f.is_finite() {
        return f;
    }
    if places == 0 {
        return f.round();
    }
    let scale = 10f64.powi(places as i32);
    if !scale.is_finite() || scale == 0.0 {
        // Extreme `places` — clamp to the operand (no observable rounding at that magnitude).
        return f;
    }
    (f * scale).round() / scale
}

/// Evaluate a float scalar function over a finite/non-finite f64 (spec/design/float.md §8). The
/// EXACT set (ceil/floor/trunc/sqrt) is correctly-rounded (in-contract); the TRANSCENDENTAL set
/// (exp/ln/log10/pow/sin/cos/tan) calls Rust's libm (exempted, may differ by an ULP cross-core).
/// Domain/overflow errors trap 22003, keeping NaN/Inf input-only (a NaN/Inf *operand* propagates).
pub(crate) fn eval_float_func(func: ScalarFunc, x: f64, arg2: Option<&Value>) -> Result<Value> {
    // PG's exact RADIANS_PER_DEGREE literal (float.c) — shared by radians/degrees so the single
    // IEEE multiply/divide is byte-identical cross-core and matches PG.
    const RADIANS_PER_DEGREE: f64 = 0.0174532925199432957692;
    let r = match func {
        ScalarFunc::Ceil => x.ceil(),
        ScalarFunc::Floor => x.floor(),
        ScalarFunc::Trunc => x.trunc(),
        ScalarFunc::Sqrt => {
            // sqrt of a NEGATIVE finite value is a domain error → 22003 (NaN stays input-only).
            // A NaN/±Inf operand propagates (sqrt(Inf) = Inf, sqrt(NaN) = NaN).
            if x.is_finite() && x < 0.0 {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "cannot take square root of a negative number",
                ));
            }
            x.sqrt()
        }
        ScalarFunc::Exp => {
            let v = x.exp();
            // exp overflow (e.g. exp(710)) → ±Inf from a finite operand traps 22003.
            if v.is_infinite() && x.is_finite() {
                return Err(overflow(ScalarType::Float64));
            }
            v
        }
        ScalarFunc::Ln => {
            // ln(0) → 22003; ln(neg) → 22003 (domain). NaN/Inf operands propagate.
            if x.is_finite() {
                if x == 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of zero",
                    ));
                }
                if x < 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of a negative number",
                    ));
                }
            }
            x.ln()
        }
        ScalarFunc::Log10 => {
            if x.is_finite() {
                if x == 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of zero",
                    ));
                }
                if x < 0.0 {
                    return Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "cannot take logarithm of a negative number",
                    ));
                }
            }
            x.log10()
        }
        ScalarFunc::Pow => {
            let y = match arg2 {
                Some(Value::Float64(y)) => *y,
                _ => unreachable!("pow's second arg is a widened f64"),
            };
            let v = x.powf(y);
            // pow overflow from finite operands → 22003.
            if v.is_infinite() && x.is_finite() && y.is_finite() {
                return Err(overflow(ScalarType::Float64));
            }
            v
        }
        ScalarFunc::Sin => x.sin(),
        ScalarFunc::Cos => x.cos(),
        ScalarFunc::Tan => x.tan(),
        // cbrt has no domain restriction: cbrt(-8) = -2, cbrt(±Inf) = ±Inf, cbrt(NaN) = NaN.
        ScalarFunc::Cbrt => x.cbrt(),
        // radians/degrees — a single correctly-rounded IEEE op (multiply/divide) by PG's exact
        // RADIANS_PER_DEGREE literal (float.c), so byte-identical cross-core (in-contract).
        ScalarFunc::Radians => x * RADIANS_PER_DEGREE,
        ScalarFunc::Degrees => x / RADIANS_PER_DEGREE,
        // asin domain is [-1, 1]: a finite |x| > 1 (and ±Inf, magnitude > 1) is out of range →
        // 22003, exactly PG; a NaN operand propagates (no trap).
        ScalarFunc::Asin => {
            if !x.is_nan() && (x < -1.0 || x > 1.0) {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.asin()
        }
        // acos shares asin's domain [-1, 1]: |x| > 1 (or ±Inf) → 22003, NaN propagates.
        ScalarFunc::Acos => {
            if !x.is_nan() && (x < -1.0 || x > 1.0) {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.acos()
        }
        // atan is defined on all of ℝ (no domain trap); atan(±Inf) = ±π/2, atan(NaN) = NaN.
        ScalarFunc::Atan => x.atan(),
        // atan2(y, x): y is the first operand (x here), the second (arg2) is the denominator.
        // Quadrant-aware; no domain trap. The resolver widened both operands to f64.
        ScalarFunc::Atan2 => {
            let x2 = match arg2 {
                Some(Value::Float64(v)) => *v,
                _ => unreachable!("atan2's second arg is a widened f64"),
            };
            x.atan2(x2)
        }
        // cot(x) = 1/tan(x) (no libm cot; 1/tan bit-matches PG). cot(0) = +Inf (no trap).
        ScalarFunc::Cot => 1.0 / x.tan(),
        // Hyperbolics: sinh/cosh overflow to ±Inf with NO trap (PG-faithful, unlike exp/pow);
        // tanh/asinh are total. acosh traps below 1; atanh traps outside [-1, 1] (atanh(±1) =
        // ±Inf is admissible). A NaN operand propagates through every one.
        ScalarFunc::Sinh => x.sinh(),
        ScalarFunc::Cosh => x.cosh(),
        ScalarFunc::Tanh => x.tanh(),
        ScalarFunc::Asinh => x.asinh(),
        ScalarFunc::Acosh => {
            if !x.is_nan() && x < 1.0 {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.acosh()
        }
        ScalarFunc::Atanh => {
            if !x.is_nan() && (x < -1.0 || x > 1.0) {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.atanh()
        }
        ScalarFunc::Abs
        | ScalarFunc::Round
        | ScalarFunc::Pi
        | ScalarFunc::Log
        | ScalarFunc::Sign
        | ScalarFunc::Div
        | ScalarFunc::Gcd
        | ScalarFunc::Lcm
        | ScalarFunc::Factorial
        | ScalarFunc::WidthBucket
        | ScalarFunc::Scale
        | ScalarFunc::MinScale
        | ScalarFunc::TrimScale
        | ScalarFunc::MakeInterval
        | ScalarFunc::MakeTimestamp
        | ScalarFunc::MakeTimestamptz
        | ScalarFunc::UuidExtractVersion
        | ScalarFunc::UuidExtractTimestamp
        | ScalarFunc::Uuidv4
        | ScalarFunc::Uuidv7
        | ScalarFunc::Now
        | ScalarFunc::ClockTimestamp
        | ScalarFunc::Nextval
        | ScalarFunc::Currval
        | ScalarFunc::Setval
        | ScalarFunc::Lastval
        | ScalarFunc::CurrentSetting
        | ScalarFunc::JsonbTypeof
        | ScalarFunc::JsonTypeof
        | ScalarFunc::JsonbArrayLength
        | ScalarFunc::JsonArrayLength
        | ScalarFunc::JsonbStripNulls
        | ScalarFunc::JsonStripNulls
        | ScalarFunc::JsonbPretty
        | ScalarFunc::ToJsonb
        | ScalarFunc::ToJson
        | ScalarFunc::JsonScalar
        | ScalarFunc::JsonSerialize
        | ScalarFunc::Length
        | ScalarFunc::OctetLength
        | ScalarFunc::BitLength
        | ScalarFunc::Substr
        | ScalarFunc::Left
        | ScalarFunc::Right
        | ScalarFunc::Lpad
        | ScalarFunc::Rpad
        | ScalarFunc::Btrim
        | ScalarFunc::Ltrim
        | ScalarFunc::Rtrim
        | ScalarFunc::Replace
        | ScalarFunc::Translate
        | ScalarFunc::Repeat
        | ScalarFunc::Reverse
        | ScalarFunc::Strpos
        | ScalarFunc::SplitPart
        | ScalarFunc::StartsWith
        | ScalarFunc::Ascii
        | ScalarFunc::Chr
        | ScalarFunc::Initcap
        | ScalarFunc::ToHex
        | ScalarFunc::Encode
        | ScalarFunc::Decode
        | ScalarFunc::QuoteLiteral
        | ScalarFunc::QuoteIdent
        | ScalarFunc::QuoteNullable => {
            unreachable!(
                "abs/round/make_interval/uuid_*/now/clock_timestamp/sequence/current_setting/json/string fns are handled before eval_float_func"
            )
        }
    };
    Ok(Value::Float64(r))
}

/// Widen a numeric value to `Decimal` (an integer operand of decimal arithmetic).
pub(crate) fn to_decimal(v: Value) -> Decimal {
    match v {
        Value::Decimal(d) => d,
        Value::Int(n) => Decimal::from_i64(n),
        _ => unreachable!("resolver guarantees a numeric operand here"),
    }
}

// === string / text function kernels (spec/design/string-functions.md) ===============

/// The character-count cap for the result-amplifying string functions (lpad / rpad / repeat):
/// PostgreSQL's `MaxAllocSize` (0x3FFFFFFF). A requested length above it traps `54000`
/// (program_limit_exceeded), bounding the allocation an untrusted query can request (CLAUDE.md §13).
pub(crate) const MAX_RESULT_CHARS: i64 = 0x3FFF_FFFF;

/// The i64 of an integer Value (every int width is stored as `Value::Int`). For the string
/// functions' integer arguments (start / count / pad length / field number).
pub(crate) fn int_value(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        _ => unreachable!("resolver guarantees an integer operand here"),
    }
}
