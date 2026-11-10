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
    pub(crate) fn check(&self, v: Value, iz: &InputZone) -> Result<Value> {
        match &self.col_type {
            Some(ct) => coerce_for_store(v, ct, self.typmod, self.not_null, &self.name, iz),
            None => store_value(v, self.target, self.typmod, self.not_null, &self.name, iz),
        }
    }
}

/// An `ON UPDATE CASCADE` value — of the referenced column's type — prepared for a referencing
/// column of another comparable type (constraints.md §6.6): PostgreSQL's assignment cast. A decimal
/// rounds half away from zero into an integer column and a `f64` rounds to nearest into a `f32`
/// column, each `22003` out of range; integers and `f32` values pass to [`store_value`], which
/// range-checks or widens them. A same-type value passes unchanged.
pub(crate) fn fk_cascade_value(v: Value, target: ScalarType) -> Result<Value> {
    match v {
        Value::Decimal(d) if target.is_integer() => match d.to_i64_round() {
            Some(n) if target.in_range(n) => Ok(Value::Int(n)),
            _ => Err(overflow(target)),
        },
        Value::Float64(f) if target.is_float32() => cast_from_float(f, target, None),
        v => Ok(v),
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
    tm: Typmod,
    not_null: bool,
    col_name: &str,
    iz: &InputZone,
) -> Result<Value> {
    let typmod = tm.decimal;
    let varchar_len = tm.varchar_len;
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
            } else if col_ty.is_float() {
                // An integer adapts to a float column or parameter (spec/design/float.md §4); an
                // INSERT ... SELECT integer value is gated out upstream (42804), so this adapts a
                // literal or a bound host value.
                Ok(int_to_float(n, col_ty))
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
            } else if col_ty.is_float() {
                // A decimal adapts to a float column or parameter at its width, ties-to-even; a
                // magnitude beyond the width traps 22003 (float.md §4). Same rule as the integer arm.
                decimal_to_float(&d, col_ty)
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
                // A string literal adapts to a bytea column, decoding its hex or escape
                // input form (types.md §6/§13); malformed input traps 22P02.
                Ok(Value::Bytea(decode_bytea_literal(&s)?))
            } else if col_ty.is_uuid() {
                // A string literal adapts to a uuid column via the PG-flexible input
                // (types.md §6/§14); malformed input traps 22P02.
                Ok(Value::Uuid(decode_uuid_literal(&s)?))
            } else if col_ty.is_timestamp() {
                // A string literal adapts to a timestamp column (spec/design/timestamp.md);
                // malformed input traps 22007, an out-of-range field 22008. A `timestamp(p)`
                // column rounds it (§8).
                Ok(Value::Timestamp(adjust_timestamp_typmod(
                    parse_timestamp(&s)?,
                    tm.temporal,
                )?))
            } else if col_ty.is_timestamptz() {
                // An offset-less string reads in the session zone (timezones.md §9.3).
                Ok(Value::Timestamptz(adjust_timestamp_typmod(
                    iz.parse_timestamptz(&s)?,
                    tm.temporal,
                )?))
            } else if col_ty.is_interval() {
                // A string literal adapts to an interval column (spec/design/interval.md);
                // malformed input traps 22007, an out-of-range field 22008. A qualified column
                // reads it under its qualifier and cuts it (§6.1).
                Ok(Value::Interval(interval::parse_interval_typmod(
                    &s,
                    tm.temporal,
                )?))
            } else if col_ty.is_date() {
                // A string literal adapts to a date column (spec/design/date.md); malformed
                // input traps 22007, an out-of-range field 22008.
                Ok(Value::Date(parse_date(&s)?))
            } else if col_ty.is_float32() {
                // A string literal adapts to a float column via float's input parse (the float8in
                // spellings — sign/digits/e-notation/Infinity/NaN; spec/design/float.md §4).
                // Malformed input traps 22P02, out of range 22003. So `INSERT ... VALUES ('NaN')`
                // works (a bare decimal literal cannot spell NaN/Infinity).
                Ok(Value::Float32(parse_f32_literal(&s)?))
            } else if col_ty.is_float64() {
                Ok(Value::Float64(parse_f64_literal(&s)?))
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
                Ok(Value::Timestamp(adjust_timestamp_typmod(m, tm.temporal)?))
            } else {
                Err(type_error(format!(
                    "cannot store a timestamp value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Timestamptz(m) => {
            if col_ty.is_timestamptz() {
                Ok(Value::Timestamptz(adjust_timestamp_typmod(m, tm.temporal)?))
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
                Ok(Value::Interval(adjust_interval_typmod(iv, tm.temporal)?))
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
        // A json value stores into a json column verbatim. json → jsonb remains explicit-only.
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
            } else if col_ty.is_json() {
                // jsonb → json is an assignment cast (json.md §6.1): the textual json column
                // receives jsonb_out's canonical representation, not the source input spelling.
                Ok(Value::Json(json::jsonb_out(&n)))
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
    tm: Typmod,
    not_null: bool,
    col_name: &str,
    iz: &InputZone,
) -> Result<Value> {
    match ty {
        ColType::Scalar(s) => store_value(v, *s, tm, not_null, col_name, iz),
        ColType::Composite { name, fields } => {
            store_composite(v, name, fields, not_null, col_name, iz)
        }
        ColType::Array(elem) => store_array(v, elem, not_null, col_name, iz),
        ColType::Range(elem) => store_range(v, elem, not_null, col_name, iz),
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
    iz: &InputZone,
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
                        *val,
                        elem,
                        Typmod::NONE,
                        false,
                        col_name,
                        iz,
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
    iz: &InputZone,
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
                out.push(coerce_for_store(
                    val,
                    elem,
                    Typmod::NONE,
                    false,
                    col_name,
                    iz,
                )?);
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
    iz: &InputZone,
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
                ))
                .no_hint());
            }
            let mut out = Vec::with_capacity(vals.len());
            for (val, f) in vals.into_iter().zip(fields.iter()) {
                out.push(coerce_for_store(
                    val,
                    &f.ty,
                    Typmod::of_col_field(f),
                    f.not_null,
                    &f.name,
                    iz,
                )?);
            }
            Ok(Value::Composite(out))
        }
        _ => Err(type_error(format!(
            "cannot store a non-record value in composite column {col_name} (type {type_name})"
        ))),
    }
}

/// Round a timestamp / timestamptz to a `timestamp(p)` typmod (timestamp.md §8); no typmod (or no
/// precision) passes it through.
pub(crate) fn adjust_timestamp_typmod(m: i64, tm: Option<TemporalTypmod>) -> Result<i64> {
    match tm.and_then(|t| t.precision) {
        Some(p) => crate::timestamp::adjust_timestamp_for_typmod(m, p),
        None => Ok(m),
    }
}

/// Cut an interval to its qualifier and precision typmod (interval.md §6.1); no typmod passes it
/// through.
pub(crate) fn adjust_interval_typmod(iv: Interval, tm: Option<TemporalTypmod>) -> Result<Interval> {
    match tm {
        Some(t) => interval::adjust_interval_for_typmod(iv, t),
        None => Ok(iv),
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
    iz: &InputZone,
) -> Result<Value> {
    match ty {
        ColType::Scalar(s) => match iv {
            InsertValue::Lit(lit, _) => literal_to_value_for(lit, *s),
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
            // Only a top-level slot is a general expression; the caller evaluates it.
            InsertValue::Expr(_) => {
                unreachable!("a VALUES expression slot is evaluated by the caller")
            }
        },
        ColType::Composite { name, fields } => match iv {
            InsertValue::Row(field_ivs) => {
                if field_ivs.len() != fields.len() {
                    // PostgreSQL's 42846 "cannot cast type record to T" (composite.md §12).
                    return Err(EngineError::new(
                        SqlState::CannotCoerce,
                        format!(
                            "ROW has {} fields but composite type {name} has {}",
                            field_ivs.len(),
                            fields.len()
                        ),
                    )
                    .no_hint());
                }
                let mut vals = Vec::with_capacity(fields.len());
                for (fiv, f) in field_ivs.iter().zip(fields.iter()) {
                    vals.push(materialize_insert_value(fiv, &f.ty, bound, iz)?);
                }
                Ok(Value::Composite(vals))
            }
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            // A bare NULL adapts to the composite as its whole-value NULL, and a bare string
            // literal adapts via record_in — the same unknown-literal-adapts-to-context rule the
            // array/range arms and the scalar slots follow (spec/design/composite.md §8).
            InsertValue::Lit(Literal::Null, _) => Ok(Value::Null),
            InsertValue::Lit(Literal::Text(s), _) => coerce_record_text(s, name, fields, iz),
            InsertValue::Lit(_, _) => Err(type_error(format!(
                "cannot assign a scalar value to composite column (type {name})"
            ))),
            InsertValue::Array(_) => Err(type_error(format!(
                "cannot assign an array value to composite column (type {name})"
            ))),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ROW(...)",
            )),
            // Only a top-level slot is a general expression; the caller evaluates it.
            InsertValue::Expr(_) => {
                unreachable!("a VALUES expression slot is evaluated by the caller")
            }
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
                        subs.push(materialize_insert_value(eiv, ty, bound, iz)?);
                    }
                    build_nested_array(subs)
                } else {
                    let mut vals = Vec::with_capacity(elem_ivs.len());
                    for eiv in elem_ivs {
                        vals.push(materialize_insert_value(eiv, elem, bound, iz)?);
                    }
                    Ok(Value::Array(ArrayVal::one_dim(vals)))
                }
            }
            // A bare string literal adapts to the array context via `array_in` (the same
            // string-adapts-to-context rule bytea/uuid use — types.md §6; spec/design/array.md §7).
            InsertValue::Lit(Literal::Text(s), _) => coerce_string_to_array(s, elem, iz),
            InsertValue::Lit(Literal::Null, _) => Ok(Value::Null),
            InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
            InsertValue::Lit(_, _) => Err(type_error(
                "cannot assign a scalar value to an array column".to_string(),
            )),
            InsertValue::Row(_) => Err(type_error(
                "cannot assign a record value to an array column".to_string(),
            )),
            InsertValue::Default => Err(EngineError::new(
                SqlState::SyntaxError,
                "DEFAULT is not allowed inside ARRAY[...]",
            )),
            // Only a top-level slot is a general expression; the caller evaluates it.
            InsertValue::Expr(_) => {
                unreachable!("a VALUES expression slot is evaluated by the caller")
            }
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
                InsertValue::Lit(Literal::Text(s), _) => {
                    Ok(Value::Range(coerce_string_to_range(s, desc, iz)?))
                }
                InsertValue::Lit(Literal::Null, _) => Ok(Value::Null),
                InsertValue::Param(nn) => Ok(bound[(*nn as usize) - 1].clone()),
                InsertValue::Lit(_, _) => Err(type_error(
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
                // Only a top-level slot is a general expression; the caller evaluates it.
                InsertValue::Expr(_) => {
                    unreachable!("a VALUES expression slot is evaluated by the caller")
                }
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
/// admits at resolve (numeric↔numeric, text→numeric/bool/uuid, bool↔i32, any scalar→text,
/// uuid↔bytea); an array element has no `numeric(p,s)` typmod (an array type takes no modifier), so
/// every decimal target is the unconstrained form. Overflow traps `22003`, a malformed text element
/// `22P02` — per element, exactly like the scalar cast. The resolver gate guarantees only the
/// admitted `(source, target)` pairs reach here, so the value/target combinations are exhaustive.
pub(crate) fn cast_array_element(
    v: Value,
    target: ScalarType,
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Value> {
    match v {
        v if target.is_text() => Ok(Value::Text(scalar_to_text(v, env, m)?)),
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

/// Render one non-null scalar as text — the `* → text` cast (spec/types/casts.toml, types.md §5).
/// Every type uses its own output form, the same text a result cell renders (`Value::render`),
/// except `timestamptz`, which renders in the SESSION zone (timezones.md §9.5) and charges the
/// `timezone` unit for a finite instant (±infinity consult no zone). Shared by the scalar cast and
/// the element-wise `T[] → text[]` cast.
pub(crate) fn scalar_to_text(v: Value, env: &EvalEnv, m: &mut Meter) -> Result<String> {
    use crate::timestamp::{NEG_INFINITY, POS_INFINITY};
    match v {
        Value::Text(s) | Value::Json(s) | Value::JsonPath(s) => Ok(s),
        Value::Timestamptz(mc) => {
            if mc != NEG_INFINITY && mc != POS_INFINITY {
                m.charge(COSTS.timezone);
                m.guard()?;
            }
            crate::timestamp::render_timestamptz_in(mc, &env.exec.session.time_zone)
        }
        v => Ok(v.render()),
    }
}

/// Whether jed admits an element-wise `array → other-element-array` cast from source element scalar
/// `from` to target element scalar `to` (spec/design/array.md §7). Mirrors the scalar cast matrix
/// (spec/types/casts.toml) for the pairs an array element can take: numeric↔numeric, text→numeric/
/// boolean/uuid, boolean⇄i32, any scalar→text, uuid⇄bytea. The identity (`from == to`) is handled by the
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
    if to.is_text() {
        return true; // any scalar → text (each type's output form — the scalar text cast)
    }
    if from.is_uuid() && to.is_bytea() {
        return true; // uuid → bytea (the uuid cast slice)
    }
    if from.is_bytea() && to.is_uuid() {
        return true; // bytea → uuid (the jed-only uuid cast)
    }
    false
}

pub(crate) fn coerce_string_to_array(s: &str, elem: &ColType, iz: &InputZone) -> Result<Value> {
    let parsed = crate::value::parse_array_literal(s).map_err(crate::value::array_in_error)?;
    let mut elements = Vec::with_capacity(parsed.tokens.len());
    for tok in parsed.tokens {
        match tok {
            None => elements.push(Value::Null),
            // Coerce the token to the element type (a scalar via the string-literal coercion, a
            // composite via record_in — array-of-composite, spec/design/array.md §12 AC1).
            Some(t) => elements.push(coerce_array_element_text(&t, elem, iz)?),
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
pub(crate) fn coerce_array_element_text(
    tok: &str,
    elem: &ColType,
    iz: &InputZone,
) -> Result<Value> {
    match elem {
        ColType::Scalar(s) => {
            let (node, _) = coerce_string_literal(tok, *s, Typmod::NONE, iz)?;
            rexpr_const_to_value(&node)
        }
        ColType::Composite { name, fields } => coerce_record_text(tok, name, fields, iz),
        ColType::Array(inner) => coerce_string_to_array(tok, inner, iz),
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
    iz: &InputZone,
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
                    let (node, _) = coerce_string_literal(&s, *sc, Typmod::of_col_field(f), iz)?;
                    rexpr_const_to_value(&node)?
                }
                ColType::Composite {
                    name: n2,
                    fields: f2,
                } => coerce_record_text(&s, n2, f2, iz)?,
                ColType::Array(inner) => coerce_string_to_array(&s, inner, iz)?,
                // A range-typed field: the token is a range text literal (`range_in`).
                ColType::Range(elem) => {
                    let ColType::Scalar(es) = elem.as_ref() else {
                        unreachable!("a range element is always a scalar (ranges.md §2)")
                    };
                    let desc = crate::range::range_for_element(*es)
                        .expect("a range field's element always has a range type");
                    Value::Range(coerce_string_to_range(&s, desc, iz)?)
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
        RExpr::ConstRange(range) => Value::Range((**range).clone()),
        _ => return Err(type_error("non-constant array element literal".to_string())),
    })
}
