//! The resolved-expression evaluator: RExpr::eval, the recursive interpreter that turns a resolved
//! expression tree into a Value over a row (mirrors the core of impl/go eval.go). The scalar value
//! kernels (arithmetic/cast/string/array/range) remain as free functions in mod.rs for now.

use super::*;

impl RExpr {
    /// Evaluate a predicate in a **filter context** — WHERE, ON, HAVING, a partial index's WHERE,
    /// ON CONFLICT … WHERE — returning whether it keeps the row (it is TRUE). The top-level AND
    /// chain stops at the first conjunct that is not TRUE (FALSE or NULL: the predicate can no
    /// longer be TRUE), PostgreSQL's qual-list rule; each AND node charges on entry as in `eval`,
    /// and every other node evaluates as a value (spec/design/cost.md §3).
    pub(crate) fn eval_filter(&self, row: &[Value], env: &EvalEnv, m: &mut Meter) -> Result<bool> {
        if let RExpr::And(l, r) = self {
            m.guard()?;
            m.charge(operator_cost("and"));
            return Ok(l.eval_filter(row, env, m)? && r.eval_filter(row, env, m)?);
        }
        Ok(self.eval(row, env, m)?.is_true())
    }

    /// Evaluate against a row, accruing cost into `m`. Returns a `Value` (which may be a
    /// boolean for comparisons/connectives). Arithmetic traps 22003 on overflow and 22012
    /// on a zero divisor; NULL propagates through arithmetic; the connectives are Kleene.
    ///
    /// Cost: each **interior** node charges `operator_eval` once, pre-order (the node, then
    /// its operands LHS-before-RHS); leaf nodes (column/constants) charge nothing. Operands are
    /// evaluated eagerly except where cost.md §3 sanctions a short-circuit (AND / OR, CASE,
    /// COALESCE), whose fixed left-to-right rule makes the skipped work deterministic.
    pub(crate) fn eval(&self, row: &[Value], env: &EvalEnv, m: &mut Meter) -> Result<Value> {
        // Enforce the cost ceiling before evaluating this node (CLAUDE.md §13). `eval` recurses
        // once per expression node, so guarding here bounds a pathological expression to ~O(1)
        // overshoot; it is a no-op when no ceiling is set (spec/design/cost.md §6).
        m.guard()?;
        match self {
            // The value is read out of a borrowed stored row, so it is cloned (Value is
            // Clone, not Copy, now that a text value owns a String). A deferred large value the
            // static touched set missed resolves ON TOUCH — the B4 demand-fault backstop
            // (bplus-reshape.md §5): deterministic rows, never a NULL-fold; unmetered (§6).
            RExpr::Column(i) => match &row[*i] {
                Value::Unfetched(u) => crate::format::resolve_unfetched_self(u),
                v => Ok(v.clone()),
            },
            // A correlated reference: the column `index` of the enclosing row `level` hops out
            // (1 = immediate parent). A leaf — reads from the outer-row environment (§26), with
            // the same demand-fault backstop as `Column`.
            RExpr::OuterColumn { level, index } => {
                outline(|| match &env.outer[env.outer.len() - level][*index] {
                    Value::Unfetched(u) => crate::format::resolve_unfetched_self(u),
                    v => Ok(v.clone()),
                })
            }
            // A bind parameter — the supplied value, already coerced to its inferred type by
            // `bind_params` before execution (spec/design/api.md §5).
            RExpr::Param(i) => Ok(env.params[*i].clone()),
            // A ROW(...) constructor — one operator_eval, then build the composite from the
            // evaluated fields (spec/design/composite.md §1, cost.md §9).
            RExpr::Row(fields) => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(fields.len());
                for f in fields {
                    vals.push(f.eval(row, env, m)?);
                }
                Ok(Value::Composite(vals))
            }),
            // An ARRAY[…] constructor — one operator_eval, then evaluate each element (already
            // coerced to the element type at resolve). A `nested` constructor stacks its sub-arrays
            // into one higher dimension (spec/design/array.md §4); otherwise it is a flat 1-D array.
            RExpr::Array { elems, nested } => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(elems.len());
                for e in elems {
                    vals.push(e.eval(row, env, m)?);
                }
                let out = if *nested {
                    build_nested_array(vals)?
                } else {
                    Value::Array(ArrayVal::one_dim(vals))
                };
                // A growth kernel (cost.md §8.1), free when every element is constant.
                if !elems.iter().all(rexpr_is_constant) {
                    charge_output(m, &out)?;
                }
                Ok(out)
            }),
            // A folded array constant (shape preserved) — return it directly.
            RExpr::ConstArray(a) => Ok(Value::Array((**a).clone())),
            RExpr::ConstRange(r) => Ok(Value::Range((**r).clone())),
            // Field selection — one operator_eval, then pull the resolved field ordinal out of the
            // evaluated composite. A whole-value-NULL composite yields NULL (PG); the index is in
            // range by construction (resolve fixed it against the static field list).
            RExpr::Field { base, index } => outline(|| {
                m.charge(COSTS.operator_eval);
                match base.eval(row, env, m)? {
                    Value::Composite(fields) => Ok(fields[*index].clone()),
                    Value::Null => Ok(Value::Null),
                    other => {
                        unreachable!("field access on a non-composite value: {other:?}")
                    }
                }
            }),
            // Array subscript `base[..][..]` (spec/design/array.md §6) — one operator_eval. A NULL
            // array or any NULL subscript bound yields NULL. Element access (`!is_slice`) returns
            // the element when the subscript count equals `ndim` and every index is in range, else
            // NULL; slice access returns a (renumbered) sub-array, with a scalar index `i` meaning
            // `1:i`. The per-element walk is internal (unmetered, cost.md §9).
            // Array subscript — extracted to a free function so its locals do not widen `eval`'s
            // (debug-build) stack frame on the deep-expression path.
            RExpr::Subscript {
                base,
                subscripts,
                is_slice,
            } => {
                m.charge(COSTS.operator_eval);
                eval_subscript(base, subscripts, *is_slice, row, env, m)
            }
            RExpr::ConstInt(n) => Ok(Value::Int(*n)),
            RExpr::ConstBool(b) => Ok(Value::Bool(*b)),
            RExpr::ConstText(s) => Ok(Value::Text(s.clone())),
            RExpr::ConstDecimal(d) => Ok(Value::Decimal(d.clone())),
            RExpr::ConstFloat32(f) => Ok(Value::Float32(*f)),
            RExpr::ConstFloat64(f) => Ok(Value::Float64(*f)),
            RExpr::ConstBytea(b) => Ok(Value::Bytea(b.clone())),
            RExpr::ConstUuid(u) => Ok(Value::Uuid(*u)),
            RExpr::ConstTimestamp(m) => Ok(Value::Timestamp(*m)),
            RExpr::ConstTimestamptz(m) => Ok(Value::Timestamptz(*m)),
            RExpr::ConstDate(d) => Ok(Value::Date(*d)),
            RExpr::ConstInterval(iv) => Ok(Value::Interval(*iv)),
            RExpr::ConstJson(s) => Ok(Value::Json(s.clone())),
            RExpr::ConstJsonPath(s) => Ok(Value::JsonPath(s.clone())),
            RExpr::ConstJsonb(n) => Ok(Value::Jsonb((**n).clone())),
            RExpr::ConstNull => Ok(Value::Null),
            RExpr::Cast {
                inner,
                target,
                typmod,
                varchar_len,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let out = match inner.eval(row, env, m)? {
                    Value::Null => Ok(Value::Null),
                    // Any scalar → text (casts.toml, types.md §5): the type's own output form,
                    // timestamptz in the session zone. Rendering bytea (hex) and jsonb (escapes) can
                    // grow the value, so those two are growth kernels (cost.md §8.1), charged for the
                    // full render before any varchar(n) truncation below.
                    v if target.is_text() => {
                        let grows = matches!(v, Value::Bytea(_) | Value::Jsonb(_));
                        let out = Value::Text(scalar_to_text(v, env, m)?);
                        if grows && !rexpr_is_constant(inner) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    Value::Int(n) => {
                        if target.is_bool() {
                            // i32 → boolean (the boolean cast slice, casts.toml): 0 → false, any
                            // nonzero (incl. negative) → true. The resolver guarantees the source
                            // is i32, so `n` is already in i32 range.
                            Ok(Value::Bool(n != 0))
                        } else if target.is_decimal() {
                            // int → decimal (lossless), then coerce to the typmod.
                            Ok(Value::Decimal(coerce_decimal(
                                Decimal::from_i64(n),
                                *typmod,
                            )?))
                        } else if target.is_float() {
                            // int → float (explicit; nearest, round-ties-to-even — Rust `as`).
                            // Never overflows: i64::MAX < f32::MAX, so the result is always finite.
                            Ok(int_to_float(n, *target))
                        } else if target.in_range(n) {
                            Ok(Value::Int(n))
                        } else {
                            Err(overflow(*target))
                        }
                    }
                    Value::Decimal(d) => {
                        if target.is_decimal() {
                            // decimal → decimal: re-scale to the target typmod.
                            Ok(Value::Decimal(coerce_decimal(d, *typmod)?))
                        } else if target.is_float() {
                            // decimal → float (explicit; nearest binary value). A magnitude that
                            // overflows the float range → 22003 (not ±Inf — the §3 finite rule).
                            decimal_to_float(&d, *target)
                        } else {
                            // decimal → int (explicit): round half-away to scale 0, then
                            // range-check the target integer type (22003).
                            let v = d.to_i64_round().ok_or_else(|| overflow(*target))?;
                            if target.in_range(v) {
                                Ok(Value::Int(v))
                            } else {
                                Err(overflow(*target))
                            }
                        }
                    }
                    // float → int / decimal / float (all explicit — spec/design/float.md §6).
                    Value::Float32(f) => cast_from_float(f as f64, *target, *typmod),
                    Value::Float64(f) => cast_from_float(f, *target, *typmod),
                    Value::Bool(b) => {
                        if target.is_bool() {
                            // boolean → boolean is the identity cast (`x::boolean` on a boolean).
                            Ok(Value::Bool(b))
                        } else {
                            // boolean → i32 (the boolean cast slice, casts.toml): true → 1, false →
                            // 0. The resolver guarantees the only non-bool target is i32.
                            Ok(Value::Int(i64::from(b)))
                        }
                    }
                    // text → json/jsonb (spec/design/json.md §6.1): json validates + stores
                    // verbatim (22P02 on malformed); jsonb parses + canonicalizes.
                    Value::Text(s) if target.is_json() => {
                        json::validate_json(&s)?;
                        Ok(Value::Json(s))
                    }
                    Value::Text(s) if target.is_jsonb() => Ok(Value::Jsonb(json::jsonb_in(&s)?)),
                    // text → uuid (the uuid cast slice, casts.toml/types.md §14): the PG-flexible
                    // uuid_in parser; a malformed string traps 22P02.
                    Value::Text(s) if target.is_uuid() => Ok(Value::Uuid(decode_uuid_literal(&s)?)),
                    // text → interval / bytea (types.md §5): the literal's own input per row —
                    // parse_interval (22007/22008) and the bytea input — `\x` hex or escape format (22P02).
                    Value::Text(s) if target.is_interval() => {
                        Ok(Value::Interval(crate::interval::parse_interval(&s)?))
                    }
                    Value::Text(s) if target.is_bytea() => {
                        Ok(Value::Bytea(decode_bytea_literal(&s)?))
                    }
                    // text → numeric/boolean (the runtime-text-cast slice, grammar.md §36): the same
                    // per-row coercion the `type 'string'` literal folds at resolve, run here over
                    // the runtime string. The resolver admits only int/decimal/float/bool targets
                    // for a text source (uuid/interval/bytea/json/jsonb are the guarded arms above),
                    // so this is exhaustive. Malformed → 22P02, out of range → 22003 (per row).
                    Value::Text(s) if target.is_bool() => Ok(Value::Bool(parse_bool_literal(&s)?)),
                    Value::Text(s) if target.is_decimal() => Ok(Value::Decimal(coerce_decimal(
                        parse_decimal_literal(&s)?,
                        *typmod,
                    )?)),
                    Value::Text(s) if *target == ScalarType::Float32 => {
                        Ok(Value::Float32(parse_f32_literal(&s)?))
                    }
                    Value::Text(s) if target.is_float() => {
                        Ok(Value::Float64(parse_f64_literal(&s)?))
                    }
                    Value::Text(s) => Ok(Value::Int(parse_int_literal(&s, *target)?)),
                    // bytea → uuid (the uuid cast slice — a jed cast PG lacks): exactly 16 raw bytes;
                    // any other length traps 22P02 (the wrong-width body — no PG code to match).
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
                    Value::Bytea(_) => unreachable!("resolver rejects a bytea cast operand"),
                    // uuid → bytea (the 16 raw bytes) — the uuid cast slice (casts.toml/types.md
                    // §14). uuid → text is the any-scalar → text arm above.
                    Value::Uuid(u) if target.is_bytea() => Ok(Value::Bytea(u.to_vec())),
                    Value::Uuid(_) => unreachable!("resolver rejects a uuid cast operand"),
                    Value::Timestamp(_) | Value::Timestamptz(_) => {
                        unreachable!("resolver rejects a timestamp cast operand")
                    }
                    Value::Date(_) => unreachable!("resolver rejects a date cast operand"),
                    Value::Interval(_) => unreachable!("resolver rejects an interval cast operand"),
                    Value::Composite(_) => {
                        unreachable!("resolver rejects a composite cast operand this slice")
                    }
                    Value::Array(_) => {
                        unreachable!("resolver rejects an array cast operand this slice")
                    }
                    Value::Range(_) => {
                        unreachable!("resolver rejects a range cast operand this slice")
                    }
                    Value::JsonPath(_) => {
                        unreachable!("resolver rejects a jsonpath cast operand this slice")
                    }
                    // The JSON cast matrix (spec/design/json.md §6.1). json → jsonb re-parses +
                    // canonicalizes; json → json is the identity (json → text is the any-scalar →
                    // text arm above: the identity on the verbatim bytes).
                    Value::Json(s) => {
                        if target.is_json() {
                            Ok(Value::Json(s))
                        } else if target.is_jsonb() {
                            Ok(Value::Jsonb(json::jsonb_in(&s)?))
                        } else {
                            unreachable!("resolver rejects this json cast target")
                        }
                    }
                    // jsonb → json renders the canonical form (jsonb_out); jsonb → jsonb is the
                    // identity (jsonb → text is the any-scalar → text arm above).
                    // Rendering is a growth kernel: escapes can expand strings (cost.md §8.1).
                    Value::Jsonb(n) if target.is_json() => {
                        let out = Value::Json(json::jsonb_out(&n));
                        if !rexpr_is_constant(inner) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    Value::Jsonb(n) if target.is_jsonb() => Ok(Value::Jsonb(n)),
                    Value::Jsonb(_) => unreachable!("resolver rejects this jsonb cast target"),
                    Value::Unfetched(_) => {
                        panic!("BUG: unfetched large value escaped the storage layer")
                    }
                }?;
                // A `varchar(n)` cast target silently truncates the resulting text to n code points
                // (the explicit-cast rule, types.md §15) — applied after any *→text conversion above.
                // A non-text result (or no length) passes through unchanged.
                match (varchar_len, out) {
                    (Some(n), Value::Text(s)) => {
                        Ok(Value::Text(truncate_to_chars(&s, *n as usize)))
                    }
                    (_, v) => Ok(v),
                }
            }),
            // The three array-involving casts (spec/design/array.md §7), none expressible by the
            // scalar `Cast` node: array→text (`array_out`), runtime text→T[] (`array_in` per row),
            // and element-wise array→array (each element through the scalar cast). The node carries
            // the cast's `operator_eval` charge (no new cost unit). `to_elem` is `None` for the
            // text target and `Some(target element)` for the two array-producing casts.
            RExpr::ArrayCast { inner, to_elem } => outline(|| {
                m.charge(COSTS.operator_eval);
                match inner.eval(row, env, m)? {
                    Value::Null => Ok(Value::Null),
                    // array → text: render via `array_out` (PG-byte-exact §7). A `timestamptz` at
                    // any depth renders in the SESSION zone, one `timezone` unit per finite instant,
                    // charged before rendering like the scalar text cast (timezones.md §9.5).
                    Value::Array(a) if to_elem.is_none() => {
                        let zoned = a.elements.iter().map(Value::zone_render_count).sum::<u64>();
                        if zoned > 0 {
                            m.charge(COSTS.timezone * zoned as i64);
                            m.guard()?;
                        }
                        let out = Value::Text(crate::value::array_out_zoned(
                            &a,
                            Some(&env.exec.session.time_zone),
                        )?);
                        // A growth kernel: element quoting can expand strings (cost.md §8.1).
                        if !rexpr_is_constant(inner) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // runtime text → T[]: coerce the per-row string via `array_in` against the
                    // target element ColType (22P02 malformed / 2202E inverted bound — the same
                    // errors the `'{…}'::T[]` literal path raises).
                    Value::Text(s) => coerce_string_to_array(
                        &s,
                        to_elem.as_ref().unwrap(),
                        &env.exec.input_zone(),
                    ),
                    // element-wise array → other-element-array: every non-null element through the
                    // scalar element cast to the target element (22003 per element on overflow); the
                    // shape (dims/lbounds) is preserved and a NULL element stays NULL. The target
                    // element is always a scalar — a same-element array is the identity (returned
                    // with no `ArrayCast` node at resolve) and a composite element cast is 0A000.
                    Value::Array(a) => {
                        let ColType::Scalar(scalar) = to_elem.as_ref().unwrap() else {
                            unreachable!("an array→array element cast has a scalar target element")
                        };
                        // bytea → text renders hex, so a bytea[] → text[] cast is a growth kernel
                        // (cost.md §8.1), charged for the whole result array.
                        let grows = scalar.is_text()
                            && a.elements.iter().any(|e| matches!(e, Value::Bytea(_)));
                        let mut elements = Vec::with_capacity(a.elements.len());
                        for e in a.elements {
                            elements.push(match e {
                                Value::Null => Value::Null,
                                v => cast_array_element(v, *scalar, env, m)?,
                            });
                        }
                        let out = Value::Array(ArrayVal {
                            dims: a.dims,
                            lbounds: a.lbounds,
                            elements,
                        });
                        if grows && !rexpr_is_constant(inner) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    _ => unreachable!("an ArrayCast operand is text or array (resolver-gated)"),
                }
            }),
            RExpr::Neg { operand, result } => outline(|| {
                m.charge(operator_cost("neg"));
                match operand.eval(row, env, m)? {
                    Value::Null => Ok(Value::Null),
                    Value::Int(n) if result.is_decimal() => {
                        Ok(Value::Decimal(Decimal::from_i64(n).neg()))
                    }
                    Value::Int(n) => {
                        // checked_neg guards i64::MIN; then range-check the result type.
                        let v = n.checked_neg().ok_or_else(|| overflow(*result))?;
                        if result.in_range(v) {
                            Ok(Value::Int(v))
                        } else {
                            Err(overflow(*result))
                        }
                    }
                    Value::Decimal(d) => Ok(Value::Decimal(d.neg())),
                    // Unary minus flips the float sign bit (no overflow; a NaN/Inf operand passes
                    // through — spec/design/float.md §5). Width preserved by the resolver's result.
                    Value::Float32(f) => Ok(Value::Float32(-f)),
                    Value::Float64(f) => Ok(Value::Float64(-f)),
                    Value::Bool(_) => unreachable!("resolver rejects a boolean unary minus"),
                    Value::Text(_) => unreachable!("resolver rejects a text unary minus"),
                    Value::Bytea(_) => unreachable!("resolver rejects a bytea unary minus"),
                    Value::Uuid(_) => unreachable!("resolver rejects a uuid unary minus"),
                    Value::Timestamp(_) | Value::Timestamptz(_) => {
                        unreachable!("resolver rejects a timestamp unary minus")
                    }
                    Value::Date(_) => unreachable!("resolver rejects a date unary minus"),
                    Value::Interval(iv) => Ok(Value::Interval(iv.neg()?)),
                    Value::Composite(_) => {
                        unreachable!("resolver rejects a composite unary minus")
                    }
                    Value::Array(_) => {
                        unreachable!("resolver rejects an array unary minus")
                    }
                    Value::Range(_) => {
                        unreachable!("resolver rejects a range unary minus")
                    }
                    Value::Json(_) | Value::Jsonb(_) | Value::JsonPath(_) => {
                        unreachable!("resolver rejects a json unary minus")
                    }
                    Value::Unfetched(_) => {
                        panic!("BUG: unfetched large value escaped the storage layer")
                    }
                }
            }),
            RExpr::Not(e) => outline(|| {
                m.charge(operator_cost("not"));
                let v = e.eval(row, env, m)?;
                Ok(not3(&v))
            }),
            RExpr::Arith {
                op,
                lhs,
                rhs,
                result,
            } => outline(|| {
                m.charge(operator_cost(op.op_name()));
                let a = lhs.eval(row, env, m)?;
                let b = rhs.eval(row, env, m)?;
                if matches!(a, Value::Null) || matches!(b, Value::Null) {
                    return Ok(Value::Null);
                }
                // Date arithmetic (spec/design/date.md §6): date ± int → date, date − date → i32,
                // date ± interval → timestamp. A Date operand is present iff this is date
                // arithmetic (the resolver settled `result` accordingly), so intercept it before
                // the interval/timestamp/integer dispatch below (which assume non-date operands).
                if matches!(a, Value::Date(_)) || matches!(b, Value::Date(_)) {
                    return eval_date_arith(*op, a, b, *result);
                }
                if result.is_interval() && matches!(op, ArithOp::Mul | ArithOp::Div) {
                    // interval ×÷ number → interval (the exact cascade; spec/design/interval.md
                    // §5). Mul commutes; Div is interval / number (the resolver guarantees the
                    // interval is the left operand). A zero divisor traps 22012.
                    let (iv, num) = match (a, b) {
                        (Value::Interval(iv), n) | (n, Value::Interval(iv)) => (iv, n),
                        _ => unreachable!("resolver guarantees an interval ×÷ number pair"),
                    };
                    let (fnum, fden) = factor_to_fraction(&num)?;
                    let (fnum, fden) = if matches!(op, ArithOp::Mul) {
                        (fnum, fden)
                    } else if fnum == 0 {
                        return Err(EngineError::new(
                            SqlState::DivisionByZero,
                            "division by zero",
                        ));
                    } else if fnum < 0 {
                        (-fden, -fnum) // interval / number = interval * (den/num); keep fden > 0
                    } else {
                        (fden, fnum)
                    };
                    Ok(Value::Interval(crate::interval::mul_by_fraction(
                        &iv, fnum, fden,
                    )?))
                } else if result.is_interval() {
                    // interval ± interval → interval; timestamp[tz] − timestamp[tz] → interval
                    // (spec/design/interval.md §5). Dispatch on the operand kinds.
                    match (a, b) {
                        (Value::Interval(x), Value::Interval(y)) => {
                            let r = match op {
                                ArithOp::Add => x.add(&y)?,
                                ArithOp::Sub => x.sub(&y)?,
                                _ => unreachable!("resolver allows only interval ±"),
                            };
                            Ok(Value::Interval(r))
                        }
                        (Value::Timestamp(x), Value::Timestamp(y))
                        | (Value::Timestamptz(x), Value::Timestamptz(y)) => {
                            Ok(Value::Interval(crate::interval::ts_diff(x, y)?))
                        }
                        _ => unreachable!("resolver guarantees a temporal-difference pair here"),
                    }
                } else if result.is_timestamp() || result.is_timestamptz() {
                    // timestamp[tz] ± interval → timestamp[tz] (calendar month-add with clamping;
                    // spec/design/interval.md §5). interval + timestamp commutes.
                    let subtract = matches!(op, ArithOp::Sub);
                    let (t, iv, is_tz) = match (a, b) {
                        (Value::Timestamp(t), Value::Interval(iv)) => (t, iv, false),
                        (Value::Interval(iv), Value::Timestamp(t)) => (t, iv, false),
                        (Value::Timestamptz(t), Value::Interval(iv)) => (t, iv, true),
                        (Value::Interval(iv), Value::Timestamptz(t)) => (t, iv, true),
                        _ => unreachable!("resolver guarantees a timestamp ± interval pair here"),
                    };
                    let r = crate::interval::ts_shift(t, &iv, subtract)?;
                    Ok(if is_tz {
                        Value::Timestamptz(r)
                    } else {
                        Value::Timestamp(r)
                    })
                } else if result.is_decimal() {
                    // Decimal arithmetic: widen any integer operand to decimal, then apply the
                    // op with PG's scale rules (spec/design/decimal.md §4). The size-scaled
                    // decimal_work is charged BEFORE the operation runs, so a cost ceiling
                    // aborts ahead of the limb work (spec/design/cost.md §3 "decimal_work").
                    let (da, db) = (to_decimal(a), to_decimal(b));
                    let w = decimal_arith_work(*op, &da, &db);
                    m.charge(COSTS.decimal_work * ((w - 1) as i64));
                    m.guard()?;
                    eval_decimal_arith(*op, da, db)
                } else if result.is_float() {
                    // Float arithmetic (spec/design/float.md §5): the IEEE correctly-rounded op at
                    // the result width, ONE op per node (no FMA fusion — the tree-walk guarantees
                    // it). The resolver promoted a mixed-width pair to f64, so both operands
                    // are already the result width. A finite overflow to ±Inf traps 22003, x/0
                    // traps 22012; an Inf/NaN operand propagates by IEEE.
                    match (a, b) {
                        (Value::Float32(x), Value::Float32(y)) => eval_float32_arith(*op, x, y),
                        (Value::Float64(x), Value::Float64(y)) => eval_float64_arith(*op, x, y),
                        _ => unreachable!("resolver promotes float arithmetic to one width"),
                    }
                } else {
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => eval_arith(*op, x, y, *result),
                        _ => unreachable!("resolver rejects non-integer arithmetic operands"),
                    }
                }
            }),
            RExpr::Compare {
                op,
                lhs,
                rhs,
                collation,
            } => outline(|| {
                m.charge(operator_cost(op.op_name()));
                let a = lhs.eval(row, env, m)?;
                let b = rhs.eval(row, env, m)?;
                // A decimal(-promotable) pair charges size-scaled decimal_work — once per
                // node, even where `<=`/`>=` decompose internally (cost.md §3 "decimal_work").
                m.charge(COSTS.decimal_work * ((decimal_cmp_work(&a, &b) - 1) as i64));
                m.guard()?;
                // A collated ORDERING comparison (`< <= > >=`) over two non-NULL text values orders
                // by the collation's UCA sort key (spec/design/collation.md §7), charging the
                // `collate` unit per code point of each operand (cost.md "collate"). `=`/`<>` are
                // byte-equality even under a deterministic collation (§7), so they take the plain
                // path and charge no collate. A NULL operand makes the result Unknown (no sort key).
                if let (Some(coll), CmpOp::Lt | CmpOp::Gt | CmpOp::Le | CmpOp::Ge) = (collation, op)
                {
                    if let (Value::Text(x), Value::Text(y)) = (&a, &b) {
                        m.charge(
                            COSTS.collate * (x.chars().count() as i64 + y.chars().count() as i64),
                        );
                        m.guard()?;
                        let ord = collated_cmp(coll, x, y)?;
                        let res = match op {
                            CmpOp::Lt => ord.is_lt(),
                            CmpOp::Gt => ord.is_gt(),
                            CmpOp::Le => ord.is_le(),
                            CmpOp::Ge => ord.is_ge(),
                            _ => unreachable!(),
                        };
                        return Ok(Value::Bool(res));
                    }
                    // Two text[] values walk their element pairs under the collation, charging
                    // `collate` per compared pair (collation.md §11).
                    if let (Value::Array(x), Value::Array(y)) = (&a, &b) {
                        let ord = collated_array_cmp(coll, x, y, Some(m))?;
                        let res = match op {
                            CmpOp::Lt => ord.is_lt(),
                            CmpOp::Gt => ord.is_gt(),
                            CmpOp::Le => ord.is_le(),
                            CmpOp::Ge => ord.is_ge(),
                            _ => unreachable!(),
                        };
                        return Ok(Value::Bool(res));
                    }
                    // Either operand NULL ⇒ Unknown (text comparison is 3-valued).
                    return Ok(Value::Null);
                }
                // Variable-length text/bytea comparison scans up to the shorter operand's length
                // (code points / bytes); charge `varlen_compare × (W − 1)` so the per-comparison
                // length work an untrusted join / correlated re-scan can amplify by fan-out is
                // metered, not flat (cost.md §3 "varlen_compare"). Collated ORDERING already charged
                // `collate` above and returned; this covers `=`/`<>`, `C`/default-collation
                // ordering, and all bytea. Charged before the compare runs, then guarded.
                m.charge(COSTS.varlen_compare * (varlen_compare_work(&a, &b) - 1));
                m.guard()?;
                let tv = match op {
                    CmpOp::Eq => a.eq3(&b),
                    CmpOp::Ne => a.eq3(&b).not(),
                    CmpOp::Lt => a.lt3(&b),
                    CmpOp::Gt => a.gt3(&b),
                    CmpOp::Le => a.lt3(&b).or(a.eq3(&b)),
                    CmpOp::Ge => a.gt3(&b).or(a.eq3(&b)),
                };
                Ok(from3(tv))
            }),
            RExpr::JsonGet { op, base, arg } => outline(|| {
                m.charge(COSTS.operator_eval);
                let bv = base.eval(row, env, m)?;
                let av = arg.eval(row, env, m)?;
                m.guard()?;
                // A NULL base or argument propagates to SQL NULL (the operators are strict).
                let node = match &bv {
                    Value::Null => return Ok(Value::Null),
                    Value::Jsonb(n) => n,
                    Value::Json(text) => {
                        if matches!(av, Value::Null) {
                            return Ok(Value::Null);
                        }
                        return json_get_verbatim(*op, text, &av);
                    }
                    _ => unreachable!("resolver guarantees a json/jsonb accessor base"),
                };
                if matches!(av, Value::Null) {
                    return Ok(Value::Null);
                }
                // Locate the accessed node: a key (text) / index (int) for `-> ->>`, or a text[]
                // path for `#> #>>`. A NULL element inside the path array misses (PG).
                let accessed: Option<&JsonNode> = match op {
                    JsonGetOp::Arrow | JsonGetOp::ArrowText => match &av {
                        Value::Text(k) => json::get_field(node, k),
                        Value::Int(i) => json::get_index(node, *i),
                        _ => unreachable!("resolver guarantees a text/int arg for -> / ->>"),
                    },
                    JsonGetOp::HashArrow | JsonGetOp::HashArrowText => match &av {
                        Value::Array(arr) => {
                            let mut steps: Vec<String> = Vec::with_capacity(arr.elements.len());
                            let mut null_step = false;
                            for e in &arr.elements {
                                match e {
                                    Value::Text(s) => steps.push(s.clone()),
                                    Value::Null => {
                                        null_step = true;
                                        break;
                                    }
                                    _ => unreachable!("a text[] path has text/NULL elements"),
                                }
                            }
                            if null_step {
                                None
                            } else {
                                json::get_path(node, &steps)
                            }
                        }
                        _ => unreachable!("resolver guarantees a text[] arg for #> / #>>"),
                    },
                };
                // `-> #>` return the node as jsonb; `->> #>>` render it as text (a JSON null or a
                // missing access → SQL NULL).
                match (op, accessed) {
                    (_, None) => Ok(Value::Null),
                    (JsonGetOp::Arrow | JsonGetOp::HashArrow, Some(n)) => {
                        Ok(Value::Jsonb(n.clone()))
                    }
                    (JsonGetOp::ArrowText | JsonGetOp::HashArrowText, Some(n)) => {
                        Ok(json::node_to_text(n).map_or(Value::Null, Value::Text))
                    }
                }
            }),
            RExpr::JsonContains { a, b } => outline(|| {
                m.charge(COSTS.operator_eval);
                let av = a.eval(row, env, m)?;
                let bv = b.eval(row, env, m)?;
                m.guard()?;
                match (&av, &bv) {
                    (Value::Jsonb(na), Value::Jsonb(nb)) => Ok(Value::Bool(json::contains(na, nb))),
                    // Strict: a NULL operand yields SQL NULL.
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    _ => unreachable!("resolver guarantees jsonb operands for @> / <@"),
                }
            }),
            RExpr::JsonHasKey { kind, base, arg } => outline(|| {
                m.charge(COSTS.operator_eval);
                let bv = base.eval(row, env, m)?;
                let av = arg.eval(row, env, m)?;
                m.guard()?;
                let node = match &bv {
                    Value::Null => return Ok(Value::Null),
                    Value::Jsonb(n) => n,
                    _ => unreachable!("resolver guarantees a jsonb base for ? / ?| / ?&"),
                };
                if matches!(av, Value::Null) {
                    return Ok(Value::Null);
                }
                let result = match kind {
                    HasKeyKind::One => match &av {
                        Value::Text(k) => json::has_key(node, k),
                        _ => unreachable!("resolver guarantees a text arg for ?"),
                    },
                    HasKeyKind::Any | HasKeyKind::All => match &av {
                        Value::Array(arr) => {
                            // A NULL element is skipped (PG `jsonb_exists_any`/`_all`): `?|` never
                            // matches it, and `?&` over only NULLs (or nothing) is vacuously true
                            // (json-sql-functions.md §1.2).
                            let mut keys: Vec<&str> = Vec::with_capacity(arr.elements.len());
                            for e in &arr.elements {
                                match e {
                                    Value::Text(s) => keys.push(s),
                                    Value::Null => {}
                                    _ => unreachable!("a text[] arg has text/NULL elements"),
                                }
                            }
                            match kind {
                                HasKeyKind::Any => keys.iter().any(|k| json::has_key(node, k)),
                                HasKeyKind::All => keys.iter().all(|k| json::has_key(node, k)),
                                HasKeyKind::One => unreachable!(),
                            }
                        }
                        _ => unreachable!("resolver guarantees a text[] arg for ?| / ?&"),
                    },
                };
                Ok(Value::Bool(result))
            }),
            RExpr::JsonConcat { a, b } => outline(|| {
                m.charge(COSTS.operator_eval);
                let av = a.eval(row, env, m)?;
                let bv = b.eval(row, env, m)?;
                m.guard()?;
                match (&av, &bv) {
                    (Value::Jsonb(na), Value::Jsonb(nb)) => {
                        // Wrapping a non-array operand can deepen the result (json.md §6.4).
                        let out = json::concat(na, nb);
                        json::check_depth(&out)?;
                        let out = Value::Jsonb(out);
                        // A growth kernel (cost.md §8.1), free over constant operands.
                        if !(rexpr_is_constant(a) && rexpr_is_constant(b)) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    _ => unreachable!("resolver guarantees jsonb operands for ||"),
                }
            }),
            RExpr::JsonDelete { kind, base, arg } => outline(|| {
                m.charge(COSTS.operator_eval);
                let bv = base.eval(row, env, m)?;
                let av = arg.eval(row, env, m)?;
                m.guard()?;
                let node = match &bv {
                    Value::Null => return Ok(Value::Null),
                    Value::Jsonb(n) => n,
                    _ => unreachable!("resolver guarantees a jsonb base for - / #-"),
                };
                if matches!(av, Value::Null) {
                    return Ok(Value::Null);
                }
                let result = match kind {
                    DeleteKind::Key => match &av {
                        Value::Text(k) => json::delete_key(node, k)?,
                        _ => unreachable!("resolver guarantees a text arg for - key"),
                    },
                    DeleteKind::Index => match &av {
                        Value::Int(i) => json::delete_index(node, *i)?,
                        _ => unreachable!("resolver guarantees an int arg for - index"),
                    },
                    // A NULL key is skipped; a NULL path element reached raises 22004 (PG).
                    DeleteKind::Keys => json::delete_keys(node, &json_text_path_arg(&av)?)?,
                    DeleteKind::Path => json::delete_path(node, &json_text_path_arg(&av)?)?,
                };
                Ok(Value::Jsonb(result))
            }),
            // jsonb_set / jsonb_insert (json-sql-functions.md §2): STRICT path mutation. Any NULL
            // argument → SQL NULL (a NULL path element is the kernel's 22004). jsonb_set_lax is jsonb_set that is not
            // strict in the value: a SQL NULL value is handled by its null_value_treatment.
            RExpr::JsonSetInsert { mode, lax, args } => outline(|| {
                m.charge(COSTS.operator_eval);
                let target = args[0].eval(row, env, m)?;
                let path_v = args[1].eval(row, env, m)?;
                let value_v = args[2].eval(row, env, m)?;
                let flag_v = args[3].eval(row, env, m)?;
                let mut value_node = None;
                if *lax {
                    let treatment_v = args[4].eval(row, env, m)?;
                    if matches!(target, Value::Null)
                        || matches!(path_v, Value::Null)
                        || matches!(flag_v, Value::Null)
                    {
                        return Ok(Value::Null);
                    }
                    let treatment = match &treatment_v {
                        Value::Text(t) => t.as_str(),
                        // An explicit NULL treatment is rejected even for a non-NULL value (PG).
                        _ => return Err(json::bad_null_value_treatment()),
                    };
                    if matches!(value_v, Value::Null) {
                        match treatment {
                            "raise_exception" => return Err(json::null_value_raised()),
                            "use_json_null" => value_node = Some(JsonNode::Null),
                            "delete_key" => {
                                // `#-` (jsonb_delete_path): removing a member never grows the
                                // document, so no output charge and no depth check.
                                let path = json_text_path_arg(&path_v)?;
                                let node = json_arg_node(&target)?;
                                return Ok(Value::Jsonb(json::delete_path(&node, &path)?));
                            }
                            "return_target" => return Ok(target),
                            _ => return Err(json::bad_null_value_treatment()),
                        }
                    }
                    // A non-NULL value (whatever the treatment says) or use_json_null: jsonb_set.
                } else if matches!(target, Value::Null)
                    || matches!(path_v, Value::Null)
                    || matches!(value_v, Value::Null)
                    || matches!(flag_v, Value::Null)
                {
                    return Ok(Value::Null);
                }
                let path = json_text_path_arg(&path_v)?;
                let node = json_arg_node(&target)?;
                let value_node = match value_node {
                    Some(n) => n,
                    None => json_arg_node(&value_v)?,
                };
                let flag = matches!(flag_v, Value::Bool(true));
                let out = match mode {
                    json::PathSetMode::Set => json::set_path(&node, &path, &value_node, flag)?,
                    json::PathSetMode::Insert => {
                        json::insert_path(&node, &path, &value_node, flag)?
                    }
                };
                // Inserting a document inside another can deepen the result (json.md §6.4).
                json::check_depth(&out)?;
                let out = Value::Jsonb(out);
                // A growth kernel (cost.md §8.1), free over constant operands.
                if !args.iter().all(rexpr_is_constant) {
                    charge_output(m, &out)?;
                }
                Ok(out)
            }),
            // json_object / jsonb_object (json-sql-functions.md §2): build an object from text array(s).
            RExpr::JsonObjectFromArrays { json, args } => outline(|| {
                m.charge(COSTS.operator_eval);
                // STRICT: a NULL whole-array argument → SQL NULL.
                let mut arrays = Vec::with_capacity(args.len());
                for a in args {
                    let v = a.eval(row, env, m)?;
                    if matches!(v, Value::Null) {
                        return Ok(Value::Null);
                    }
                    arrays.push(
                        value_to_opt_text_array(&v).expect("resolver guarantees a text[] arg"),
                    );
                }
                // Pair up keys/values: one array of alternating k/v (even length), or two arrays.
                let pairs: Vec<(Option<String>, Option<String>)> = if arrays.len() == 1 {
                    let flat = &arrays[0];
                    if flat.len() % 2 != 0 {
                        return Err(EngineError::new(
                            SqlState::ArraySubscriptError,
                            "array must have even number of elements",
                        ));
                    }
                    flat.chunks_exact(2)
                        .map(|c| (c[0].clone(), c[1].clone()))
                        .collect()
                } else {
                    if arrays[0].len() != arrays[1].len() {
                        return Err(EngineError::new(
                            SqlState::ArraySubscriptError,
                            "mismatched array dimensions",
                        ));
                    }
                    arrays[0]
                        .iter()
                        .cloned()
                        .zip(arrays[1].iter().cloned())
                        .collect()
                };
                m.charge(COSTS.operator_eval * pairs.len() as i64);
                m.guard()?;
                // A NULL key → 22004; a NULL value → JSON null, else a JSON string of its text.
                let out = if *json {
                    let mut parts = Vec::with_capacity(pairs.len());
                    for (k, v) in &pairs {
                        let key = k.as_ref().ok_or_else(object_key_null)?;
                        let val = match v {
                            Some(s) => json::json_compact_out(&JsonNode::String(s.clone())),
                            None => "null".to_string(),
                        };
                        parts.push(format!(
                            "{} : {}",
                            json::json_compact_out(&JsonNode::String(key.clone())),
                            val
                        ));
                    }
                    Value::Json(format!("{{{}}}", parts.join(", ")))
                } else {
                    let mut members = Vec::with_capacity(pairs.len());
                    for (k, v) in pairs {
                        let key = k.ok_or_else(object_key_null)?;
                        let node = match v {
                            Some(s) => JsonNode::String(s),
                            None => JsonNode::Null,
                        };
                        members.push((key, node));
                    }
                    Value::Jsonb(json::make_object(members))
                };
                // A growth kernel (cost.md §8.1), free over constant operands.
                if !args.iter().all(rexpr_is_constant) {
                    charge_output(m, &out)?;
                }
                Ok(out)
            }),
            // A scalar jsonpath query function (P2, jsonpath.md §5). STRICT: a NULL argument → NULL.
            RExpr::JsonPathFn { kind, args, .. } => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(row, env, m)?);
                }
                if vals.iter().any(|v| matches!(v, Value::Null)) {
                    return Ok(Value::Null);
                }
                // The `jsonb_path_*` functions take a `vars` object (one base object for
                // `.keyvalue()` ids, checked before evaluation even in silent mode) and a `silent`
                // flag; the operators are PostgreSQL's silent forms with no variables
                // (jsonpath.md §3.1, §5).
                let (vars, silent) = match &vals[2..] {
                    [Value::Jsonb(v), Value::Bool(s)] => {
                        crate::jsonpath::check_vars_object(v)?;
                        (crate::jsonpath::Vars::Object(v), *s)
                    }
                    _ => (crate::jsonpath::Vars::None, true),
                };
                let casing = env.exec.session.env.property();
                let Some((node, compiled)) = jsonpath_operands(&vals[0], &vals[1], casing, m)?
                else {
                    return Ok(Value::Null);
                };
                let opts = crate::jsonpath::EvalOpts {
                    exists: matches!(kind, JsonPathFnKind::Exists | JsonPathFnKind::ExistsSilent),
                    vars,
                    casing,
                };
                let (seq, errored) = if silent {
                    crate::jsonpath::eval_silent(&compiled, &node, opts, m)?
                } else {
                    (crate::jsonpath::eval(&compiled, &node, opts, m)?, false)
                };
                // Charge per produced item so a runaway `[*]` fan-out stays cost-proportional.
                m.charge(COSTS.operator_eval * seq.len() as i64);
                m.guard()?;
                match kind {
                    // Silent: a SQL/JSON error → SQL NULL.
                    JsonPathFnKind::Exists | JsonPathFnKind::ExistsSilent if errored => {
                        Ok(Value::Null)
                    }
                    JsonPathFnKind::Exists | JsonPathFnKind::ExistsSilent => {
                        Ok(Value::Bool(!seq.is_empty()))
                    }
                    JsonPathFnKind::QueryFirst => Ok(seq
                        .into_iter()
                        .next()
                        .map_or(Value::Null, |n| Value::Jsonb(n.into_owned()))),
                    JsonPathFnKind::QueryArray => {
                        // Wrapping the items in an array can deepen the result (json.md §6.4).
                        let out = JsonNode::Array(seq.into_iter().map(Cow::into_owned).collect());
                        json::check_depth(&out)?;
                        let out = Value::Jsonb(out);
                        // A growth kernel (cost.md §8.1), free over constant operands.
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // jsonb_path_match: the path must produce EXACTLY one boolean item (or a JSON
                    // null — an unknown predicate — which is SQL NULL). Silent (`@@`, or
                    // `silent => true`) suppresses anything else to SQL NULL; an error ends the
                    // evaluation, and the items found before it still decide.
                    JsonPathFnKind::Match | JsonPathFnKind::MatchSilent => {
                        match seq.iter().map(|n| n.as_ref()).collect::<Vec<_>>()[..] {
                            [JsonNode::Bool(b)] => Ok(Value::Bool(*b)),
                            [JsonNode::Null] => Ok(Value::Null),
                            _ if silent => Ok(Value::Null),
                            _ => Err(EngineError::new(
                                SqlState::SingletonSqlJsonItemRequired,
                                "single boolean result is expected",
                            )),
                        }
                    }
                }
            }),
            // A SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md
            // §5, S2). A NULL context / path → NULL; a SQL/JSON (class-22) error honors ON ERROR.
            RExpr::JsonSqlFn {
                kind,
                ctx,
                path,
                passing,
                passing_names,
                returning,
                decimal,
                wrapper,
                on_empty,
                on_error,
                on_empty_default,
                on_error_default,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let defaults = JsonDefaults {
                    on_empty: on_empty_default.as_deref(),
                    on_error: on_error_default.as_deref(),
                    row,
                };
                let cv = ctx.eval(row, env, m)?;
                let pv = path.eval(row, env, m)?;
                if matches!(cv, Value::Null) || matches!(pv, Value::Null) {
                    return Ok(Value::Null);
                }
                // Reading a `json` context as jsonb (a `\u0000` / unpaired surrogate raises) is the
                // context's conversion, not path evaluation, so ON ERROR does not catch it — as in
                // PostgreSQL (json.md §4.2).
                let casing = env.exec.session.env.property();
                let Some((node, compiled)) = jsonpath_operands(&cv, &pv, casing, m)? else {
                    return Ok(Value::Null);
                };
                // The PASSING values, evaluated after the context and path (as in PostgreSQL) and
                // outside ON ERROR: each is a base object for `.keyvalue()` ids (jsonpath.md §3.1).
                let vars = passing_bindings(passing, passing_names, row, env, m)?;
                // A SQL/JSON (data-exception) error from path navigation is caught by ON ERROR;
                // anything else (a cost abort, a missing variable, etc.) propagates.
                let opts = crate::jsonpath::EvalOpts {
                    casing,
                    exists: matches!(kind, JsonSqlKind::Exists),
                    vars: crate::jsonpath::Vars::Passing {
                        vars: &vars,
                        count: passing.len() as i64,
                    },
                };
                let seq = match crate::jsonpath::eval(&compiled, &node, opts, m) {
                    Ok(s) => s,
                    Err(e) if is_sqljson_error(&e) => {
                        return apply_json_behavior(
                            *on_error,
                            JsonClause::Error,
                            defaults,
                            e,
                            *returning,
                            env,
                            m,
                        );
                    }
                    Err(e) => return Err(e),
                };
                m.charge(COSTS.operator_eval * seq.len() as i64);
                m.guard()?;
                let constant = rexpr_is_constant(ctx)
                    && rexpr_is_constant(path)
                    && passing.iter().all(rexpr_is_constant);
                eval_json_sql_result(
                    *kind, seq, *returning, *decimal, *wrapper, *on_empty, *on_error, defaults,
                    constant, env, m,
                )
            }),
            // AND / OR short-circuit left to right (cost.md §3): a FALSE left operand decides AND,
            // a TRUE one decides OR, and the right operand is then never evaluated or charged.
            RExpr::And(l, r) => outline(|| {
                m.charge(operator_cost("and"));
                let lv = l.eval(row, env, m)?;
                if matches!(lv, Value::Bool(false)) {
                    return Ok(lv);
                }
                let rv = r.eval(row, env, m)?;
                Ok(and3(&lv, &rv))
            }),
            RExpr::Or(l, r) => outline(|| {
                m.charge(operator_cost("or"));
                let lv = l.eval(row, env, m)?;
                if matches!(lv, Value::Bool(true)) {
                    return Ok(lv);
                }
                let rv = r.eval(row, env, m)?;
                Ok(or3(&lv, &rv))
            }),
            RExpr::IsNull { operand, negated } => outline(|| {
                m.charge(COSTS.operator_eval);
                // IS [NOT] NULL is always a definite boolean, never unknown (CLAUDE.md §4). For a
                // composite operand this is PG's recursive all-fields rule (NOT a negation —
                // spec/design/composite.md §5); a scalar follows the ordinary rule. `is_null_test`
                // unifies both.
                let v = operand.eval(row, env, m)?;
                Ok(Value::Bool(v.is_null_test(*negated)))
            }),
            RExpr::IsJson {
                operand,
                negated,
                kind,
                unique_keys,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let v = operand.eval(row, env, m)?;
                let ok = match v {
                    Value::Null => return Ok(Value::Null), // a NULL operand → NULL (never raises)
                    // jsonb is always well-formed with unique keys; only the kind can fail.
                    Value::Jsonb(node) => json_pred_kind_matches(&node, *kind),
                    // A string / json operand: parse (preserving duplicate keys); malformed → false.
                    // Only the UNIQUE KEYS check reads string contents, so only it de-escapes (PG's
                    // `need_escapes = check_unique_keys`): a `\u0000` or unpaired surrogate is then
                    // a parse failure → false (json.md §4.2).
                    Value::Json(s) | Value::Text(s) => match if *unique_keys {
                        json::parse_preserving(&s)
                    } else {
                        json::parse_shape(&s)
                    } {
                        Err(_) => false,
                        Ok(node) => {
                            json_pred_kind_matches(&node, *kind)
                                && !(*unique_keys && json::has_duplicate_keys(&node))
                        }
                    },
                    _ => unreachable!(
                        "resolver restricts IS JSON to a string / json / jsonb operand"
                    ),
                };
                Ok(Value::Bool(ok ^ *negated))
            }),
            RExpr::JsonCtor {
                operand,
                unique_keys,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let v = operand.eval(row, env, m)?;
                match v {
                    Value::Null => Ok(Value::Null),
                    Value::Text(s) => {
                        // Validate the string is well-formed JSON (22P02 on malformed), preserving
                        // duplicate keys so the optional UNIQUE KEYS check (22030) can see them. Only
                        // that check de-escapes (PG's `need_escapes = unique_keys`, json.md §4.2).
                        if !*unique_keys {
                            json::parse_shape(&s)?;
                            return Ok(Value::Json(s));
                        }
                        let node = json::parse_preserving(&s)?;
                        if json::has_duplicate_keys(&node) {
                            return Err(EngineError::new(
                                SqlState::DuplicateJsonObjectKeyValue,
                                "duplicate JSON object key value",
                            ));
                        }
                        // The result is the verbatim input text as a `json` value (PG).
                        Ok(Value::Json(s))
                    }
                    _ => unreachable!("resolver restricts JSON() to a text operand"),
                }
            }),
            RExpr::Distinct { lhs, rhs, negated } => outline(|| {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                let rv = rhs.eval(row, env, m)?;
                // IS [NOT] DISTINCT FROM is a comparison: a decimal pair charges its
                // size-scaled decimal_work like Compare (cost.md §3 "decimal_work").
                m.charge(COSTS.decimal_work * ((decimal_cmp_work(&lv, &rv) - 1) as i64));
                m.guard()?;
                let same = lv.not_distinct_from(&rv);
                // `negated` carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks
                // "are they the same?" → `same`; IS DISTINCT FROM asks the opposite. Either
                // way the result is a definite boolean — never unknown (the null_safe
                // discipline, functions.md §3).
                Ok(Value::Bool(same == *negated))
            }),
            RExpr::Like {
                lhs,
                rhs,
                negated,
                insensitive,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let subject = lhs.eval(row, env, m)?;
                let pattern = rhs.eval(row, env, m)?;
                // NULL propagates BEFORE the matcher runs, so a malformed pattern against a
                // NULL operand is still NULL, never 22025 (matches PG — grammar.md §22).
                if matches!(subject, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                let (s, p) = match (&subject, &pattern) {
                    (Value::Text(s), Value::Text(p)) => (s.as_str(), p.as_str()),
                    _ => unreachable!("resolver requires text LIKE operands"),
                };
                // ILIKE: simple-lowercase both sides under the engine casing regime (collation.md
                // §16) before matching — 1:1 folding so `_`/length semantics survive.
                let matched = if *insensitive {
                    let prop = env.exec.session.env.property();
                    let p_ref = prop.as_deref();
                    let s = crate::collation::fold_lower_simple(s, p_ref);
                    let p = crate::collation::fold_lower_simple(p, p_ref);
                    like_match(&s, &p)?
                } else {
                    like_match(s, p)?
                };
                // `negated` carries NOT LIKE/ILIKE: matched != negated flips for the NOT form.
                Ok(Value::Bool(matched != *negated))
            }),
            RExpr::Regex {
                lhs,
                rhs,
                negated,
                insensitive,
                program,
                compile_charged,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let subject = lhs.eval(row, env, m)?;
                let pattern = rhs.eval(row, env, m)?;
                // NULL propagates BEFORE the matcher runs (regex.md §1) — a malformed pattern
                // against a NULL operand is still NULL, never 2201B.
                if matches!(subject, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                let (s, p) = match (&subject, &pattern) {
                    (Value::Text(s), Value::Text(p)) => (s.as_str(), p.as_str()),
                    _ => unreachable!("resolver requires text regex operands"),
                };
                // ~* (insensitive): simple-lowercase under the engine casing regime (collation.md
                // §16). The subject is folded here; the constant pattern was folded at resolve, a
                // non-constant pattern is folded below before compiling.
                let prop = if *insensitive {
                    env.exec.session.env.property()
                } else {
                    None
                };
                let subj_folded;
                let subj_chars: Vec<char> = if *insensitive {
                    subj_folded = crate::collation::fold_lower_simple(s, prop.as_deref());
                    subj_folded.chars().collect()
                } else {
                    s.chars().collect()
                };
                let matched = match program {
                    Some(prog) => {
                        // Constant precompiled pattern: charge its regex_compile cost ONCE per
                        // statement execution (on first eval), not per row (regex.md §5).
                        if !compile_charged.get() {
                            compile_charged.set(true);
                            m.charge(COSTS.regex_compile * prog.ninst() as i64);
                            m.guard()?;
                        }
                        prog.is_match(&subj_chars, m)?
                    }
                    None => {
                        // Non-constant pattern: compile now (charging regex_compile) and run.
                        let pat_folded;
                        let pat_ref = if *insensitive {
                            pat_folded = crate::regex::fold_pattern(p, prop.as_deref());
                            pat_folded.as_str()
                        } else {
                            p
                        };
                        let prog = crate::regex::compile(pat_ref)?;
                        m.charge(COSTS.regex_compile * prog.ninst() as i64);
                        m.guard()?;
                        prog.is_match(&subj_chars, m)?
                    }
                };
                // `negated` carries !~ / !~*: matched != negated flips for the negated form.
                Ok(Value::Bool(matched != *negated))
            }),
            RExpr::RegexFunc {
                func,
                args,
                program,
                compile_charged,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                // STRICT: evaluate the args; any NULL short-circuits to NULL (regex.md §8).
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    let v = a.eval(row, env, m)?;
                    if matches!(v, Value::Null) {
                        return Ok(Value::Null);
                    }
                    vals.push(v);
                }
                let text = |v: &Value| -> String {
                    match v {
                        Value::Text(s) => s.clone(),
                        _ => unreachable!("resolver requires text regexp_* operands"),
                    }
                };
                let int = |v: &Value| -> i64 {
                    match v {
                        Value::Int(n) => *n,
                        _ => unreachable!("resolver requires integer regexp_* operands"),
                    }
                };
                let source = text(&vals[0]);
                let pattern = text(&vals[1]);
                // Per-function argument layout (regex.md §8 / §8b); the numeric defaults match PG.
                let mut start = 1i64;
                let mut nth = 1i64;
                let mut endoption = 0i64;
                let mut subexpr = 0i64;
                let mut replacement: Option<String> = None;
                let mut flags = String::new();
                match func {
                    RegexFunc::Replace => {
                        replacement = Some(text(&vals[2]));
                        if let Some(v) = vals.get(3) {
                            flags = text(v);
                        }
                    }
                    RegexFunc::Substring => {}
                    RegexFunc::Match
                    | RegexFunc::Like
                    | RegexFunc::SplitToArray
                    | RegexFunc::Matches
                    | RegexFunc::SplitToTable => {
                        if let Some(v) = vals.get(2) {
                            flags = text(v);
                        }
                    }
                    RegexFunc::Count => {
                        if let Some(v) = vals.get(2) {
                            start = int(v);
                        }
                        if let Some(v) = vals.get(3) {
                            flags = text(v);
                        }
                    }
                    RegexFunc::Substr => {
                        if let Some(v) = vals.get(2) {
                            start = int(v);
                        }
                        if let Some(v) = vals.get(3) {
                            nth = int(v);
                        }
                        if let Some(v) = vals.get(4) {
                            flags = text(v);
                        }
                        if let Some(v) = vals.get(5) {
                            subexpr = int(v);
                        }
                    }
                    RegexFunc::Instr => {
                        if let Some(v) = vals.get(2) {
                            start = int(v);
                        }
                        if let Some(v) = vals.get(3) {
                            nth = int(v);
                        }
                        if let Some(v) = vals.get(4) {
                            endoption = int(v);
                        }
                        if let Some(v) = vals.get(5) {
                            flags = text(v);
                        }
                        if let Some(v) = vals.get(6) {
                            subexpr = int(v);
                        }
                    }
                }
                // Numeric argument validation (regex.md §8b), BEFORE the pattern compiles (PG order:
                // a bad `start` beats a bad pattern). 22023 names the offending parameter.
                let bad_param = |p: &str, v: i64| {
                    EngineError::new(
                        SqlState::InvalidParameterValue,
                        format!("invalid value for parameter \"{p}\": {v}"),
                    )
                };
                match func {
                    RegexFunc::Count => {
                        if start < 1 {
                            return Err(bad_param("start", start));
                        }
                    }
                    RegexFunc::Substr => {
                        if start < 1 {
                            return Err(bad_param("start", start));
                        }
                        if nth < 1 {
                            return Err(bad_param("n", nth));
                        }
                        if subexpr < 0 {
                            return Err(bad_param("subexpr", subexpr));
                        }
                    }
                    RegexFunc::Instr => {
                        if start < 1 {
                            return Err(bad_param("start", start));
                        }
                        if nth < 1 {
                            return Err(bad_param("n", nth));
                        }
                        if endoption != 0 && endoption != 1 {
                            return Err(bad_param("endoption", endoption));
                        }
                        if subexpr < 0 {
                            return Err(bad_param("subexpr", subexpr));
                        }
                    }
                    RegexFunc::Replace
                    | RegexFunc::Match
                    | RegexFunc::Like
                    | RegexFunc::SplitToArray
                    | RegexFunc::Matches
                    | RegexFunc::SplitToTable
                    | RegexFunc::Substring => {}
                }
                // Validate flags: `i` and `g` are known, anything else is 2201B. Then (PG order) `g`
                // on a function that does not take it — all but regexp_replace / regexp_matches — is
                // PostgreSQL's 22023 (regex.md §8c).
                for c in flags.chars() {
                    if c != 'i' && c != 'g' {
                        return Err(EngineError::new(
                            SqlState::InvalidRegularExpression,
                            format!("invalid regular expression: invalid option \"{c}\""),
                        ));
                    }
                }
                let insensitive = flags.contains('i');
                let global = flags.contains('g');
                if global && !matches!(func, RegexFunc::Replace | RegexFunc::Matches) {
                    return Err(EngineError::new(
                        SqlState::InvalidParameterValue,
                        format!(
                            "{}() does not support the \"global\" option",
                            explain_regex_func(*func)
                        ),
                    ));
                }
                // The original-case subject (for output/captures) and the matched subject (folded
                // when case-insensitive — same length, so offsets carry over, regex.md §8).
                let orig_chars: Vec<char> = source.chars().collect();
                let prop = if insensitive {
                    env.exec.session.env.property()
                } else {
                    None
                };
                let match_chars: Vec<char> = if insensitive {
                    crate::collation::fold_lower_simple(&source, prop.as_deref())
                        .chars()
                        .collect()
                } else {
                    orig_chars.clone()
                };
                // The compiled program: precompiled (constant pattern + statically-known `i`), else
                // compiled now (charging regex_compile once per row).
                let owned_prog;
                let prog: &crate::regex::Program = match program {
                    Some(p) => {
                        if !compile_charged.get() {
                            compile_charged.set(true);
                            m.charge(COSTS.regex_compile * p.ninst() as i64);
                            m.guard()?;
                        }
                        p
                    }
                    None => {
                        let pat = if insensitive {
                            crate::regex::fold_pattern(&pattern, prop.as_deref())
                        } else {
                            pattern.clone()
                        };
                        owned_prog = crate::regex::compile(&pat)?;
                        m.charge(COSTS.regex_compile * owned_prog.ninst() as i64);
                        m.guard()?;
                        &owned_prog
                    }
                };
                let len = match_chars.len();
                // 0-based search start; clamp to len+1 (wasm-safe, and a start past len+1 simply
                // never enters the iteration loop → 0 / NULL, the PG rule, regex.md §8b).
                let start0 = (start - 1).min(len as i64 + 1) as usize;
                match func {
                    RegexFunc::Replace => {
                        let repl: Vec<char> = replacement.unwrap().chars().collect();
                        Ok(Value::Text(regexp_replace_text(
                            prog,
                            &match_chars,
                            &orig_chars,
                            &repl,
                            global,
                            m,
                        )?))
                    }
                    RegexFunc::Match => {
                        let mut arrays =
                            regexp_capture_arrays(prog, &match_chars, &orig_chars, false, m)?;
                        Ok(arrays.pop().unwrap_or(Value::Null))
                    }
                    RegexFunc::Like => Ok(Value::Bool(prog.is_match(&match_chars, m)?)),
                    RegexFunc::SplitToArray | RegexFunc::SplitToTable => {
                        let pieces = prog.split_pieces(&match_chars, &orig_chars, m)?;
                        Ok(Value::Array(ArrayVal::one_dim(
                            pieces.into_iter().map(Value::Text).collect(),
                        )))
                    }
                    // The SRF transport (RegexFunc::Matches): one text[] capture array per match.
                    RegexFunc::Matches => Ok(Value::Array(ArrayVal::one_dim(
                        regexp_capture_arrays(prog, &match_chars, &orig_chars, global, m)?,
                    ))),
                    RegexFunc::Count => {
                        Ok(Value::Int(prog.regexp_count(&match_chars, start0, m)?))
                    }
                    RegexFunc::Substr | RegexFunc::Substring => {
                        match prog.nth_match(&match_chars, start0, nth, m)? {
                            None => Ok(Value::Null),
                            Some(caps) => {
                                // substring(text, text) reports the first capture group when the
                                // pattern has one, else the whole match (regex.md §8d).
                                let subexpr = if matches!(func, RegexFunc::Substring) {
                                    i64::from(prog.ngroups > 0)
                                } else {
                                    subexpr
                                };
                                // `subexpr` selects the whole match (0) or a capture group; out of range
                                // (> group count) or a non-participating group (-1) → NULL.
                                if subexpr > prog.ngroups as i64 {
                                    return Ok(Value::Null);
                                }
                                let si = 2 * subexpr as usize;
                                let (s2, e2) = (prog.slot(&caps, si), prog.slot(&caps, si + 1));
                                if s2 < 0 || e2 < 0 {
                                    return Ok(Value::Null);
                                }
                                Ok(Value::Text(
                                    orig_chars[s2 as usize..e2 as usize].iter().collect(),
                                ))
                            }
                        }
                    }
                    RegexFunc::Instr => match prog.nth_match(&match_chars, start0, nth, m)? {
                        None => Ok(Value::Int(0)),
                        Some(caps) => {
                            if subexpr > prog.ngroups as i64 {
                                return Ok(Value::Int(0));
                            }
                            let si = 2 * subexpr as usize;
                            let (s2, e2) = (prog.slot(&caps, si), prog.slot(&caps, si + 1));
                            if s2 < 0 || e2 < 0 {
                                return Ok(Value::Int(0));
                            }
                            // endoption 0 → first-char position, 1 → after-last-char (1-based).
                            Ok(Value::Int(if endoption == 0 { s2 + 1 } else { e2 + 1 }))
                        }
                    },
                }
            }),
            RExpr::Casing { upper, arg } => outline(|| {
                m.charge(COSTS.operator_eval);
                match arg.eval(row, env, m)? {
                    Value::Null => Ok(Value::Null),
                    Value::Text(s) => {
                        let prop = env.exec.session.env.property();
                        Ok(Value::Text(crate::collation::fold_case(
                            &s,
                            *upper,
                            prop.as_deref(),
                        )))
                    }
                    _ => unreachable!("resolver restricts upper/lower to text operands"),
                }
            }),
            RExpr::AtTimeZone {
                zone,
                value,
                to_timestamptz,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let zv = zone.eval(row, env, m)?;
                let vv = value.eval(row, env, m)?;
                if matches!(zv, Value::Null) || matches!(vv, Value::Null) {
                    return Ok(Value::Null);
                }
                let zone_str = match &zv {
                    Value::Text(s) => s.as_str(),
                    _ => unreachable!("resolver requires a text zone"),
                };
                let micros = match vv {
                    Value::Timestamp(m) | Value::Timestamptz(m) => m,
                    _ => unreachable!("resolver requires a timestamp/timestamptz value"),
                };
                m.charge(COSTS.timezone);
                m.guard()?;
                // ±infinity passes through unchanged (PG): no zone offset applies.
                if micros == crate::timestamp::POS_INFINITY
                    || micros == crate::timestamp::NEG_INFINITY
                {
                    return Ok(if *to_timestamptz {
                        Value::Timestamptz(micros)
                    } else {
                        Value::Timestamp(micros)
                    });
                }
                let zr = env.exec.session.env.resolve_zone(zone_str).ok_or_else(|| {
                    EngineError::new(
                        SqlState::InvalidParameterValue,
                        format!("time zone \"{zone_str}\" not recognized"),
                    )
                })?;
                // A finite reading shifted past the timestamp range traps 22008.
                Ok(if *to_timestamptz {
                    Value::Timestamptz(
                        crate::timezone::local_to_instant_micros(&zr, micros)
                            .ok_or_else(crate::timestamp::zone_shift_out_of_range)?,
                    )
                } else {
                    Value::Timestamp(
                        crate::timezone::instant_to_local_micros(&zr, micros)
                            .ok_or_else(crate::timestamp::zone_shift_out_of_range)?,
                    )
                })
            }),
            RExpr::DateTrunc { unit, value, zone } => outline(|| {
                m.charge(COSTS.operator_eval);
                let uv = unit.eval(row, env, m)?;
                let vv = value.eval(row, env, m)?;
                let zv = match zone {
                    Some(z) => Some(z.eval(row, env, m)?),
                    None => None,
                };
                if matches!(uv, Value::Null)
                    || matches!(vv, Value::Null)
                    || matches!(zv, Some(Value::Null))
                {
                    return Ok(Value::Null);
                }
                let unit_s = match &uv {
                    Value::Text(s) => s.as_str(),
                    _ => unreachable!("resolver requires a text unit"),
                };
                match vv {
                    Value::Timestamp(mc) => Ok(Value::Timestamp(
                        crate::datetime_fn::date_trunc_micros(unit_s, mc, false)?,
                    )),
                    Value::Interval(iv) => Ok(Value::Interval(
                        crate::datetime_fn::date_trunc_interval(unit_s, iv)?,
                    )),
                    Value::Timestamptz(mc) => {
                        // ±infinity passes through (PG), no zone consulted — the kernel still
                        // validates the unit (22023 / 0A000 even at ±inf).
                        if mc == crate::timestamp::POS_INFINITY
                            || mc == crate::timestamp::NEG_INFINITY
                        {
                            return Ok(Value::Timestamptz(crate::datetime_fn::date_trunc_micros(
                                unit_s, mc, true,
                            )?));
                        }
                        // Truncate in the explicit zone (3-arg) or the session zone (2-arg).
                        let zr = match &zv {
                            Some(Value::Text(s)) => {
                                env.exec.session.env.resolve_zone(s).ok_or_else(|| {
                                    EngineError::new(
                                        SqlState::InvalidParameterValue,
                                        format!("time zone \"{s}\" not recognized"),
                                    )
                                })?
                            }
                            Some(_) => unreachable!("resolver requires a text zone"),
                            None => env.exec.session.time_zone.clone(),
                        };
                        m.charge(COSTS.timezone);
                        m.guard()?;
                        let local = crate::timezone::instant_to_local_micros(&zr, mc)
                            .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
                        let trunc = crate::datetime_fn::date_trunc_micros(unit_s, local, true)?;
                        Ok(Value::Timestamptz(
                            crate::timezone::local_to_instant_micros(&zr, trunc)
                                .ok_or_else(crate::timestamp::zone_shift_out_of_range)?,
                        ))
                    }
                    _ => unreachable!("resolver restricts date_trunc to ts/tstz/interval"),
                }
            }),
            RExpr::Extract { field, value } => outline(|| {
                m.charge(COSTS.operator_eval);
                let vv = value.eval(row, env, m)?;
                use crate::datetime_fn::ExtractSrc;
                let src = match vv {
                    Value::Null => return Ok(Value::Null),
                    Value::Timestamp(mc) => ExtractSrc::Timestamp(mc),
                    Value::Date(d) => ExtractSrc::Date(d),
                    Value::Interval(iv) => ExtractSrc::Interval(iv),
                    Value::Timestamptz(mc) => {
                        // `epoch` is zone-independent (the instant); every other field decomposes in
                        // the session zone — so only the zone-consulting fields charge `timezone`.
                        if field == "epoch"
                            || mc == crate::timestamp::POS_INFINITY
                            || mc == crate::timestamp::NEG_INFINITY
                        {
                            ExtractSrc::Timestamptz {
                                instant: mc,
                                local: mc,
                                offset_secs: 0,
                            }
                        } else {
                            let zr = env.exec.session.time_zone.clone();
                            m.charge(COSTS.timezone);
                            m.guard()?;
                            let local = crate::timezone::instant_to_local_micros(&zr, mc)
                                .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
                            let off =
                                crate::timezone::offset_at_ref(&zr, mc.div_euclid(1_000_000)).utoff;
                            ExtractSrc::Timestamptz {
                                instant: mc,
                                local,
                                offset_secs: off as i64,
                            }
                        }
                    }
                    _ => unreachable!("resolver restricts EXTRACT to ts/tstz/date/interval"),
                };
                // An oscillating field of a ±infinity source is NULL (timezones.md §9.2).
                Ok(match crate::datetime_fn::extract_field(field, src)? {
                    Some(d) => Value::Decimal(d),
                    None => Value::Null,
                })
            }),
            RExpr::DateConvert { inner, to } => outline(|| {
                m.charge(COSTS.operator_eval);
                let v = inner.eval(row, env, m)?;
                if matches!(v, Value::Null) {
                    return Ok(Value::Null);
                }
                eval_date_convert(v, *to, env, m)
            }),
            RExpr::DateClock { offset_days } => outline(|| {
                // A clock-relative date literal ('today'/'now'/'tomorrow'/'yesterday' —
                // date.md §6): the statement clock's day in the session zone + offset_days.
                // STABLE — the clock is read once per statement, so every evaluation in the
                // statement yields the same day.
                m.charge(COSTS.operator_eval);
                date_clock_value(env.exec, env.rng, m, *offset_days)
            }),
            RExpr::Case {
                arms,
                els,
                coerce_decimal,
            } => outline(|| {
                // CASE is a sanctioned short-circuit (cost.md §3), like AND / OR and COALESCE:
                // conditions are evaluated in order and evaluation STOPS at the first TRUE — a
                // FALSE or NULL/UNKNOWN condition falls through, and later arms (and their
                // results) are NOT evaluated. This is required for PG semantics (e.g.
                // `CASE WHEN a=0 THEN 0 ELSE 1/a END` must not divide by zero). Charge the node,
                // then only the conditions up to the match plus the selected result accrue.
                m.charge(COSTS.operator_eval);
                for (cond, result) in arms {
                    if cond.eval(row, env, m)?.is_true() {
                        return Ok(coerce_case(result.eval(row, env, m)?, *coerce_decimal));
                    }
                }
                Ok(coerce_case(els.eval(row, env, m)?, *coerce_decimal))
            }),
            RExpr::Coalesce {
                args,
                coerce_decimal,
            } => outline(|| {
                // COALESCE shares CASE's sanctioned short-circuit (cost.md §3): charge the node,
                // then evaluate arguments left to right — each at most ONCE — stopping at the
                // first non-NULL, which is the result. All-NULL → NULL. Later arguments are never
                // evaluated, so an error (or cost) in an unreached argument does not surface
                // (grammar.md §51).
                m.charge(COSTS.operator_eval);
                for a in args {
                    let v = a.eval(row, env, m)?;
                    if !matches!(v, Value::Null) {
                        return Ok(coerce_case(v, *coerce_decimal));
                    }
                }
                Ok(Value::Null)
            }),
            RExpr::GreatestLeast {
                args,
                coerce_decimal,
                greatest,
                collation,
            } => outline(|| {
                // GREATEST/LEAST is EAGER (grammar.md §52): charge the node, then evaluate EVERY
                // argument (all must be, to be compared — GREATEST(1, 1/0) traps). NULL arguments
                // are ignored; the running winner is the max (greatest) or min (least) under the
                // unified type's total order (`value_cmp`, or `collation` for text / text[]). All-NULL →
                // NULL. Non-NULL values are coerced to the unified type (integer → decimal) before
                // comparison so the comparator sees a single type. Each comparison after the first
                // charges its size-scaled work (decimal_work / varlen_compare / collate), the same
                // metering an explicit `<` chain would — the untrusted-query cost bound (§13).
                m.charge(COSTS.operator_eval);
                let mut best: Option<Value> = None;
                for a in args {
                    let v = coerce_case(a.eval(row, env, m)?, *coerce_decimal);
                    if matches!(v, Value::Null) {
                        continue;
                    }
                    match &best {
                        None => best = Some(v),
                        Some(cur) => {
                            let ord = if let (Some(coll), Value::Text(a), Value::Text(b)) =
                                (collation, &v, cur)
                            {
                                m.charge(
                                    COSTS.collate * (a.chars().count() + b.chars().count()) as i64,
                                );
                                m.guard()?;
                                collated_cmp(coll, a, b)?
                            } else if let (Some(coll), Value::Array(a), Value::Array(b)) =
                                (collation, &v, cur)
                            {
                                collated_array_cmp(coll, a, b, Some(m))?
                            } else {
                                m.charge(
                                    COSTS.decimal_work * (decimal_cmp_work(&v, cur) - 1) as i64,
                                );
                                m.charge(COSTS.varlen_compare * (varlen_compare_work(&v, cur) - 1));
                                m.guard()?;
                                value_cmp(&v, cur)
                            };
                            let take = if *greatest { ord.is_gt() } else { ord.is_lt() };
                            if take {
                                best = Some(v);
                            }
                        }
                    }
                }
                Ok(best.unwrap_or(Value::Null))
            }),
            RExpr::NullIf {
                args,
                coerce_decimal,
                widen_float,
            } => outline(|| {
                // NULLIF is EAGER (grammar.md §58): charge the node, evaluate `a` then `b` (both,
                // always — NULLIF(1, 1/0) traps), then compare them as `=` does, charging the same
                // size-scaled work (decimal_work / varlen_compare). Equal → NULL; otherwise —
                // including when either side is NULL — the result is `a`.
                m.charge(COSTS.operator_eval);
                let a = args[0].eval(row, env, m)?;
                let b = args[1].eval(row, env, m)?;
                m.charge(COSTS.decimal_work * ((decimal_cmp_work(&a, &b) - 1) as i64));
                m.charge(COSTS.varlen_compare * (varlen_compare_work(&a, &b) - 1));
                m.guard()?;
                let equal = if *widen_float {
                    // A mixed-width float pair compares at f64 (the implicit f32 → f64 cast).
                    let widen = |v: &Value| match v {
                        Value::Float32(x) => Value::Float64(f64::from(*x)),
                        other => other.clone(),
                    };
                    widen(&a).eq3(&widen(&b))
                } else {
                    a.eq3(&b)
                };
                if from3(equal).is_true() {
                    return Ok(Value::Null);
                }
                Ok(coerce_case(a, *coerce_decimal))
            }),
            RExpr::HostFunc {
                id, args, result, ..
            } => outline(|| {
                // A host-defined scalar function (spec/design/extensibility.md §4.2 / §5.1): the
                // injection seam. The kernel is reached BY ID through the frozen registry — the path
                // a compiled `match` on the closed `ScalarFunc` enum cannot offer a host.
                let hf = env.exec.session.env.functions().function(*id);
                // The declared static cost (cost.md §6 design (a)), charged once per call; arguments
                // charge their own. Guard immediately after (the size-scaled-charge discipline,
                // cost.md §3) so a large declared weight aborts BEFORE the kernel runs.
                m.charge(hf.cost);
                m.guard()?;
                // The kernel's outcome for this row: prefetched by the site's batch (§4.2.1 —
                // speculative batch, scalar replay), or a batch-of-one call when it has none. A
                // prefetched outcome proves every argument is trivially evaluable (free, cannot raise)
                // and non-NULL on this row, so evaluating them again here would charge nothing, raise
                // nothing, and not short-circuit — the replay skips it.
                let out = match env.host_batch.and_then(|b| b.take(self)) {
                    Some(outcome) => outcome?,
                    None => {
                        // STRICT: a NULL argument short-circuits to NULL before the kernel runs
                        // (§4.2), so the kernel never sees a NULL — exactly like the built-in
                        // scalar-function arm.
                        let mut vals = Vec::with_capacity(args.len());
                        for a in args {
                            let v = a.eval(row, env, m)?;
                            if matches!(v, Value::Null) {
                                return Ok(Value::Null); // NULL propagates
                            }
                            vals.push(v);
                        }
                        hf.call_one(vals)?
                    }
                };
                // Defend jed's own strict type system against a misbehaving host kernel (CLAUDE.md
                // §13 — the host owns its consequences, but a wrong-typed value must never reach
                // jed's codecs / comparators): the returned value must be NULL or match the declared
                // scalar result type.
                if !crate::extension::value_matches_result(&out, *result) {
                    return Err(EngineError::new(
                        SqlState::DataException,
                        format!(
                            "host function returned a value not matching its declared result type {result:?}"
                        ),
                    ));
                }
                // An integer must also fit the declared width: `Value::Int` is one i64 carrier for
                // i16/i32/i64, so a kernel declared `-> i32` returning 2^40 matches the type above
                // yet would leak an out-of-range i32 into comparisons / casts / key encodings. Trap
                // 22003 here, on this row, exactly as a built-in's result-boundary check does.
                if let Value::Int(x) = out
                    && !result.in_range(x)
                {
                    return Err(overflow(*result));
                }
                Ok(out)
            }),
            RExpr::ScalarFunc { func, args, result } => outline(|| {
                // One operator_eval per call (the uniform weight); arguments charge their own.
                m.charge(COSTS.operator_eval);
                // quote_nullable is the one NON-STRICT scalar function (null = "none"): a NULL
                // argument yields the text 'NULL', not a propagated NULL, so it must run before
                // the strict short-circuit loop below (string-functions.md §3).
                if matches!(func, ScalarFunc::QuoteNullable) {
                    let out = Value::Text(match args[0].eval(row, env, m)? {
                        Value::Null => "NULL".to_string(),
                        Value::Text(s) => quote_literal_text(&s),
                        _ => unreachable!("resolver restricts quote_nullable to text"),
                    });
                    // A growth kernel: quoting doubles every quote (cost.md §8.1).
                    if !args.iter().all(rexpr_is_constant) {
                        charge_output(m, &out)?;
                    }
                    return Ok(out);
                }
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    let v = a.eval(row, env, m)?;
                    if matches!(v, Value::Null) {
                        return Ok(Value::Null); // NULL propagates
                    }
                    vals.push(v);
                }
                match func {
                    ScalarFunc::Abs => match &vals[0] {
                        // abs over an integer: |x| then range-check at the result type's
                        // boundary (abs(i16 -32768) → 22003), exactly like Neg.
                        Value::Int(n) => {
                            let v = n.checked_abs().ok_or_else(|| overflow(*result))?;
                            if result.in_range(v) {
                                Ok(Value::Int(v))
                            } else {
                                Err(overflow(*result))
                            }
                        }
                        Value::Decimal(d) => Ok(Value::Decimal(d.abs())),
                        // abs over a float keeps the operand width (NaN passes through; |±Inf| = Inf).
                        Value::Float32(f) => Ok(Value::Float32(f.abs())),
                        Value::Float64(f) => Ok(Value::Float64(f.abs())),
                        _ => unreachable!("resolver restricts abs to numeric operands"),
                    },
                    // sign: -1 / 0 / +1. Decimal → numeric at scale 0; float → f64 (EXACT,
                    // in-contract). sign(NaN) = sign(±0) = 0, sign(±Inf) = ±1 (PG dsign tests
                    // x > 0 / x < 0, so NaN falls through to 0). Dispatches on the operand, like abs.
                    ScalarFunc::Sign => match &vals[0] {
                        Value::Decimal(d) => {
                            let s = if d.is_zero() {
                                0
                            } else if d.is_negative() {
                                -1
                            } else {
                                1
                            };
                            Ok(Value::Decimal(Decimal::from_i64(s)))
                        }
                        Value::Float64(f) => {
                            let r = if *f > 0.0 {
                                1.0
                            } else if *f < 0.0 {
                                -1.0
                            } else {
                                0.0 // NaN and ±0 → 0 (PG-faithful)
                            };
                            Ok(Value::Float64(r))
                        }
                        _ => unreachable!("resolver restricts sign to decimal/float operands"),
                    },
                    // div(a, b): the truncated integer quotient at scale 0, computed EXACTLY as
                    // (a − a%b)/b — a − a%b is exactly q·b, so the division is exact and the
                    // round_to_scale(0) only drops the (already-zero) fraction. 22012 on a zero
                    // divisor (the a%b step traps, like the `%` operator). Integer operands promote.
                    ScalarFunc::Div => {
                        let to_dec = |v: &Value| match v {
                            Value::Int(n) => Decimal::from_i64(*n),
                            Value::Decimal(d) => d.clone(),
                            _ => unreachable!("resolver restricts div to integer/decimal operands"),
                        };
                        let a = to_dec(&vals[0]);
                        let b = to_dec(&vals[1]);
                        let r = a.rem(&b)?;
                        let diff = a.sub(&r)?;
                        let q = diff.div(&b)?;
                        Ok(Value::Decimal(q.round_to_scale(0)))
                    }
                    // gcd: integer operands → Euclid (a result whose magnitude overflows the promoted
                    // type → 22003 — gcd(i64::MIN, 0) and the rare i16-cap edge); a decimal operand →
                    // exact decimal Euclid at scale max(sₐ, s_b). gcd(0, 0) = 0.
                    ScalarFunc::Gcd => match (&vals[0], &vals[1]) {
                        (Value::Int(a), Value::Int(b)) => {
                            let g = gcd_i64(*a, *b).ok_or_else(|| overflow(*result))?;
                            if result.in_range(g) {
                                Ok(Value::Int(g))
                            } else {
                                Err(overflow(*result))
                            }
                        }
                        _ => {
                            let (a, b) = (value_to_decimal(&vals[0]), value_to_decimal(&vals[1]));
                            Ok(Value::Decimal(gcd_decimal(&a, &b)?))
                        }
                    },
                    // lcm: |a/gcd · b|. Integer → the promoted type (an i64-overflow or
                    // out-of-result-type magnitude → 22003); a decimal operand → exact at scale
                    // max(sₐ, s_b). lcm(_, 0) = 0.
                    ScalarFunc::Lcm => match (&vals[0], &vals[1]) {
                        (Value::Int(a), Value::Int(b)) => {
                            let l = lcm_i64(*a, *b).ok_or_else(|| overflow(*result))?;
                            if result.in_range(l) {
                                Ok(Value::Int(l))
                            } else {
                                Err(overflow(*result))
                            }
                        }
                        _ => {
                            let (a, b) = (value_to_decimal(&vals[0]), value_to_decimal(&vals[1]));
                            Ok(Value::Decimal(lcm_decimal(&a, &b)?))
                        }
                    },
                    // factorial(n) = n! at scale 0. A negative operand → 22003. Each multiply is
                    // metered (size-scaled decimal_work, guarded) so the cost ceiling bounds a large
                    // factorial before its limb work runs (cost.md §3, §13); a product over the
                    // decimal value cap traps 22003.
                    ScalarFunc::Factorial => {
                        let n = match &vals[0] {
                            Value::Int(n) => *n,
                            _ => unreachable!("resolver restricts factorial to an integer operand"),
                        };
                        if n < 0 {
                            return Err(EngineError::new(
                                SqlState::NumericValueOutOfRange,
                                "factorial of a negative number is undefined",
                            ));
                        }
                        let mut acc = Decimal::from_i64(1);
                        let mut k = 2i64;
                        while k <= n {
                            let kd = Decimal::from_i64(k);
                            m.charge(
                                COSTS.decimal_work * ((decimal::work_mul(&acc, &kd) - 1) as i64),
                            );
                            m.guard()?;
                            acc = acc.mul(&kd)?;
                            k += 1;
                        }
                        Ok(Value::Decimal(acc))
                    }
                    // width_bucket(op, low, high, count): the histogram bucket index. count > 0
                    // (else 2201G); dispatch numeric vs float on the operand; the raw index is
                    // range-checked to int4 (count+1 past int4 max → 22003 "integer out of range").
                    ScalarFunc::WidthBucket => {
                        let count = match &vals[3] {
                            Value::Int(n) => *n,
                            _ => unreachable!("resolver restricts width_bucket count to integer"),
                        };
                        if count <= 0 {
                            return Err(width_bucket_err("count must be greater than zero"));
                        }
                        // The resolver guarantees the value trio is homogeneous: all float → the
                        // float kernel; otherwise the numeric kernel (integers promote to decimal).
                        let idx = if matches!(vals[0], Value::Float32(_) | Value::Float64(_)) {
                            let f = |v: &Value| match v {
                                Value::Float32(f) => *f as f64,
                                Value::Float64(f) => *f,
                                _ => unreachable!("resolver makes the float trio homogeneous"),
                            };
                            width_bucket_float(f(&vals[0]), f(&vals[1]), f(&vals[2]), count)?
                        } else {
                            let (op, low, high) = (
                                value_to_decimal(&vals[0]),
                                value_to_decimal(&vals[1]),
                                value_to_decimal(&vals[2]),
                            );
                            width_bucket_numeric(&op, &low, &high, count)?
                        };
                        if ScalarType::Int32.in_range(idx) {
                            Ok(Value::Int(idx))
                        } else {
                            Err(overflow(ScalarType::Int32))
                        }
                    }
                    // scale(numeric) → the display (fractional-digit) scale, as i32 (always ≤ 16383).
                    ScalarFunc::Scale => match &vals[0] {
                        Value::Decimal(d) => Ok(Value::Int(d.scale() as i64)),
                        _ => unreachable!("resolver restricts scale to a decimal operand"),
                    },
                    // min_scale(numeric) → the smallest exact scale (trailing fractional zeros dropped).
                    ScalarFunc::MinScale => match &vals[0] {
                        Value::Decimal(d) => Ok(Value::Int(min_scale_of(d) as i64)),
                        _ => unreachable!("resolver restricts min_scale to a decimal operand"),
                    },
                    // trim_scale(numeric) → the value re-scaled down to its min_scale (exact; the
                    // dropped digits are zeros, so round_to_scale does not round).
                    ScalarFunc::TrimScale => match &vals[0] {
                        Value::Decimal(d) => Ok(Value::Decimal(d.round_to_scale(min_scale_of(d)))),
                        _ => unreachable!("resolver restricts trim_scale to a decimal operand"),
                    },
                    // round over a float (1- or 2-arg) → f64 (half-away — the engine's mode;
                    // a NaN/Inf operand passes through). Distinguished from decimal round by the
                    // operand variant.
                    ScalarFunc::Round if matches!(&vals[0], Value::Float64(_)) => {
                        let f = match &vals[0] {
                            Value::Float64(f) => *f,
                            _ => unreachable!(),
                        };
                        let places = match vals.get(1) {
                            None => 0,
                            Some(Value::Int(k)) => *k,
                            Some(_) => unreachable!("resolver restricts round's count to integer"),
                        };
                        Ok(Value::Float64(round_f64_places(f, places)))
                    }
                    ScalarFunc::Round => {
                        let d = match &vals[0] {
                            Value::Int(n) => Decimal::from_i64(*n),
                            Value::Decimal(d) => d.clone(),
                            _ => {
                                unreachable!("resolver restricts round to numeric operands")
                            }
                        };
                        let places = match vals.get(1) {
                            None => 0,
                            Some(Value::Int(k)) => *k,
                            Some(_) => unreachable!("resolver restricts round's count to integer"),
                        };
                        Ok(Value::Decimal(d.round_places(places)?))
                    }
                    // ceil / ceiling / floor / trunc over decimal (and integer, promoted) — the
                    // EXACT-numeric overloads (decimal.md §6, functions.md §9). The float overloads
                    // fall through to the libm arm below (these guards exclude Float64). ceil/floor
                    // round to scale 0 toward ±∞ (a round-up carry can trap 22003); trunc truncates
                    // toward zero to scale 0 or its `n`-place argument (never overflows).
                    ScalarFunc::Ceil if !matches!(&vals[0], Value::Float64(_)) => {
                        let d = match &vals[0] {
                            Value::Int(n) => Decimal::from_i64(*n),
                            Value::Decimal(d) => d.clone(),
                            _ => unreachable!("resolver restricts ceil to numeric operands"),
                        };
                        Ok(Value::Decimal(d.ceil()?))
                    }
                    ScalarFunc::Floor if !matches!(&vals[0], Value::Float64(_)) => {
                        let d = match &vals[0] {
                            Value::Int(n) => Decimal::from_i64(*n),
                            Value::Decimal(d) => d.clone(),
                            _ => unreachable!("resolver restricts floor to numeric operands"),
                        };
                        Ok(Value::Decimal(d.floor()?))
                    }
                    ScalarFunc::Trunc if !matches!(&vals[0], Value::Float64(_)) => {
                        let d = match &vals[0] {
                            Value::Int(n) => Decimal::from_i64(*n),
                            Value::Decimal(d) => d.clone(),
                            _ => unreachable!("resolver restricts trunc to numeric operands"),
                        };
                        let places = match vals.get(1) {
                            None => 0,
                            Some(Value::Int(k)) => *k,
                            Some(_) => unreachable!("resolver restricts trunc's count to integer"),
                        };
                        Ok(Value::Decimal(d.trunc_places(places)))
                    }
                    // EXACT-numeric transcendentals over decimal (decimal.md §8): sqrt / exp / ln /
                    // log / log10 / power. A hand-rolled PG-faithful arbitrary-precision port —
                    // byte-identical across cores by construction (unlike the libm float arms below,
                    // which ride the `R`-tag ULP exemption). Guarded on a Decimal operand so the
                    // float overloads still reach the libm arm. Domain errors: sqrt of a negative and
                    // the power domain errors → 2201F; ln/log of a non-positive → 2201E; exp/power
                    // overflow → 22003.
                    ScalarFunc::Sqrt if matches!(&vals[0], Value::Decimal(_)) => {
                        let Value::Decimal(d) = &vals[0] else {
                            unreachable!()
                        };
                        Ok(Value::Decimal(d.dec_sqrt_metered(Some(m))?))
                    }
                    ScalarFunc::Exp if matches!(&vals[0], Value::Decimal(_)) => {
                        let Value::Decimal(d) = &vals[0] else {
                            unreachable!()
                        };
                        Ok(Value::Decimal(d.dec_exp_metered(Some(m))?))
                    }
                    ScalarFunc::Ln if matches!(&vals[0], Value::Decimal(_)) => {
                        let Value::Decimal(d) = &vals[0] else {
                            unreachable!()
                        };
                        Ok(Value::Decimal(d.dec_ln_metered(Some(m))?))
                    }
                    ScalarFunc::Log10 if matches!(&vals[0], Value::Decimal(_)) => {
                        let Value::Decimal(d) = &vals[0] else {
                            unreachable!()
                        };
                        Ok(Value::Decimal(d.dec_log10_metered(Some(m))?))
                    }
                    // `log` is decimal-only (no float `log` in the catalog): 1-arg = base-10 log,
                    // 2-arg = log(base, num) in an arbitrary base. A promoted integer argument
                    // converts to decimal (functions.md §9.2).
                    ScalarFunc::Log => {
                        let a = value_to_decimal(&vals[0]);
                        match vals.get(1) {
                            None => Ok(Value::Decimal(a.dec_log10_metered(Some(m))?)),
                            Some(num) => Ok(Value::Decimal(Decimal::dec_log_metered(
                                &a,
                                &value_to_decimal(num),
                                Some(m),
                            )?)),
                        }
                    }
                    // The decimal power: the overload decides the kernel, and a promoted integer
                    // argument converts to decimal (functions.md §9.2).
                    ScalarFunc::Pow if *result == ScalarType::Decimal => {
                        Ok(Value::Decimal(Decimal::dec_power_metered(
                            &value_to_decimal(&vals[0]),
                            &value_to_decimal(&vals[1]),
                            Some(m),
                        )?))
                    }
                    // pi() — the constant π, no operand (float.md §8). In-contract: the same f64
                    // literal in every core.
                    ScalarFunc::Pi => Ok(Value::Float64(std::f64::consts::PI)),
                    // The other float functions all take a single f64 arg (the resolver widened
                    // it) and return f64 (spec/design/float.md §8). EXACT (in-contract):
                    // ceil/floor/trunc/sqrt. sqrt of a negative is a DOMAIN error → 22003 (NaN stays
                    // input-only). TRANSCENDENTAL (exempted — native libm): exp/ln/log10/pow/sin/
                    // cos/tan; ln(0)/ln(neg) → 22003, exp/pow overflow → 22003.
                    ScalarFunc::Ceil
                    | ScalarFunc::Floor
                    | ScalarFunc::Trunc
                    | ScalarFunc::Sqrt
                    | ScalarFunc::Exp
                    | ScalarFunc::Ln
                    | ScalarFunc::Log10
                    | ScalarFunc::Pow
                    | ScalarFunc::Sin
                    | ScalarFunc::Cos
                    | ScalarFunc::Tan
                    | ScalarFunc::Cbrt
                    | ScalarFunc::Radians
                    | ScalarFunc::Degrees
                    | ScalarFunc::Asin
                    | ScalarFunc::Acos
                    | ScalarFunc::Atan
                    | ScalarFunc::Atan2
                    | ScalarFunc::Cot
                    | ScalarFunc::Sinh
                    | ScalarFunc::Cosh
                    | ScalarFunc::Tanh
                    | ScalarFunc::Asinh
                    | ScalarFunc::Acosh
                    | ScalarFunc::Atanh => {
                        let x = match &vals[0] {
                            Value::Float64(f) => *f,
                            _ => unreachable!("resolver widens a float function arg to f64"),
                        };
                        eval_float_func(*func, x, vals.get(1))
                    }
                    // make_interval — six integer components plus the f64 `secs`. years/
                    // months → months field (×12), weeks/days → days field (×7), hours/mins/secs
                    // → micros; an i32/i64 field overflow traps 22008 (functions.md §11). The one
                    // float step (secs → micros) is correctly-rounded + deterministic, so the
                    // resulting interval is in-contract (not an `R`-exempt float).
                    ScalarFunc::MakeInterval => {
                        let geti = |k: usize| match &vals[k] {
                            Value::Int(n) => *n,
                            _ => unreachable!(
                                "resolver restricts make_interval's components to integers"
                            ),
                        };
                        let secs = match &vals[6] {
                            Value::Float64(f) => *f,
                            // f32 widens losslessly to f64 (every binary32 is an exact binary64).
                            Value::Float32(f) => *f as f64,
                            _ => unreachable!("resolver restricts make_interval's secs to a float"),
                        };
                        let sec_micros = f64_to_micros(secs)?;
                        let iv = interval::make_interval(
                            geti(0),
                            geti(1),
                            geti(2),
                            geti(3),
                            geti(4),
                            geti(5),
                            sec_micros,
                        )?;
                        Ok(Value::Interval(iv))
                    }
                    // make_timestamp / make_timestamptz — the make_interval siblings (functions.md
                    // §11). Assemble the wall clock from the five integer fields + the f64 `sec`
                    // (an out-of-range field traps 22008). make_timestamptz then interprets that
                    // wall clock in a zone (session zone for the 6-arg form, the trailing `timezone`
                    // text for the 7-arg form), charging one `timezone` unit like AT TIME ZONE; an
                    // unrecognized explicit zone is 22023.
                    ScalarFunc::MakeTimestamp | ScalarFunc::MakeTimestamptz => {
                        let geti = |k: usize| match &vals[k] {
                            Value::Int(n) => *n,
                            _ => unreachable!(
                                "resolver restricts make_timestamp's date/time fields to integers"
                            ),
                        };
                        let sec = match &vals[5] {
                            Value::Float64(f) => *f,
                            // f32 widens losslessly to f64 (every binary32 is an exact binary64).
                            Value::Float32(f) => *f as f64,
                            _ => unreachable!("resolver restricts make_timestamp's sec to a float"),
                        };
                        let wall = crate::timestamp::make_timestamp(
                            geti(0),
                            geti(1),
                            geti(2),
                            geti(3),
                            geti(4),
                            sec,
                        )?;
                        if matches!(func, ScalarFunc::MakeTimestamp) {
                            return Ok(Value::Timestamp(wall));
                        }
                        // make_timestamptz: interpret the wall clock in a zone → a UTC instant.
                        m.charge(COSTS.timezone);
                        m.guard()?;
                        let zr = if vals.len() == 7 {
                            let zone_str = match &vals[6] {
                                Value::Text(s) => s.as_str(),
                                _ => unreachable!("resolver restricts the timezone arg to text"),
                            };
                            env.exec.session.env.resolve_zone(zone_str).ok_or_else(|| {
                                EngineError::new(
                                    SqlState::InvalidParameterValue,
                                    format!("time zone \"{zone_str}\" not recognized"),
                                )
                            })?
                        } else {
                            env.exec.session.time_zone.clone()
                        };
                        let instant = crate::timezone::local_to_instant_micros(&zr, wall)
                            .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
                        Ok(Value::Timestamptz(instant))
                    }
                    ScalarFunc::MakeDate => {
                        // make_date(year, month, day) — the make_timestamp sibling (functions.md
                        // §11): a negative year is BC; year zero / a bad field / an out-of-range
                        // day count traps 22008.
                        let geti = |k: usize| match &vals[k] {
                            Value::Int(n) => *n,
                            _ => unreachable!("resolver restricts make_date's fields to integers"),
                        };
                        Ok(Value::Date(crate::date::make_date(
                            geti(0),
                            geti(1),
                            geti(2),
                        )?))
                    }
                    ScalarFunc::CurrentDate => {
                        // CURRENT_DATE (functions.md §12, date.md §6): the statement clock's day
                        // in the session zone — the 'today' literal as a function. STABLE;
                        // date_clock_value charges the timezone unit.
                        date_clock_value(env.exec, env.rng, m, 0)
                    }
                    ScalarFunc::DatePart => {
                        // date_part(field, source) — the float8-returning EXTRACT twin
                        // (timezones.md §9.2): the shared extract kernel, then decimal → f64. The
                        // field is a RUNTIME text value (case-insensitive, validated here — 22023
                        // unrecognized / 0A000 unsupported-for-type, like date_trunc's unit). A
                        // date source WIDENS TO MIDNIGHT and the timestamp matrix applies (PG
                        // defines date_part(text, date) over ::timestamp — so 'hour' is 0 where
                        // EXTRACT over a date is 0A000); the widen traps 22008 past the timestamp
                        // range. A timestamptz source decomposes in the session zone with
                        // EXTRACT's exact selective timezone charge. NULL propagation is the
                        // blanket case above.
                        use crate::datetime_fn::ExtractSrc;
                        let field = match &vals[0] {
                            Value::Text(s) => s.to_ascii_lowercase(),
                            _ => unreachable!("resolver restricts date_part's field to text"),
                        };
                        let src = match &vals[1] {
                            Value::Date(d) => ExtractSrc::Timestamp(date_midnight_micros(*d)?),
                            Value::Timestamp(mc) => ExtractSrc::Timestamp(*mc),
                            Value::Interval(iv) => ExtractSrc::Interval(*iv),
                            Value::Timestamptz(mc) => {
                                let mc = *mc;
                                if field == "epoch"
                                    || mc == crate::timestamp::POS_INFINITY
                                    || mc == crate::timestamp::NEG_INFINITY
                                {
                                    ExtractSrc::Timestamptz {
                                        instant: mc,
                                        local: mc,
                                        offset_secs: 0,
                                    }
                                } else {
                                    let zr = env.exec.session.time_zone.clone();
                                    m.charge(COSTS.timezone);
                                    m.guard()?;
                                    let local = crate::timezone::instant_to_local_micros(&zr, mc)
                                        .ok_or_else(
                                        crate::timestamp::zone_shift_out_of_range,
                                    )?;
                                    let off = crate::timezone::offset_at_ref(
                                        &zr,
                                        mc.div_euclid(1_000_000),
                                    )
                                    .utoff;
                                    ExtractSrc::Timestamptz {
                                        instant: mc,
                                        local,
                                        offset_secs: off as i64,
                                    }
                                }
                            }
                            _ => unreachable!(
                                "resolver restricts date_part to date/ts/tstz/interval"
                            ),
                        };
                        if crate::datetime_fn::canonical_field(&field) == Some("julian") {
                            // PG's float8 julian formula, not the numeric value converted
                            // (timezones.md §9.2).
                            match src {
                                ExtractSrc::Timestamp(local)
                                | ExtractSrc::Timestamptz { local, .. }
                                    if local != crate::timestamp::POS_INFINITY
                                        && local != crate::timestamp::NEG_INFINITY =>
                                {
                                    return Ok(Value::Float64(
                                        crate::datetime_fn::date_part_julian(local),
                                    ));
                                }
                                _ => {}
                            }
                        }
                        // Over a ±infinity source PG's float8 result carries what EXTRACT's
                        // numeric cannot: ±Infinity for a monotonic field, NULL for an
                        // oscillating one (timezones.md §9.2).
                        match crate::datetime_fn::extract_part(&field, src)? {
                            crate::datetime_fn::ExtractOut::Value(d) => {
                                decimal_to_float(&d, ScalarType::Float64)
                            }
                            crate::datetime_fn::ExtractOut::Null => Ok(Value::Null),
                            crate::datetime_fn::ExtractOut::Infinite { negative } => {
                                Ok(Value::Float64(if negative {
                                    f64::NEG_INFINITY
                                } else {
                                    f64::INFINITY
                                }))
                            }
                        }
                    }
                    // interval normalizers (spec/design/interval.md §5.1). The NULL-input case is
                    // already handled above.
                    ScalarFunc::JustifyHours
                    | ScalarFunc::JustifyDays
                    | ScalarFunc::JustifyInterval => match &vals[0] {
                        Value::Interval(iv) => Ok(Value::Interval(match func {
                            ScalarFunc::JustifyHours => crate::interval::justify_hours(iv)?,
                            ScalarFunc::JustifyDays => crate::interval::justify_days(iv)?,
                            _ => crate::interval::justify_interval(iv)?,
                        })),
                        _ => unreachable!("resolver restricts justify_* to an interval"),
                    },
                    // age (spec/design/interval.md §5.2). The one-argument form is PG's SQL
                    // wrapper age(CURRENT_DATE::timestamp[tz], x): the statement clock's day in
                    // the session zone (date_clock_value charges one timezone unit), widened to
                    // midnight — for timestamptz, midnight read in the session zone (one more
                    // timezone unit). A timestamptz pair then decomposes each finite operand in
                    // the session zone (one timezone unit each); any ±infinity operand traps
                    // 22008 before any decomposition. The NULL-input case is already handled above.
                    ScalarFunc::Age => {
                        let (dt1, x) = if vals.len() == 1 {
                            let today = match date_clock_value(env.exec, env.rng, m, 0)? {
                                Value::Date(d) => d,
                                _ => unreachable!("date_clock_value returns a date"),
                            };
                            let mid = date_midnight_micros(today)?;
                            let anchor = match &vals[0] {
                                Value::Timestamptz(_) => {
                                    m.charge(COSTS.timezone);
                                    m.guard()?;
                                    crate::timezone::local_to_instant_micros(
                                        &env.exec.session.time_zone,
                                        mid,
                                    )
                                    .ok_or_else(crate::timestamp::zone_shift_out_of_range)?
                                }
                                _ => mid,
                            };
                            (anchor, &vals[0])
                        } else {
                            let dt1 = match &vals[0] {
                                Value::Timestamp(t) | Value::Timestamptz(t) => *t,
                                _ => unreachable!("resolver restricts age to timestamp[tz]"),
                            };
                            (dt1, &vals[1])
                        };
                        let (dt2, tz) = match x {
                            Value::Timestamp(t) => (*t, false),
                            Value::Timestamptz(t) => (*t, true),
                            _ => unreachable!("resolver restricts age to timestamp[tz]"),
                        };
                        crate::interval::age_reject_infinite(dt1, dt2)?;
                        let (local1, local2) = if tz {
                            let zr = env.exec.session.time_zone.clone();
                            let mut local = |t: i64| -> Result<i64> {
                                m.charge(COSTS.timezone);
                                m.guard()?;
                                crate::timezone::instant_to_local_micros(&zr, t)
                                    .ok_or_else(crate::timestamp::zone_shift_out_of_range)
                            };
                            (local(dt1)?, local(dt2)?)
                        } else {
                            (dt1, dt2)
                        };
                        Ok(Value::Interval(crate::interval::age(
                            dt1, dt2, local1, local2,
                        )?))
                    }
                    // uuid extractors (spec/design/functions.md §12): pure bit inspection. Both
                    // return NULL (Value::Null) for a non-RFC variant; the timestamp also for any
                    // version other than 1/7. The NULL-input case is already handled above.
                    ScalarFunc::UuidExtractVersion => match &vals[0] {
                        Value::Uuid(b) => {
                            Ok(crate::uuid::extract_version(b).map_or(Value::Null, Value::Int))
                        }
                        _ => unreachable!("resolver restricts uuid_extract_version to a uuid"),
                    },
                    ScalarFunc::UuidExtractTimestamp => match &vals[0] {
                        Value::Uuid(b) => Ok(crate::uuid::extract_timestamp_micros(b)
                            .map_or(Value::Null, Value::Timestamptz)),
                        _ => unreachable!("resolver restricts uuid_extract_timestamp to a uuid"),
                    },
                    // uuid generators (spec/design/entropy.md §3): draw from the per-statement seam
                    // (a Cell on EvalEnv — interior mutability), advancing the PRNG/counter. The
                    // NULL-arg case (uuidv7(NULL)) already returned NULL above.
                    ScalarFunc::Uuidv4 => {
                        let mut r = env.rng.get();
                        let b = r.uuid_v4(&env.exec.session.env)?;
                        env.rng.set(r);
                        Ok(Value::Uuid(b))
                    }
                    ScalarFunc::Uuidv7 => {
                        let mut r = env.rng.get();
                        let clock = r.statement_clock_micros(&env.exec.session.env);
                        // The optional interval arg shifts the embedded instant via the existing
                        // calendar-aware timestamptz arithmetic (entropy.md §4).
                        let shifted = match vals.first() {
                            Some(Value::Interval(iv)) => {
                                crate::interval::ts_shift(clock, iv, false)?
                            }
                            Some(_) => {
                                unreachable!("resolver restricts uuidv7's arg to an interval")
                            }
                            None => clock,
                        };
                        let b = r.uuid_v7(&env.exec.session.env, shifted)?;
                        env.rng.set(r);
                        Ok(Value::Uuid(b))
                    }
                    // current-time functions (spec/design/entropy.md §5): now() reads the statement
                    // clock ONCE and reuses it (STABLE); clock_timestamp() reads the seam on every
                    // call (VOLATILE). Both return the seam's micros directly as timestamptz.
                    ScalarFunc::Now => {
                        let mut r = env.rng.get();
                        let micros = r.statement_clock_micros(&env.exec.session.env);
                        env.rng.set(r);
                        Ok(Value::Timestamptz(micros))
                    }
                    ScalarFunc::ClockTimestamp => {
                        let r = env.rng.get();
                        let micros = r.clock_now_micros(&env.exec.session.env);
                        Ok(Value::Timestamptz(micros))
                    }
                    // Sequence value functions (spec/design/sequences.md §4/§6). nextval charges an
                    // additional sequence_advance unit (the catalog-tuple read+rewrite) and mutates
                    // the per-statement pending state; currval is a pure session-state read.
                    ScalarFunc::Nextval => {
                        m.charge(COSTS.sequence_advance);
                        let name = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts nextval's argument to text"),
                        };
                        // The text names the sequence as one identifier (grammar.md §3).
                        let name = &crate::parser::parse_name_text(name)?;
                        Ok(Value::Int(env.exec.seq_nextval(name)?))
                    }
                    ScalarFunc::Currval => {
                        let name = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts currval's argument to text"),
                        };
                        let name = &crate::parser::parse_name_text(name)?;
                        Ok(Value::Int(env.exec.seq_currval(name)?))
                    }
                    // setval charges sequence_advance (it rewrites the catalog tuple, like nextval).
                    // Arity 2 → is_called defaults true; arity 3 → the boolean third argument.
                    ScalarFunc::Setval => {
                        m.charge(COSTS.sequence_advance);
                        let name = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts setval's first argument to text"),
                        };
                        let name = &crate::parser::parse_name_text(name)?;
                        let n = match &vals[1] {
                            Value::Int(n) => *n,
                            _ => unreachable!("resolver restricts setval's value to integer"),
                        };
                        let is_called = match vals.get(2) {
                            None => true,
                            Some(Value::Bool(b)) => *b,
                            Some(_) => {
                                unreachable!(
                                    "resolver restricts setval's third argument to boolean"
                                )
                            }
                        };
                        Ok(Value::Int(env.exec.seq_setval(name, n, is_called)?))
                    }
                    ScalarFunc::Lastval => Ok(Value::Int(env.exec.seq_lastval()?)),
                    // current_setting (spec/design/session.md §6.1): read the named session variable
                    // from the session's variable map. The blanket NULL short-circuit above already
                    // returned NULL for a NULL name / missing_ok argument, so both are non-NULL here.
                    // An unset name is 42704 UNLESS the two-arg overload's missing_ok is true (→ NULL).
                    ScalarFunc::CurrentSetting => {
                        let name = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts current_setting's name to text"),
                        };
                        let missing_ok = match vals.get(1) {
                            None => false,
                            Some(Value::Bool(b)) => *b,
                            Some(_) => unreachable!(
                                "resolver restricts current_setting's missing_ok to boolean"
                            ),
                        };
                        match env.exec.session.vars.get(&name.to_ascii_lowercase()) {
                            Some(v) => Ok(Value::Text(v.clone())),
                            None if missing_ok => Ok(Value::Null),
                            None => Err(EngineError::new(
                                SqlState::UndefinedObject,
                                format!("unrecognized configuration parameter: {name}"),
                            )),
                        }
                    }
                    // json/jsonb processing functions (B1). A jsonb arg is the node directly; a json
                    // arg is parsed from its verbatim text on demand (json.md §4), then dispatched to
                    // the same kernel.
                    ScalarFunc::JsonbTypeof | ScalarFunc::JsonTypeof => {
                        let node = json_arg_shape(&vals[0])?;
                        Ok(Value::Text(json::typeof_name(&node).to_string()))
                    }
                    ScalarFunc::JsonbArrayLength | ScalarFunc::JsonArrayLength => {
                        let node = json_arg_shape(&vals[0])?;
                        Ok(Value::Int(json::array_length(&node)?))
                    }
                    ScalarFunc::JsonbStripNulls => {
                        let node = json_arg_node(&vals[0])?;
                        Ok(Value::Jsonb(json::strip_nulls(&node)))
                    }
                    ScalarFunc::JsonStripNulls => {
                        // json_strip_nulls returns json — render the stripped tree COMPACTLY (PG's
                        // json output style), preserving the on-demand parse's key order.
                        let node = json_arg_node(&vals[0])?;
                        Ok(Value::Json(json::json_compact_out(&json::strip_nulls(
                            &node,
                        ))))
                    }
                    ScalarFunc::JsonbPretty => Ok(Value::Text(jsonb_pretty_text(&vals[0], m)?)),
                    // JSON_SERIALIZE(v) → the value's text serialization: json verbatim, jsonb canonical.
                    ScalarFunc::JsonSerialize => {
                        let out = Value::Text(match &vals[0] {
                            Value::Json(s) => s.clone(),
                            Value::Jsonb(n) => json::jsonb_out(n),
                            _ => unreachable!("resolver restricts JSON_SERIALIZE to json/jsonb"),
                        });
                        // A growth kernel (cost.md §8.1), free over constant operands.
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // length(text) → i32 — the number of characters (Unicode code points). Rust
                    // String is UTF-8, so `chars()` yields one item per code point (string-functions.md §3).
                    // Over bytea, length is the byte count (string-functions.md §4).
                    ScalarFunc::Length => match &vals[0] {
                        Value::Text(s) => Ok(Value::Int(s.chars().count() as i64)),
                        Value::Bytea(b) => Ok(Value::Int(b.len() as i64)),
                        _ => unreachable!("resolver restricts length to text/bytea"),
                    },
                    // octet_length(text) → i32 — the UTF-8 byte count (`len()` of the String's
                    // bytes), distinct from length's code-point count (string-functions.md §3).
                    ScalarFunc::OctetLength => match &vals[0] {
                        Value::Text(s) => Ok(Value::Int(s.len() as i64)),
                        Value::Bytea(b) => Ok(Value::Int(b.len() as i64)),
                        _ => unreachable!("resolver restricts octet_length to text/bytea"),
                    },
                    // bit_length(text) → i32 — the UTF-8 bit count = byte count × 8.
                    ScalarFunc::BitLength => match &vals[0] {
                        Value::Text(s) => Ok(Value::Int(s.len() as i64 * 8)),
                        Value::Bytea(b) => Ok(Value::Int(b.len() as i64 * 8)),
                        _ => unreachable!("resolver restricts bit_length to text/bytea"),
                    },
                    // substr / substring(text, start[, count]) → text — the function form of
                    // SUBSTRING; over bytea the same window over bytes (string-functions.md §4).
                    ScalarFunc::Substr => {
                        let start = int_value(&vals[1]);
                        let count = vals.get(2).map(int_value);
                        match &vals[0] {
                            Value::Text(s) => Ok(Value::Text(substr_chars(s, start, count)?)),
                            Value::Bytea(b) => Ok(Value::Bytea(substr_bytes(b, start, count)?)),
                            _ => unreachable!("resolver restricts substr to text/bytea"),
                        }
                    }
                    // left(text, n) → text — the first n characters (negative n drops the last |n|).
                    ScalarFunc::Left => match &vals[0] {
                        Value::Text(s) => Ok(Value::Text(left_chars(s, int_value(&vals[1])))),
                        _ => unreachable!("resolver restricts left to text"),
                    },
                    // right(text, n) → text — the last n characters (negative n drops the first |n|).
                    ScalarFunc::Right => match &vals[0] {
                        Value::Text(s) => Ok(Value::Text(right_chars(s, int_value(&vals[1])))),
                        _ => unreachable!("resolver restricts right to text"),
                    },
                    // lpad(text, length[, fill]) → text — pad/truncate on the LEFT.
                    ScalarFunc::Lpad => {
                        let s = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts lpad to text"),
                        };
                        let len = int_value(&vals[1]);
                        let fill = match vals.get(2) {
                            Some(Value::Text(f)) => f.as_str(),
                            Some(_) => unreachable!("resolver restricts lpad fill to text"),
                            None => " ",
                        };
                        Ok(Value::Text(pad_chars(s, len, fill, true, m)?))
                    }
                    // rpad(text, length[, fill]) → text — pad/truncate on the RIGHT.
                    ScalarFunc::Rpad => {
                        let s = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts rpad to text"),
                        };
                        let len = int_value(&vals[1]);
                        let fill = match vals.get(2) {
                            Some(Value::Text(f)) => f.as_str(),
                            Some(_) => unreachable!("resolver restricts rpad fill to text"),
                            None => " ",
                        };
                        Ok(Value::Text(pad_chars(s, len, fill, false, m)?))
                    }
                    // btrim(text[, chars]) → text — trim `chars`-set characters from both ends.
                    ScalarFunc::Btrim => {
                        // btrim(bytea, bytes) → bytea — the same trim over a byte set (§4).
                        if let (Value::Bytea(b), Some(Value::Bytea(set))) = (&vals[0], vals.get(1))
                        {
                            return Ok(Value::Bytea(trim_bytes(b, set, true, true)));
                        }
                        let s = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts btrim to text/bytea"),
                        };
                        let set = match vals.get(1) {
                            Some(Value::Text(c)) => c.as_str(),
                            Some(_) => unreachable!("resolver restricts btrim chars to text"),
                            None => " ",
                        };
                        Ok(Value::Text(trim_chars(s, set, true, true)))
                    }
                    // ltrim(text[, chars]) → text — trim `chars`-set characters from the LEFT end.
                    ScalarFunc::Ltrim => {
                        // ltrim(bytea, bytes) → bytea — the same trim over a byte set (§4).
                        if let (Value::Bytea(b), Some(Value::Bytea(set))) = (&vals[0], vals.get(1))
                        {
                            return Ok(Value::Bytea(trim_bytes(b, set, true, false)));
                        }
                        let s = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts ltrim to text/bytea"),
                        };
                        let set = match vals.get(1) {
                            Some(Value::Text(c)) => c.as_str(),
                            Some(_) => unreachable!("resolver restricts ltrim chars to text"),
                            None => " ",
                        };
                        Ok(Value::Text(trim_chars(s, set, true, false)))
                    }
                    // rtrim(text[, chars]) → text — trim `chars`-set characters from the RIGHT end.
                    ScalarFunc::Rtrim => {
                        // rtrim(bytea, bytes) → bytea — the same trim over a byte set (§4).
                        if let (Value::Bytea(b), Some(Value::Bytea(set))) = (&vals[0], vals.get(1))
                        {
                            return Ok(Value::Bytea(trim_bytes(b, set, false, true)));
                        }
                        let s = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts rtrim to text/bytea"),
                        };
                        let set = match vals.get(1) {
                            Some(Value::Text(c)) => c.as_str(),
                            Some(_) => unreachable!("resolver restricts rtrim chars to text"),
                            None => " ",
                        };
                        Ok(Value::Text(trim_chars(s, set, false, true)))
                    }
                    // replace(text, from, to) → text — substring replace-all; empty `from` is a no-op.
                    ScalarFunc::Replace => {
                        let (s, from, to) = match (&vals[0], &vals[1], &vals[2]) {
                            (Value::Text(s), Value::Text(f), Value::Text(t)) => (s, f, t),
                            _ => unreachable!("resolver restricts replace to text"),
                        };
                        Ok(Value::Text(replace_text(s, from, to, m)?))
                    }
                    // translate(text, from, to) → text — per-character map/delete.
                    ScalarFunc::Translate => {
                        let (s, from, to) = match (&vals[0], &vals[1], &vals[2]) {
                            (Value::Text(s), Value::Text(f), Value::Text(t)) => (s, f, t),
                            _ => unreachable!("resolver restricts translate to text"),
                        };
                        Ok(Value::Text(translate_chars(s, from, to)))
                    }
                    // repeat(text, n) → text — concatenate the string n times.
                    ScalarFunc::Repeat => {
                        let s = match &vals[0] {
                            Value::Text(s) => s,
                            _ => unreachable!("resolver restricts repeat to text"),
                        };
                        Ok(Value::Text(repeat_text(s, int_value(&vals[1]), m)?))
                    }
                    // reverse(text) → text — the code points in reverse order.
                    ScalarFunc::Reverse => match &vals[0] {
                        Value::Text(s) => Ok(Value::Text(s.chars().rev().collect())),
                        _ => unreachable!("resolver restricts reverse to text"),
                    },
                    // strpos(text, substring) → i32 — 1-based code-point position, else 0.
                    // position(string, substring) shares this kernel; over bytea it is the 1-based
                    // byte position (§4).
                    ScalarFunc::Strpos => {
                        let (s, sub) = match (&vals[0], &vals[1]) {
                            (Value::Text(s), Value::Text(sub)) => (s, sub),
                            (Value::Bytea(b), Value::Bytea(sub)) => {
                                return Ok(Value::Int(position_bytes(b, sub)));
                            }
                            _ => unreachable!("resolver restricts strpos/position to text/bytea"),
                        };
                        // find returns a BYTE offset; convert to a 1-based CODE-POINT position by
                        // counting the code points in the prefix (empty substring → byte 0 → 1).
                        Ok(Value::Int(match s.find(sub.as_str()) {
                            Some(b) => s[..b].chars().count() as i64 + 1,
                            None => 0,
                        }))
                    }
                    // split_part(text, delimiter, n) → text — the n-th split field.
                    ScalarFunc::SplitPart => {
                        let (s, delim) = match (&vals[0], &vals[1]) {
                            (Value::Text(s), Value::Text(d)) => (s, d),
                            _ => unreachable!("resolver restricts split_part to text"),
                        };
                        Ok(Value::Text(split_part(s, delim, int_value(&vals[2]))?))
                    }
                    // starts_with(text, prefix) → boolean — string begins with prefix.
                    ScalarFunc::StartsWith => match (&vals[0], &vals[1]) {
                        (Value::Text(s), Value::Text(pfx)) => {
                            Ok(Value::Bool(s.starts_with(pfx.as_str())))
                        }
                        _ => unreachable!("resolver restricts starts_with to text"),
                    },
                    // ascii(text) → i32 — the code point of the first character (empty → 0).
                    ScalarFunc::Ascii => match &vals[0] {
                        Value::Text(s) => Ok(Value::Int(s.chars().next().map_or(0, |c| c as i64))),
                        _ => unreachable!("resolver restricts ascii to text"),
                    },
                    // chr(int) → text — the one-character string for a code point.
                    ScalarFunc::Chr => Ok(Value::Text(chr_text(int_value(&vals[0]))?)),
                    // initcap(text) → text — titlecase each word.
                    ScalarFunc::Initcap => match &vals[0] {
                        Value::Text(s) => Ok(Value::Text(initcap_ascii(s))),
                        _ => unreachable!("resolver restricts initcap to text"),
                    },
                    // to_hex(int) → text — lowercase hex of the 64-bit two's-complement pattern.
                    ScalarFunc::ToHex => {
                        Ok(Value::Text(format!("{:x}", int_value(&vals[0]) as u64)))
                    }
                    // encode(bytea, format) → text — hex / base64 / escape rendering.
                    ScalarFunc::Encode => {
                        let (bytes, fmt) = match (&vals[0], &vals[1]) {
                            (Value::Bytea(b), Value::Text(f)) => (b, f),
                            _ => unreachable!("resolver restricts encode to (bytea, text)"),
                        };
                        let out = Value::Text(encode_bytea(bytes, fmt)?);
                        // A growth kernel: hex doubles its input (cost.md §8.1).
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // decode(text, format) → bytea — parse hex / base64 / escape back to bytes.
                    ScalarFunc::Decode => {
                        let (s, fmt) = match (&vals[0], &vals[1]) {
                            (Value::Text(s), Value::Text(f)) => (s, f),
                            _ => unreachable!("resolver restricts decode to (text, text)"),
                        };
                        Ok(Value::Bytea(decode_text(s, fmt)?))
                    }
                    // get_byte(bytea, n) → i32 — the n-th byte (0-based), unsigned.
                    ScalarFunc::GetByte => match &vals[0] {
                        Value::Bytea(b) => Ok(Value::Int(get_byte(b, int_value(&vals[1]))?)),
                        _ => unreachable!("resolver restricts get_byte to (bytea, int)"),
                    },
                    // set_byte(bytea, n, v) → bytea — the n-th byte replaced by v's low 8 bits.
                    ScalarFunc::SetByte => match &vals[0] {
                        Value::Bytea(b) => Ok(Value::Bytea(set_byte(
                            b,
                            int_value(&vals[1]),
                            int_value(&vals[2]),
                        )?)),
                        _ => unreachable!("resolver restricts set_byte to (bytea, int, int)"),
                    },
                    // get_bit(bytea, n) → i32 — the n-th bit (0-based), 0 or 1.
                    ScalarFunc::GetBit => match &vals[0] {
                        Value::Bytea(b) => Ok(Value::Int(get_bit(b, int_value(&vals[1]))?)),
                        _ => unreachable!("resolver restricts get_bit to (bytea, int)"),
                    },
                    // set_bit(bytea, n, v) → bytea — the n-th bit set to v (0 or 1).
                    ScalarFunc::SetBit => match &vals[0] {
                        Value::Bytea(b) => Ok(Value::Bytea(set_bit(
                            b,
                            int_value(&vals[1]),
                            int_value(&vals[2]),
                        )?)),
                        _ => unreachable!("resolver restricts set_bit to (bytea, int, int)"),
                    },
                    // md5(text | bytea) → text — 32 lowercase hex digits; text hashes its UTF-8.
                    ScalarFunc::Md5 => match &vals[0] {
                        Value::Text(s) => Ok(Value::Text(md5_hex(s.as_bytes()))),
                        Value::Bytea(b) => Ok(Value::Text(md5_hex(b))),
                        _ => unreachable!("resolver restricts md5 to text/bytea"),
                    },
                    // sha224/sha256/sha384/sha512(bytea) → bytea — the raw SHA-2 digest.
                    ScalarFunc::Sha224
                    | ScalarFunc::Sha256
                    | ScalarFunc::Sha384
                    | ScalarFunc::Sha512 => match &vals[0] {
                        Value::Bytea(b) => Ok(Value::Bytea(sha2_digest(*func, b))),
                        _ => unreachable!("resolver restricts the sha2 functions to bytea"),
                    },
                    // overlay(string, new, start[, count]) → text / bytea — the SQL OVERLAY splice;
                    // count defaults to length(new) (characters for text, bytes for bytea). A growth
                    // kernel (cost.md §8.1), free over constant operands.
                    ScalarFunc::Overlay => {
                        let start = int_value(&vals[2]);
                        let out = match (&vals[0], &vals[1]) {
                            (Value::Text(s), Value::Text(r)) => {
                                let count = match vals.get(3) {
                                    Some(c) => int_value(c),
                                    None => r.chars().count() as i64,
                                };
                                Value::Text(overlay_chars(s, r, start, count)?)
                            }
                            (Value::Bytea(b), Value::Bytea(r)) => {
                                let count = match vals.get(3) {
                                    Some(c) => int_value(c),
                                    None => r.len() as i64,
                                };
                                Value::Bytea(overlay_bytes(b, r, start, count)?)
                            }
                            _ => unreachable!("resolver restricts overlay to text/bytea"),
                        };
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // bytea || bytea → bytea. A growth kernel (cost.md §8.1), free over constant
                    // operands.
                    ScalarFunc::ByteaConcat => {
                        let out = match (&vals[0], &vals[1]) {
                            (Value::Bytea(a), Value::Bytea(b)) => {
                                let mut out = Vec::with_capacity(a.len() + b.len());
                                out.extend_from_slice(a);
                                out.extend_from_slice(b);
                                Value::Bytea(out)
                            }
                            _ => unreachable!("resolver restricts bytea || to (bytea, bytea)"),
                        };
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // text || text → text. A growth kernel (cost.md §8.1), free over constant
                    // operands.
                    ScalarFunc::TextConcat => {
                        let out = match (&vals[0], &vals[1]) {
                            (Value::Text(a), Value::Text(b)) => {
                                let mut out = String::with_capacity(a.len() + b.len());
                                out.push_str(a);
                                out.push_str(b);
                                Value::Text(out)
                            }
                            _ => unreachable!("resolver restricts text || to (text, text)"),
                        };
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // quote_literal(text) → text — wrap as a SQL string literal.
                    // quote_ident(text) → text — wrap as a SQL identifier. Both are growth kernels:
                    // quoting doubles every quote (cost.md §8.1).
                    ScalarFunc::QuoteLiteral | ScalarFunc::QuoteIdent => {
                        let s = match &vals[0] {
                            Value::Text(s) => s,
                            _ => {
                                unreachable!("resolver restricts quote_literal/quote_ident to text")
                            }
                        };
                        let out = Value::Text(if matches!(func, ScalarFunc::QuoteLiteral) {
                            quote_literal_text(s)
                        } else {
                            quote_ident_text(s)
                        });
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // like_escape(pattern, escape) → text — the `LIKE … ESCAPE` rewrite (grammar.md §22). A
                    // growth kernel like quote_literal: it can double backslashes, and nests.
                    ScalarFunc::LikeEscape => {
                        let (p, e) = match (&vals[0], &vals[1]) {
                            (Value::Text(p), Value::Text(e)) => (p, e),
                            _ => unreachable!("resolver restricts like_escape to text"),
                        };
                        let out = Value::Text(like_escape_text(p, e)?);
                        if !args.iter().all(rexpr_is_constant) {
                            charge_output(m, &out)?;
                        }
                        Ok(out)
                    }
                    // quote_nullable is handled by the non-strict pre-check above (it returns before
                    // the strict short-circuit loop), so it never reaches this match.
                    ScalarFunc::QuoteNullable => {
                        unreachable!("quote_nullable is handled by the non-strict pre-check")
                    }
                }
            }),
            // A polymorphic array function (spec/design/array-functions.md §3). One operator_eval
            // per call; arguments charge their own. NULL handling is per-kernel (the introspectors
            // propagate, the builders are non-strict), so — unlike `ScalarFunc` — there is no
            // blanket NULL short-circuit here.
            RExpr::ArrayFunc { func, args } => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(row, env, m)?);
                }
                // array_to_string renders its elements in the session zone (an amplifier that
                // charges itself, cost.md §8.1).
                if matches!(func, ArrayFunc::ArrayToString) {
                    return array_to_string_value(&vals, &env.exec.session.time_zone, m);
                }
                let out = eval_array_func(func, &vals, m)?;
                // The builders are growth kernels (cost.md §8.1), free over constant operands.
                if matches!(
                    func,
                    ArrayFunc::ArrayAppend | ArrayFunc::ArrayPrepend | ArrayFunc::ArrayCat
                ) && !args.iter().all(rexpr_is_constant)
                {
                    charge_output(m, &out)?;
                }
                Ok(out)
            }),
            RExpr::RangeFunc { func, args } => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(row, env, m)?);
                }
                eval_range_func(func, &vals)
            }),
            // A range CONSTRUCTOR call (spec/design/range-functions.md §2). One operator_eval (like
            // the range accessors); arguments charge their own evaluation. Non-strict — the kernel
            // turns a NULL bound into an infinite bound, so there is no blanket NULL short-circuit.
            RExpr::RangeCtor { elem, args } => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(row, env, m)?);
                }
                eval_range_ctor(*elem, &vals, &env.exec.input_zone())
            }),
            // A range BOOLEAN operator (spec/design/range-functions.md §3). One operator_eval; the
            // operands charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in
            // `eval_range_op`.
            RExpr::RangeOp { op, args, elem } => outline(|| {
                m.charge(COSTS.operator_eval);
                let l = args[0].eval(row, env, m)?;
                let r = args[1].eval(row, env, m)?;
                eval_range_op(*op, &l, &r, *elem, &env.exec.input_zone())
            }),
            // A range SET operator (spec/design/range-functions.md §4). One operator_eval; the
            // operands charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in
            // `eval_range_set_op`.
            RExpr::RangeSetOp { op, args } => outline(|| {
                m.charge(COSTS.operator_eval);
                let l = args[0].eval(row, env, m)?;
                let r = args[1].eval(row, env, m)?;
                eval_range_set_op(*op, &l, &r)
            }),
            // A VARIADIC argument-counting call (spec/design/array-functions.md §12). One
            // operator_eval (the per-element/arg count walk is unmetered, like the array
            // introspectors §3.3); arguments charge their own evaluation. Non-strict — no blanket
            // NULL short-circuit. The two forms differ: the spread form counts the args' null-ness
            // (never NULL); the VARIADIC-array form returns NULL on a NULL whole-array, else counts
            // the array's flattened elements' null-ness.
            RExpr::Variadic {
                func,
                args,
                array_form,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                if matches!(
                    func,
                    VariadicFunc::Concat | VariadicFunc::ConcatWs | VariadicFunc::Format
                ) {
                    return eval_text_variadic(func, args, *array_form, row, env, m);
                }
                let want_nulls = matches!(func, VariadicFunc::NumNulls);
                let count = if *array_form {
                    match args[0].eval(row, env, m)? {
                        Value::Null => return Ok(Value::Null),
                        Value::Array(a) => count_nulls(a.elements.iter(), want_nulls),
                        _ => unreachable!("resolver restricts a VARIADIC operand to an array"),
                    }
                } else {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(a.eval(row, env, m)?);
                    }
                    count_nulls(vals.iter(), want_nulls)
                };
                Ok(Value::Int(count as i64))
            }),
            // A JSON-image function (json-sql-functions.md §2.2): to_json / to_jsonb / array_to_json /
            // row_to_json / JSON_SCALAR. STRICT. A finite timestamptz at any depth charges one `timezone` unit,
            // charged and guarded before rendering (the array → text cast's rule, timezones.md §9.5).
            RExpr::JsonImage { func, args, ty } => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(row, env, m)?);
                }
                if vals.iter().any(|v| matches!(v, Value::Null)) {
                    return Ok(Value::Null);
                }
                let pretty = matches!(vals.get(1), Some(Value::Bool(true)));
                let zoned = vals[0].zone_render_count();
                if zoned > 0 {
                    m.charge(COSTS.timezone * zoned as i64);
                    m.guard()?;
                }
                let zone = &env.exec.session.time_zone;
                let out = if *func == JsonImageFunc::ToJsonb {
                    let node = datum_to_jsonb(&vals[0], ty, zone)?;
                    // An array or composite of json/jsonb nests them deeper (json.md §6.4).
                    json::check_depth(&node)?;
                    Value::Jsonb(node)
                } else {
                    let mut text = String::new();
                    match (func, &vals[0], ty) {
                        (
                            JsonImageFunc::RowToJson,
                            Value::Composite(fields),
                            ResolvedType::Composite(c),
                        ) => composite_to_json(fields, c, pretty, zone, &mut text)?,
                        (
                            JsonImageFunc::ArrayToJson,
                            Value::Array(a),
                            ResolvedType::Array(elem),
                        ) => array_to_json(a, elem, pretty, zone, &mut text)?,
                        (JsonImageFunc::ToJson | JsonImageFunc::JsonScalar, v, ty) => {
                            datum_to_json(v, ty, zone, &mut text)?
                        }
                        _ => unreachable!("resolver restricts the JSON-image argument types"),
                    }
                    // A jsonb element or field can nest a deep document (json.md §6.4).
                    json::check_text_depth(&text)?;
                    Value::Json(text)
                };
                // A growth kernel (cost.md §8.1), free over constant operands.
                if !args.iter().all(rexpr_is_constant) {
                    charge_output(m, &out)?;
                }
                Ok(out)
            }),
            // A VARIADIC json/jsonb builder (json-sql-functions.md §2). Gather the argument values
            // (the spread form directly; the VARIADIC-array form spreads the lone array — a NULL
            // array → NULL), then build an array / object node.
            RExpr::JsonBuild {
                kind,
                json,
                args,
                array_form,
                tys,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let vals: Vec<Value> = if *array_form {
                    match args[0].eval(row, env, m)? {
                        Value::Null => return Ok(Value::Null),
                        Value::Array(a) => a.elements.clone(),
                        _ => unreachable!("resolver restricts a VARIADIC operand to an array"),
                    }
                } else {
                    let mut vs = Vec::with_capacity(args.len());
                    for a in args {
                        vs.push(a.eval(row, env, m)?);
                    }
                    vs
                };
                m.charge(COSTS.operator_eval * vals.len() as i64);
                // A finite timestamptz at any depth renders in the session zone: one `timezone` unit
                // each, charged before rendering (json-sql-functions.md §2.2).
                let zoned: u64 = vals.iter().map(Value::zone_render_count).sum();
                m.charge(COSTS.timezone * zoned as i64);
                m.guard()?;
                // Each argument's static type: the spread form's own, the VARIADIC-array form's element.
                let elem_ty = |i: usize| -> &ResolvedType {
                    if *array_form {
                        match &tys[0] {
                            ResolvedType::Array(e) => e,
                            _ => &ResolvedType::Null,
                        }
                    } else {
                        &tys[i]
                    }
                };
                let zone = &env.exec.session.time_zone;
                let out = match kind {
                    JsonBuildKind::Array => {
                        if *json {
                            // PG's json_build_array: each element's json image, joined with ", ".
                            let mut out = String::from("[");
                            for (i, v) in vals.iter().enumerate() {
                                if i > 0 {
                                    out.push_str(", ");
                                }
                                datum_to_json(v, elem_ty(i), zone, &mut out)?;
                            }
                            out.push(']');
                            // The builders nest their arguments one level (json.md §6.4).
                            json::check_text_depth(&out)?;
                            Value::Json(out)
                        } else {
                            let mut nodes = Vec::with_capacity(vals.len());
                            for (i, v) in vals.iter().enumerate() {
                                nodes.push(datum_to_jsonb(v, elem_ty(i), zone)?);
                            }
                            let out = JsonNode::Array(nodes);
                            json::check_depth(&out)?;
                            Value::Jsonb(out)
                        }
                    }
                    JsonBuildKind::Object => {
                        if vals.len() % 2 != 0 {
                            return Err(EngineError::new(
                                SqlState::InvalidParameterValue,
                                "argument list must have even number of elements",
                            ));
                        }
                        if *json {
                            // PG's json_build_object: `key : value` pairs joined with ", "; a key is
                            // always a JSON string. A NULL key is 22004 (the jsonb form's is 22023).
                            let mut out = String::from("{");
                            for (i, pair) in vals.chunks_exact(2).enumerate() {
                                if matches!(pair[0], Value::Null) {
                                    return Err(object_key_null());
                                }
                                if i > 0 {
                                    out.push_str(", ");
                                }
                                json::write_json_string(&json_key_text(&pair[0], zone)?, &mut out);
                                out.push_str(" : ");
                                datum_to_json(&pair[1], elem_ty(2 * i + 1), zone, &mut out)?;
                            }
                            out.push('}');
                            json::check_text_depth(&out)?;
                            Value::Json(out)
                        } else {
                            let mut members = Vec::with_capacity(vals.len() / 2);
                            for (i, pair) in vals.chunks_exact(2).enumerate() {
                                if matches!(pair[0], Value::Null) {
                                    return Err(EngineError::new(
                                        SqlState::InvalidParameterValue,
                                        format!("argument {}: key must not be null", 2 * i + 1),
                                    ));
                                }
                                let key = json_key_text(&pair[0], zone)?;
                                members.push((
                                    key,
                                    datum_to_jsonb(&pair[1], elem_ty(2 * i + 1), zone)?,
                                ));
                            }
                            let out = json::make_object(members);
                            json::check_depth(&out)?;
                            Value::Jsonb(out)
                        }
                    }
                };
                // A growth kernel (cost.md §8.1), free over constant operands.
                if !args.iter().all(rexpr_is_constant) {
                    charge_output(m, &out)?;
                }
                Ok(out)
            }),
            // A correlated subquery (spec/design/grammar.md §26): re-executed once per outer row.
            // Push the current row onto the outer-row stack, run the inner plan against it, fold
            // its accrued cost into this meter, plus one operator_eval for the node. (Uncorrelated
            // subqueries were folded to a constant / `InValues` before exec, so this is correlated.)
            RExpr::Subquery {
                plan,
                kind,
                lhs,
                negated,
                row_valued,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let mut child: Vec<&[Value]> = env.outer.to_vec();
                child.push(row);
                let r = env
                    .exec
                    .exec_hidden_query_plan(plan, &child, env.params, env.ctes)?;
                m.charge(r.cost);
                // The charged result is consumed by this expression and released once it has been
                // used (memory.md §5.3).
                let held = m.query.measure_rows(&r.rows);
                let out = match kind {
                    SubqueryKind::Scalar => {
                        if r.rows.len() > 1 {
                            return Err(EngineError::new(
                                SqlState::CardinalityViolation,
                                "more than one row returned by a subquery used as an expression",
                            ));
                        }
                        // 0 rows -> NULL (the static type was settled at resolve via the column
                        // type, so a cross-family comparison already errored at plan time).
                        Ok(r.rows
                            .into_iter()
                            .next()
                            .map(|row| subquery_row_value(row, *row_valued))
                            .unwrap_or(Value::Null))
                    }
                    // EXISTS ignores the select list entirely and is never NULL.
                    SubqueryKind::Exists => Ok(Value::Bool(!r.rows.is_empty() != *negated)),
                    SubqueryKind::In => {
                        let lv = lhs
                            .as_ref()
                            .expect("an IN subquery carries its resolved lhs")
                            .eval(row, env, m)?;
                        let list: Vec<Value> = r
                            .rows
                            .into_iter()
                            .map(|row| subquery_row_value(row, *row_valued))
                            .collect();
                        in_membership(&lv, &list, *negated, m)
                    }
                    // A correlated quantified subquery (array-functions.md §11.6): gather the body's
                    // single column into an array and run the SAME 3VL fold as the array form.
                    SubqueryKind::Quantified { op, all, collation } => {
                        let lv = lhs
                            .as_ref()
                            .expect("a quantified subquery carries its resolved lhs")
                            .eval(row, env, m)?;
                        let elements: Vec<Value> = r
                            .rows
                            .into_iter()
                            .map(|row| subquery_row_value(row, *row_valued))
                            .collect();
                        let arr = if elements.is_empty() {
                            ArrayVal::empty()
                        } else {
                            ArrayVal {
                                dims: vec![elements.len()],
                                lbounds: vec![1],
                                elements,
                            }
                        };
                        quantified_membership(
                            *op,
                            *all,
                            *row_valued,
                            &lv,
                            &Value::Array(arr),
                            collation.as_deref(),
                            m,
                        )
                    }
                };
                m.release_query(held);
                out
            }),
            // A folded uncorrelated `IN (subquery)` — the list is constant; test membership per row.
            RExpr::InValues { lhs, list, negated } => outline(|| {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                in_membership(&lv, list, *negated, m)
            }),
            // A quantified array comparison `lhs op ANY/ALL(array)` (array-functions.md §11) — the
            // array spelling of IN, the 3VL fold over the array's flattened elements.
            RExpr::Quantified {
                op,
                all,
                lhs,
                array,
                row_wise,
                collation,
                ..
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                let av = array.eval(row, env, m)?;
                quantified_membership(*op, *all, *row_wise, &lv, &av, collation.as_deref(), m)
            }),
            // `width_bucket(operand, thresholds)` (array-functions.md §14): strict like a scalar
            // function — a NULL operand short-circuits before `thresholds` is evaluated.
            RExpr::WidthBucketArray {
                operand,
                thresholds,
                collation,
            } => outline(|| {
                m.charge(COSTS.operator_eval);
                let ov = operand.eval(row, env, m)?;
                if matches!(ov, Value::Null) {
                    return Ok(Value::Null);
                }
                let tv = thresholds.eval(row, env, m)?;
                if matches!(tv, Value::Null) {
                    return Ok(Value::Null);
                }
                width_bucket_array(&ov, &tv, collation.as_deref(), m)
            }),
        }
    }
}

/// A `json` accessor operator over a verbatim-text base (json.md §4, json-sql-functions.md §1):
/// `-> ->>` take a key (text) or index (int), `#> #>>` a `text[]` path (a NULL element misses, PG).
/// `-> #>` return the matched sub-text as `json` exactly as written; `->> #>>` render it as text (a
/// string de-escaped, a JSON null → SQL NULL). An EMPTY `#>` path returns the whole document.
fn json_get_verbatim(op: JsonGetOp, text: &str, arg: &Value) -> Result<Value> {
    let steps: Vec<json::VerbatimStep> = match (op, arg) {
        (JsonGetOp::Arrow | JsonGetOp::ArrowText, Value::Text(k)) => vec![json::VerbatimStep {
            key: Some(k.clone()),
            index: None,
        }],
        (JsonGetOp::Arrow | JsonGetOp::ArrowText, Value::Int(i)) => vec![json::VerbatimStep {
            key: None,
            index: Some(*i),
        }],
        (JsonGetOp::HashArrow | JsonGetOp::HashArrowText, Value::Array(arr)) => {
            let mut steps = Vec::with_capacity(arr.elements.len());
            for e in &arr.elements {
                match e {
                    Value::Text(s) => steps.push(json::VerbatimStep {
                        key: Some(s.clone()),
                        index: json::path_step_index(s),
                    }),
                    Value::Null => return Ok(Value::Null),
                    _ => unreachable!("a text[] path has text/NULL elements"),
                }
            }
            steps
        }
        _ => {
            unreachable!("resolver guarantees a text/int arg for -> / ->> and text[] for #> / #>>")
        }
    };
    // PG's get_worker lexes the WHOLE document with `need_escapes`, so a `\u0000` or an unpaired
    // surrogate anywhere in it raises, matched or not (json.md §4.2).
    json::check_escapes(text)?;
    let sub = if steps.is_empty() {
        Some(json::verbatim_whole(text))
    } else {
        json::verbatim_get(text, &steps)?
    };
    let Some(sub) = sub else {
        return Ok(Value::Null);
    };
    match op {
        JsonGetOp::Arrow | JsonGetOp::HashArrow => Ok(Value::Json(sub.to_string())),
        JsonGetOp::ArrowText | JsonGetOp::HashArrowText => {
            Ok(json::verbatim_to_text(sub)?.map_or(Value::Null, Value::Text))
        }
    }
}
