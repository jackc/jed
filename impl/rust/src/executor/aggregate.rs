//! Aggregate accumulation — the Acc accumulator state machine (fold/unfold/finalize, including
//! ordered-set/hypothetical-set finalization and the percentile helpers). Mirrors the accumulation half
//! of impl/go aggregate.go; aggregate/window function-call resolution stays in mod.rs for now.

use super::*;

impl Acc {
    /// Build the accumulator for one resolved aggregate. Ordered-set aggregates need their
    /// `WITHIN GROUP` direction + fraction (carried on the spec, not the `Copy` plan), so they go
    /// through here; every other plan delegates to [`Acc::new`]. Only the aggregation stage builds
    /// ordered-set accumulators — the window stage (which calls [`Acc::new`] directly) never sees
    /// one (an ordered-set aggregate with `OVER` is 0A000, rejected at resolve).
    /// `zone` is the session zone a JSON aggregate renders a `timestamptz` input in.
    pub(crate) fn from_spec(s: &AggSpec, zone: &crate::timezone::ZoneRef) -> Acc {
        // An in-aggregate ORDER BY buffers the passing rows first (aggregates.md §22); the plan's own
        // accumulator is built at finalize ([`Acc::from_spec_unordered`]) and fed in key order.
        if s.order.is_some() {
            return Acc::Ordered { rows: Vec::new() };
        }
        Acc::from_spec_unordered(s, zone)
    }

    /// [`Acc::from_spec`] ignoring any in-aggregate `ORDER BY` — the accumulator an ordered
    /// aggregate's sorted arguments are finally folded into.
    pub(crate) fn from_spec_unordered(s: &AggSpec, zone: &crate::timezone::ZoneRef) -> Acc {
        match s.plan {
            AggPlan::OrderedSetMode
            | AggPlan::OrderedSetDisc
            | AggPlan::OrderedSetCont
            | AggPlan::OrderedSetContInterval => {
                let osa = s
                    .osa
                    .as_ref()
                    .expect("an ordered-set plan carries its OSA params");
                Acc::OrderedSet {
                    kind: s.plan,
                    desc: osa.desc,
                    // The per-group fraction is filled in just before finalize (the direct argument
                    // evaluated against the synthetic row); `mode` keeps `None`.
                    frac: None,
                    collation: osa.collation.clone(),
                    vals: Vec::new(),
                    floats: Vec::new(),
                }
            }
            AggPlan::HypoRank
            | AggPlan::HypoDenseRank
            | AggPlan::HypoPercentRank
            | AggPlan::HypoCumeDist => Acc::Hypothetical {
                kind: s.plan,
                rows: Vec::new(),
            },
            _ => Acc::with_context(s.plan, s.collation.clone(), &s.json_tys, zone),
        }
    }

    /// [`Acc::new`] with a text MIN/MAX's comparison collation (spec/design/collation.md §1) and a
    /// JSON aggregate's operand type(s) + session zone ([`JsonAggConv`]); every other plan ignores
    /// them.
    pub(crate) fn with_context(
        plan: AggPlan,
        coll: Option<std::sync::Arc<Collation>>,
        json_tys: &[ResolvedType],
        zone: &crate::timezone::ZoneRef,
    ) -> Acc {
        let mut acc = Acc::new(
            plan,
            JsonAggConv {
                tys: json_tys.to_vec(),
                zone: zone.clone(),
            },
        );
        if let Acc::MinMax { collation, .. } = &mut acc {
            *collation = coll;
        }
        acc
    }

    pub(crate) fn new(plan: AggPlan, conv: JsonAggConv) -> Acc {
        match plan {
            AggPlan::CountStar => Acc::CountStar(0),
            AggPlan::Count => Acc::Count(0),
            AggPlan::SumInt => Acc::SumInt {
                sum: 0,
                seen: false,
            },
            AggPlan::SumDecimal => Acc::SumDecimal {
                sum: Decimal::from_i64(0),
                seen: false,
            },
            AggPlan::Avg => Acc::Avg {
                sum: Decimal::from_i64(0),
                count: 0,
            },
            AggPlan::SumFloat(w) => Acc::FloatFold {
                width: w,
                is_avg: false,
                total: 0.0,
                count: 0,
                any_nan: false,
                pos_inf: false,
                neg_inf: false,
            },
            AggPlan::AvgFloat(w) => Acc::FloatFold {
                width: w,
                is_avg: true,
                total: 0.0,
                count: 0,
                any_nan: false,
                pos_inf: false,
                neg_inf: false,
            },
            AggPlan::Min => Acc::MinMax {
                cur: None,
                is_min: true,
                collation: None,
            },
            AggPlan::Max => Acc::MinMax {
                cur: None,
                is_min: false,
                collation: None,
            },
            AggPlan::JsonAgg { compact, strict } => Acc::JsonAgg {
                elems: Vec::new(),
                compact,
                strict,
                seen: false,
                conv,
            },
            AggPlan::JsonObjectAgg { json, unique } => Acc::JsonObjectAgg {
                pairs: Vec::new(),
                json,
                unique,
                seen: false,
                conv,
            },
            AggPlan::OrderedSetMode
            | AggPlan::OrderedSetDisc
            | AggPlan::OrderedSetCont
            | AggPlan::OrderedSetContInterval
            | AggPlan::HypoRank
            | AggPlan::HypoDenseRank
            | AggPlan::HypoPercentRank
            | AggPlan::HypoCumeDist => {
                unreachable!(
                    "ordered-set / hypothetical-set accumulators are built via Acc::from_spec, never the window stage"
                )
            }
        }
    }

    /// Fold one input value into the accumulator. NULL arguments are skipped (COUNT(*) ignores
    /// the value and always counts). Traps 22003 on SUM/AVG overflow at the result bound.
    /// A decimal SUM/AVG fold charges size-scaled `decimal_work` against the running
    /// accumulator (the `+` formula — spec/design/cost.md §3 "decimal_work"); MIN/MAX folds
    /// are direct Value compares like the sort's and stay unmetered — under a collation too, where
    /// they compare sort keys like a collated sort (spec/design/collation.md §11).
    pub(crate) fn fold(&mut self, value: Value, m: &mut Meter) -> Result<()> {
        match self {
            Acc::CountStar(n) => *n += 1,
            Acc::Count(n) => {
                if !matches!(value, Value::Null) {
                    *n += 1;
                }
            }
            Acc::SumInt { sum, seen } => {
                if let Value::Int(v) = value {
                    *sum = sum
                        .checked_add(v)
                        .ok_or_else(|| overflow(ScalarType::Int64))?;
                    *seen = true;
                }
            }
            Acc::SumDecimal { sum, seen } => {
                if !matches!(value, Value::Null) {
                    let d = to_decimal(value);
                    m.charge(COSTS.decimal_work * ((decimal::work_linear(sum, &d) - 1) as i64));
                    m.guard()?;
                    // Uncapped: the running sum may exceed the §2 format cap mid-fold; only the
                    // FINAL result is cap-checked (in `finalize`), matching PG and making the trap
                    // order-independent (spec/design/decimal.md §2, determinism.md §7).
                    *sum = sum.add_uncapped(&d);
                    *seen = true;
                }
            }
            Acc::Avg { sum, count } => {
                if !matches!(value, Value::Null) {
                    let d = to_decimal(value);
                    m.charge(COSTS.decimal_work * ((decimal::work_linear(sum, &d) - 1) as i64));
                    m.guard()?;
                    // Uncapped (as SumDecimal): the average's final divide brings the value back in
                    // range, so AVG never traps on an over-cap intermediate sum the way PG does not.
                    *sum = sum.add_uncapped(&d);
                    *count += 1;
                }
            }
            Acc::FloatFold {
                width,
                total,
                count,
                any_nan,
                pos_inf,
                neg_inf,
                ..
            } => {
                // Classify each non-NULL input order-independently (the §7 special-value pass); fold
                // each finite input into the running total in scan order (ledgered order-dependent).
                // Convert a f32 to its exact f64 first.
                let f = match value {
                    Value::Null => return Ok(()),
                    Value::Float32(f) => f as f64,
                    Value::Float64(f) => f,
                    _ => unreachable!("resolver restricts float SUM/AVG to a float operand"),
                };
                *count += 1;
                if f.is_nan() {
                    *any_nan = true;
                } else if f.is_infinite() {
                    if f > 0.0 {
                        *pos_inf = true;
                    } else {
                        *neg_inf = true;
                    }
                } else {
                    // Canonicalize -0 → +0, then add in scan order. When f32, re-round the running
                    // total to binary32 each add (the §7 width-correct fold): `*total as f32`
                    // recovers the f32-valued total exactly.
                    let x = if f == 0.0 { 0.0 } else { f };
                    if width.is_float32() {
                        *total = (*total as f32 + x as f32) as f64;
                    } else {
                        *total += x;
                    }
                }
            }
            Acc::MinMax {
                cur,
                is_min,
                collation,
            } => {
                if !matches!(value, Value::Null) {
                    let next = match cur.take() {
                        None => value,
                        Some(c) => {
                            let ord = collated_or_value_cmp(collation.as_deref(), &c, &value)?;
                            let keep_current = if *is_min {
                                ord != std::cmp::Ordering::Greater
                            } else {
                                ord != std::cmp::Ordering::Less
                            };
                            if keep_current { c } else { value }
                        }
                    };
                    *cur = Some(next);
                }
            }
            Acc::JsonAgg {
                elems,
                compact,
                strict,
                seen,
                conv,
            } => {
                // Mark the group non-empty even when the strict filter drops this row (an all-skipped
                // group still finalizes to `[]`, not NULL — only a zero-row group is NULL).
                *seen = true;
                // Non-strict: a NULL input contributes a JSON null; `_strict` skips it. Each input is
                // converted to its JSON image as it is folded (PG's transition function); a finite
                // timestamptz in it renders in the session zone, one `timezone` unit each.
                if !(*strict && matches!(value, Value::Null)) {
                    m.charge(COSTS.generated_row);
                    m.charge(COSTS.timezone * value.zone_render_count() as i64);
                    m.guard()?;
                    elems.push(json_agg_image(&value, *compact, &conv.tys[0], &conv.zone)?);
                }
            }
            Acc::JsonObjectAgg {
                pairs,
                json,
                unique,
                seen,
                conv,
            } => {
                *seen = true;
                m.charge(COSTS.generated_row);
                // The operand is a Row(key, value); split it back out.
                let (kv, vv) = match value {
                    Value::Composite(mut fields) if fields.len() == 2 => {
                        let v = fields.pop().unwrap();
                        let k = fields.pop().unwrap();
                        (k, v)
                    }
                    _ => unreachable!("object_agg operand is a 2-field Row"),
                };
                m.charge(COSTS.timezone * (kv.zone_render_count() + vv.zone_render_count()) as i64);
                m.guard()?;
                // A NULL key: json_object_agg 22004, jsonb_object_agg 22023 (PG's two messages).
                if matches!(kv, Value::Null) {
                    if *json {
                        return Err(object_key_null());
                    }
                    return Err(EngineError::new(
                        SqlState::InvalidParameterValue,
                        "field name must not be null",
                    ));
                }
                // The key is always a JSON string (an array/composite/json key is 22023).
                let key = json_key_text(&kv, &conv.zone)?;
                if *unique && pairs.iter().any(|(k, _)| k == &key) {
                    return Err(EngineError::new(
                        SqlState::DuplicateJsonObjectKeyValue,
                        "duplicate JSON object key value",
                    ));
                }
                let image = json_agg_image(&vv, *json, &conv.tys[1], &conv.zone)?;
                pairs.push((key, image));
            }
            Acc::OrderedSet {
                kind, vals, floats, ..
            } => {
                // Collect the non-NULL aggregated argument (the WITHIN GROUP order key, evaluated per
                // row). percentile_cont widens each numeric value to f64 up front (the correctly-
                // rounded cast, matching PG's numeric→float8); mode/percentile_disc keep the Value.
                if !matches!(value, Value::Null) {
                    if matches!(kind, AggPlan::OrderedSetCont) {
                        floats.push(percentile_input_f64(&value)?);
                    } else {
                        vals.push(value);
                    }
                }
            }
            // A hypothetical-set aggregate buffers its key tuple in the fold LOOP (which has the row),
            // not through `Acc::fold` (aggregates.md §19), so this is never reached.
            Acc::Hypothetical { .. } => {
                unreachable!("a hypothetical-set accumulator buffers tuples in the fold loop")
            }
            // Likewise an ordered aggregate's `[keys…, argument]` tuples (aggregates.md §22).
            Acc::Ordered { .. } => {
                unreachable!("an ordered aggregate buffers tuples in the fold loop")
            }
        }
        Ok(())
    }

    /// The query-memory bytes folding `value` makes this accumulator retain (memory.md §6.3): the input
    /// of a JSON / ordered-set aggregate, nothing for a fixed-size running value. Hypothetical-set and
    /// DISTINCT-set retention is charged by the fold loop, which owns those buffers.
    pub(crate) fn retained_bytes(&self, value: &Value) -> i64 {
        match self {
            Acc::JsonAgg { strict, .. } => {
                if *strict && matches!(value, Value::Null) {
                    0
                } else {
                    crate::memsize::value_bytes(value)
                }
            }
            Acc::JsonObjectAgg { .. } => match value {
                Value::Composite(fields) => crate::memsize::key_bytes(fields),
                _ => 0,
            },
            Acc::OrderedSet { .. } if !matches!(value, Value::Null) => {
                crate::memsize::value_bytes(value)
            }
            _ => 0,
        }
    }

    /// [`Acc::fold`], then reserve what the fold retained (memory.md §6.3), adding it to `charge` — the
    /// caller's running total for this accumulator, released when the accumulator is discarded.
    pub(crate) fn fold_charged(
        &mut self,
        value: Value,
        m: &mut Meter,
        charge: &mut i64,
    ) -> Result<()> {
        let bytes = if m.query_memory_active() {
            self.retained_bytes(&value)
        } else {
            0
        };
        self.fold(value, m)?;
        if bytes > 0 {
            m.reserve_query(bytes)?;
            *charge += bytes;
        }
        Ok(())
    }

    /// Un-fold one input value — the inverse of `fold` — used ONLY by the sliding-window
    /// optimization (window.md §5.2/§8) for the exactly-invertible COUNT / COUNT(*) (integer
    /// counters: add-then-remove is exact and order-independent). Every other accumulator is
    /// never un-folded — a moving frame over SUM/AVG/MIN/MAX/float re-folds from scratch instead
    /// (decimal scale, intermediate-overflow trap order, and float non-associativity make them
    /// unsafe to invert). Charges nothing (a count step is unmetered like its fold).
    pub(crate) fn unfold(&mut self, value: Value, _m: &mut Meter) -> Result<()> {
        match self {
            Acc::CountStar(n) => *n -= 1,
            Acc::Count(n) => {
                if !matches!(value, Value::Null) {
                    *n -= 1;
                }
            }
            _ => {
                unreachable!("only COUNT/COUNT(*) are un-folded by the sliding-window optimization")
            }
        }
        Ok(())
    }

    /// Produce the aggregate's final value over the group. COUNT → its count (0 over empty);
    /// SUM/MIN/MAX → NULL over an empty/all-NULL group; AVG → sum/count (NULL if count 0).
    pub(crate) fn finalize(self) -> Result<Value> {
        Ok(match self {
            Acc::CountStar(n) | Acc::Count(n) => Value::Int(n),
            Acc::SumInt { sum, seen } => {
                if seen {
                    Value::Int(sum)
                } else {
                    Value::Null
                }
            }
            Acc::SumDecimal { sum, seen } => {
                if seen {
                    // The only cap check for the fold: the FINAL sum traps 22003 if over the §2
                    // cap (PG's make_result), but no intermediate does (decimal.md §2).
                    Value::Decimal(sum.check_cap()?)
                } else {
                    Value::Null
                }
            }
            Acc::Avg { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    // `div` cap-checks its (in-range) result; the over-cap-capable running `sum` is
                    // never surfaced directly, so AVG matches PG even when SUM would overflow.
                    Value::Decimal(sum.div(&Decimal::from_i64(count))?)
                }
            }
            Acc::FloatFold {
                width,
                is_avg,
                total,
                count,
                any_nan,
                pos_inf,
                neg_inf,
            } => finalize_float_fold(width, is_avg, total, count, any_nan, pos_inf, neg_inf)?,
            Acc::MinMax { cur, .. } => cur.unwrap_or(Value::Null),
            // json_agg/jsonb_agg: NULL over an empty group; else the JSON array (json: PG's
            // `", "`-joined element texts; jsonb: the canonical array).
            Acc::JsonAgg {
                elems,
                compact,
                strict: _,
                seen,
                conv,
            } => {
                if !seen {
                    // A zero-row group → SQL NULL. (A non-empty group the strict filter emptied still
                    // finalizes to `[]` below — `seen` is true, `elems` is empty.)
                    Value::Null
                } else if compact {
                    let breaks = json_agg_breaks_line(&conv.tys[0]);
                    let mut out = String::from("[");
                    for (i, e) in elems.iter().enumerate() {
                        push_json_agg_elem(&mut out, i, e, breaks);
                    }
                    out.push(']');
                    // The aggregate nests each input one level (json.md §6.4).
                    json::check_text_depth(&out)?;
                    Value::Json(out)
                } else {
                    let nodes = elems.into_iter().map(jsonb_image_node).collect();
                    let arr = JsonNode::Array(nodes);
                    json::check_depth(&arr)?;
                    Value::Jsonb(arr)
                }
            }
            // json_object_agg/jsonb_object_agg: NULL over an empty group; else the JSON object (json
            // keeps insertion order + dups + " : " spacing, jsonb canonicalizes last-wins).
            Acc::JsonObjectAgg {
                pairs, json, seen, ..
            } => {
                if !seen {
                    Value::Null
                } else if json {
                    let mut out = String::from("{ ");
                    for (i, (k, v)) in pairs.iter().enumerate() {
                        push_json_object_agg_pair(&mut out, i, k, v);
                    }
                    // PG's json_object_agg PADS the braces (`{ … }`) — distinct from json_build_object.
                    out.push_str(" }");
                    // The aggregate nests each value one level (json.md §6.4).
                    json::check_text_depth(&out)?;
                    Value::Json(out)
                } else {
                    let members = pairs
                        .into_iter()
                        .map(|(k, v)| (k, jsonb_image_node(v)))
                        .collect();
                    let out = json::make_object(members);
                    json::check_depth(&out)?;
                    Value::Jsonb(out)
                }
            }
            Acc::OrderedSet {
                kind,
                desc,
                frac,
                collation,
                vals,
                floats,
            } => finalize_ordered_set(
                kind,
                desc,
                collation.as_deref(),
                frac.as_ref(),
                vals,
                floats,
            )?,
            // A hypothetical-set aggregate is finalized in the group-emission loop (it needs the
            // spec's per-key sort specs), never through `Acc::finalize` (aggregates.md §19).
            Acc::Hypothetical { .. } => {
                unreachable!(
                    "a hypothetical-set accumulator is finalized in the group-emission loop"
                )
            }
            // An ordered aggregate is sorted + folded in the group-emission loop (it needs the
            // spec's `AggOrder`), never through `Acc::finalize` (aggregates.md §22).
            Acc::Ordered { .. } => {
                unreachable!("an ordered aggregate is finalized in the group-emission loop")
            }
        })
    }
}

/// Evaluate one passing input row's in-aggregate `ORDER BY` keys (aggregates.md §22) and return the
/// buffered tuple `[key…, value]`. Each key's `operator_eval`s are charged (it is evaluated per row,
/// after the argument). A collated text / text[] key is replaced by its UCA sort key (`bytea`), so the
/// finalize sort is a plain value sort and an unmapped code point fails here, per row — independent
/// of how many rows the group has.
pub(crate) fn agg_order_tuple(
    order: &AggOrder,
    value: Value,
    row: &[Value],
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Vec<Value>> {
    let mut tuple = Vec::with_capacity(order.keys.len() + 1);
    for (k, sort) in order.keys.iter().zip(&order.sorts) {
        let kv = k.eval(row, env, m)?;
        let kv = match &sort.collation {
            Some(c) => match collation_sort_key(c, &kv)? {
                Some(bytes) => Value::Bytea(bytes),
                None => kv,
            },
            None => kv,
        };
        tuple.push(kv);
    }
    tuple.push(value);
    Ok(tuple)
}

/// Stable-sort an ordered aggregate's buffered `[key…, value]` tuples by its `ORDER BY` (each key's
/// direction + NULL placement; ties keep input order) and return the values in that order
/// (aggregates.md §22). The sort is unmetered, like the query ORDER BY sort.
pub(crate) fn sort_agg_order_rows(mut rows: Vec<Vec<Value>>, order: &AggOrder) -> Vec<Value> {
    rows.sort_by(|a, b| {
        for (i, sort) in order.sorts.iter().enumerate() {
            let ord = key_cmp(&a[i], &b[i], sort.desc, sort.nulls_first);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    rows.into_iter()
        .map(|mut r| r.pop().expect("an ordered tuple ends with its value"))
        .collect()
}

/// A JSON aggregate input's image (json-sql-functions.md §4): for the json aggregates its `json` text
/// (`Value::Null` for a NULL input, rendered `null`), for the jsonb aggregates a `Value::Jsonb` node.
pub(crate) fn json_agg_image(
    v: &Value,
    json: bool,
    ty: &ResolvedType,
    zone: &crate::timezone::ZoneRef,
) -> Result<Value> {
    if json {
        if matches!(v, Value::Null) {
            return Ok(Value::Null);
        }
        let mut text = String::new();
        datum_to_json(v, ty, zone, &mut text)?;
        Ok(Value::Json(text))
    } else {
        Ok(Value::Jsonb(datum_to_jsonb(v, ty, zone)?))
    }
}

/// The node of a jsonb aggregate's buffered image ([`json_agg_image`]).
pub(crate) fn jsonb_image_node(v: Value) -> JsonNode {
    match v {
        Value::Jsonb(n) => n,
        _ => unreachable!("a jsonb aggregate buffers jsonb images"),
    }
}

/// Append the `i`-th json_agg element image onto `out` (PostgreSQL's json_agg_transfn): elements are
/// separated by `", "`, and a non-NULL array or composite after the first (`breaks`) also starts a
/// new line (`"\n "`).
pub(crate) fn push_json_agg_elem(out: &mut String, i: usize, e: &Value, breaks: bool) {
    if i > 0 {
        out.push_str(", ");
    }
    match e {
        Value::Json(t) => {
            if i > 0 && breaks {
                out.push_str("\n ");
            }
            out.push_str(t);
        }
        _ => out.push_str("null"),
    }
}

/// Append the `i`-th json_object_agg `key : value` pair onto `out`, `", "`-separated.
pub(crate) fn push_json_object_agg_pair(out: &mut String, i: usize, key: &str, v: &Value) {
    if i > 0 {
        out.push_str(", ");
    }
    json::write_json_string(key, out);
    out.push_str(" : ");
    match v {
        Value::Json(t) => out.push_str(t),
        _ => out.push_str("null"),
    }
}
