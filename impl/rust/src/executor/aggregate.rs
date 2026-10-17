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
            AggPlan::RegrCount => Acc::RegrCount(0),
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
            AggPlan::BoolAnd => Acc::Bool {
                cur: None,
                is_and: true,
            },
            AggPlan::Bit(op) => Acc::Bit { cur: None, op },
            AggPlan::Moment { stat, float: false } => Acc::MomentsExact {
                stat,
                n: 0,
                sx: Decimal::from_i64(0),
                sxx: Decimal::from_i64(0),
            },
            AggPlan::Moment { stat, float: true } => Acc::MomentsFloat {
                stat,
                n: 0.0,
                sx: 0.0,
                sxx: 0.0,
            },
            AggPlan::Pair { stat, float: false } => Acc::PairsExact {
                stat,
                n: 0,
                sums: Box::new(std::array::from_fn(|_| Decimal::from_i64(0))),
            },
            AggPlan::Pair { stat, float: true } => Acc::PairsFloat { stat, t: [0.0; 6] },
            AggPlan::StringAgg => Acc::StringAgg {
                rows: Vec::new(),
                seen: false,
            },
            AggPlan::ArrayAgg { nested } => Acc::ArrayAgg {
                elems: Vec::new(),
                seen: false,
                nested,
                shape: None,
            },
            AggPlan::BoolOr => Acc::Bool {
                cur: None,
                is_and: false,
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
            Acc::Bool { cur, is_and } => {
                // NULLs are skipped (aggregates.md §23).
                if let Value::Bool(b) = value {
                    *cur = Some(match *cur {
                        None => b,
                        Some(c) if *is_and => c && b,
                        Some(c) => c || b,
                    });
                }
            }
            Acc::Bit { cur, op } => {
                // NULLs are skipped (aggregates.md §23). The result stays within the input width.
                if let Value::Int(v) = value {
                    *cur = Some(match (*cur, *op) {
                        (None, _) => v,
                        (Some(c), BitOp::And) => c & v,
                        (Some(c), BitOp::Or) => c | v,
                        (Some(c), BitOp::Xor) => c ^ v,
                    });
                }
            }
            Acc::MomentsExact { n, sx, sxx, .. } => {
                // NULLs are skipped. Each fold charges decimal_work for the square and the two
                // additions, by the `*` and `+` formulas (spec/design/cost.md §3).
                if !matches!(value, Value::Null) {
                    let x = to_decimal(value);
                    let x2 = x.mul_exact(&x);
                    m.charge(
                        COSTS.decimal_work
                            * ((decimal::work_mul(&x, &x) - 1 + decimal::work_linear(sx, &x) - 1
                                + decimal::work_linear(sxx, &x2)
                                - 1) as i64),
                    );
                    m.guard()?;
                    *sx = sx.add_uncapped(&x);
                    *sxx = sxx.add_uncapped(&x2);
                    *n += 1;
                }
            }
            Acc::MomentsFloat { n, sx, sxx, .. } => {
                let x = match value {
                    Value::Null => return Ok(()),
                    Value::Float32(f) => f as f64,
                    Value::Float64(f) => f,
                    _ => unreachable!("the resolver gives a float moment a float operand"),
                };
                fold_float_moment(n, sx, sxx, x)?;
            }
            Acc::RegrCount(n) => {
                if pair_args(value).is_some() {
                    *n += 1;
                }
            }
            Acc::PairsExact { n, sums, .. } => fold_exact_pair(n, sums, value, m)?,
            Acc::PairsFloat { t, .. } => fold_float_pair(t, value)?,
            Acc::StringAgg { rows, seen } => {
                // A NULL value is skipped; a NULL delimiter adds nothing (PostgreSQL
                // string_agg_transfn).
                let Value::Composite(fields) = value else {
                    unreachable!("string_agg's operand is a 2-field Row")
                };
                if !matches!(fields[0], Value::Null) {
                    *seen = true;
                    rows.push(fields);
                }
            }
            Acc::ArrayAgg {
                elems,
                seen,
                nested,
                shape,
            } => {
                if *nested {
                    check_array_agg_input(shape, &value)?;
                }
                *seen = true;
                elems.push(value);
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
            Acc::ArrayAgg { .. } => crate::memsize::value_bytes(value),
            Acc::StringAgg { .. } => match value {
                Value::Composite(fields) if !matches!(fields[0], Value::Null) => {
                    crate::memsize::key_bytes(fields)
                }
                _ => 0,
            },
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
            Acc::CountStar(n) | Acc::Count(n) | Acc::RegrCount(n) => Value::Int(n),
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
            Acc::Bool { cur, .. } => cur.map_or(Value::Null, Value::Bool),
            Acc::Bit { cur, .. } => cur.map_or(Value::Null, Value::Int),
            Acc::MomentsExact {
                stat, n, sx, sxx, ..
            } => {
                let (sample, stddev) = stat.sample_stddev();
                decimal::moment_variance(n, &sx, &sxx, sample, stddev)?
                    .map_or(Value::Null, Value::Decimal)
            }
            Acc::MomentsFloat { stat, n, sxx, .. } => {
                let (sample, stddev) = stat.sample_stddev();
                float_moment_variance(n, sxx, sample, stddev)
            }
            Acc::PairsExact { stat, n, sums } => finalize_exact_pair(stat, n, &sums)?,
            Acc::PairsFloat { stat, t } => finalize_float_pair(stat, &t),
            Acc::StringAgg { rows, seen } => {
                if seen {
                    finalize_string_agg(rows)
                } else {
                    Value::Null
                }
            }
            Acc::ArrayAgg {
                elems,
                seen,
                nested,
                ..
            } => {
                if seen {
                    finalize_array_agg(nested, elems)
                } else {
                    Value::Null
                }
            }
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

/// Validate one input of array_agg over arrays (PostgreSQL `accumArrayResultArr`, aggregates.md §23):
/// a NULL is 22004, an empty first input 2202E, a result past `MAXDIM` 54000, and an input whose shape
/// differs from the first's 2202E. The first input records the shape.
fn check_array_agg_input(shape: &mut Option<(Vec<usize>, Vec<i32>)>, value: &Value) -> Result<()> {
    let arr = match value {
        Value::Null => {
            return Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "cannot accumulate null arrays",
            ));
        }
        Value::Array(a) => a,
        _ => unreachable!("the resolver gives a nested array_agg an array operand"),
    };
    match shape {
        None => {
            if arr.ndim() == 0 {
                return Err(EngineError::new(
                    SqlState::ArraySubscriptError,
                    "cannot accumulate empty arrays",
                ));
            }
            if arr.ndim() + 1 > 6 {
                return Err(EngineError::new(
                    SqlState::ProgramLimitExceeded,
                    format!(
                        "number of array dimensions ({}) exceeds the maximum allowed (6)",
                        arr.ndim() + 1
                    ),
                ));
            }
            *shape = Some((arr.dims.clone(), arr.lbounds.clone()));
        }
        Some((dims, lbounds)) => {
            if &arr.dims != dims || &arr.lbounds != lbounds {
                return Err(EngineError::new(
                    SqlState::ArraySubscriptError,
                    "cannot accumulate arrays of different dimensionality",
                ));
            }
        }
    }
    Ok(())
}

/// array_agg's result over a non-empty group's inputs (aggregates.md §23): a 1-D array of the inputs,
/// or — over array inputs, whose shapes the fold has already matched — the inputs stacked into one
/// more dimension (lower bound 1).
pub(crate) fn finalize_array_agg(nested: bool, elems: Vec<Value>) -> Value {
    if !nested {
        return Value::Array(ArrayVal::one_dim(elems));
    }
    let n = elems.len();
    let mut out = ArrayVal::empty();
    for (i, v) in elems.into_iter().enumerate() {
        let Value::Array(a) = v else {
            unreachable!("a nested array_agg holds only arrays")
        };
        if i == 0 {
            out.dims = std::iter::once(n).chain(a.dims.iter().copied()).collect();
            out.lbounds = std::iter::once(1)
                .chain(a.lbounds.iter().copied())
                .collect();
        }
        out.elements.extend(a.elements);
    }
    Value::Array(out)
}

/// Join string_agg's non-NULL values in fold order (aggregates.md §23): the first value alone, each
/// later one preceded by its own row's delimiter (none when the delimiter is NULL). The result is
/// bytea over bytea values, else text.
pub(crate) fn finalize_string_agg(rows: Vec<Vec<Value>>) -> Value {
    let bytea = matches!(rows[0][0], Value::Bytea(_));
    let mut out: Vec<u8> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            push_string_agg_bytes(&mut out, &row[1]);
        }
        push_string_agg_bytes(&mut out, &row[0]);
    }
    if bytea {
        Value::Bytea(out)
    } else {
        Value::Text(String::from_utf8(out).expect("joined text stays UTF-8"))
    }
}

fn push_string_agg_bytes(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Text(s) => out.extend_from_slice(s.as_bytes()),
        Value::Bytea(b) => out.extend_from_slice(b),
        Value::Null => {}
        _ => unreachable!("string_agg joins text or bytea"),
    }
}

/// PostgreSQL's float8_accum (and float4_accum, which widens to f64 first) — the Youngs-Cramer update
/// of the one-argument float moments (aggregates.md §23). A finite input whose update overflows to
/// ±Inf traps 22003; an infinite or NaN input makes Sxx NaN.
fn fold_float_moment(n: &mut f64, sx: &mut f64, sxx: &mut f64, x: f64) -> Result<()> {
    let n1 = *n + 1.0;
    let sx1 = *sx + x;
    let mut sxx1 = *sxx;
    if *n > 0.0 {
        let tmp = x * n1 - sx1;
        sxx1 += tmp * tmp / (n1 * *n);
        if sx1.is_infinite() || sxx1.is_infinite() {
            if !sx.is_infinite() && !x.is_infinite() {
                return Err(overflow(ScalarType::Float64));
            }
            sxx1 = f64::NAN;
        }
    } else if x.is_nan() || x.is_infinite() {
        sxx1 = f64::NAN;
    }
    *n = n1;
    *sx = sx1;
    *sxx = sxx1;
    Ok(())
}

/// float8_var_pop / float8_var_samp / float8_stddev_pop / float8_stddev_samp: NULL over no input (or
/// one, for a sample); else Sxx / N or Sxx / (N − 1), square-rooted for a standard deviation.
fn float_moment_variance(n: f64, sxx: f64, sample: bool, stddev: bool) -> Value {
    if n == 0.0 || (sample && n <= 1.0) {
        return Value::Null;
    }
    let r = sxx / if sample { n - 1.0 } else { n };
    Value::Float64(if stddev { r.sqrt() } else { r })
}

/// Split a two-argument statistical aggregate's Row(Y, X) operand; `None` for a pair with a NULL.
fn pair_args(value: Value) -> Option<(Value, Value)> {
    let Value::Composite(mut fields) = value else {
        unreachable!("a two-argument statistical aggregate's operand is a 2-field Row")
    };
    let x = fields.pop().unwrap();
    let y = fields.pop().unwrap();
    if matches!(y, Value::Null) || matches!(x, Value::Null) {
        None
    } else {
        Some((y, x))
    }
}

/// Add one (Y, X) pair to the exact two-argument moments (aggregates.md §23), skipping a pair with a
/// NULL. `sums` is [Σx, Σx², Σy, Σy², Σxy]. Each fold charges decimal_work for the three products
/// and five additions, by the `*` and `+` formulas (spec/design/cost.md §3).
fn fold_exact_pair(
    n: &mut i64,
    sums: &mut [Decimal; 5],
    value: Value,
    m: &mut Meter,
) -> Result<()> {
    let Some((y, x)) = pair_args(value) else {
        return Ok(());
    };
    let (y, x) = (to_decimal(y), to_decimal(x));
    let terms = [
        x.clone(),
        x.mul_exact(&x),
        y.clone(),
        y.mul_exact(&y),
        x.mul_exact(&y),
    ];
    let work = decimal::work_mul(&x, &x)
        + decimal::work_mul(&y, &y)
        + decimal::work_mul(&x, &y)
        + (0..5)
            .map(|i| decimal::work_linear(&sums[i], &terms[i]))
            .sum::<u64>()
        - 8;
    m.charge(COSTS.decimal_work * work as i64);
    m.guard()?;
    for (sum, term) in sums.iter_mut().zip(&terms) {
        *sum = sum.add_uncapped(term);
    }
    *n += 1;
    Ok(())
}

/// Evaluate a two-argument statistic over the exact sums (aggregates.md §23), with the exact
/// numerators numX = N·Σx² − Σx·Σx, numY = N·Σy² − Σy·Σy, and numXY = N·Σxy − Σx·Σy; "/" is jed's
/// decimal division (select_div_scale, half away from zero, 22003 past the cap). NULL over no pair,
/// and where the table in aggregates.md §23 says.
fn finalize_exact_pair(stat: PairStat, n: i64, sums: &[Decimal; 5]) -> Result<Value> {
    if n == 0 {
        return Ok(Value::Null);
    }
    let [sx, _sxx, sy, _syy, sxy] = sums;
    let count = Decimal::from_i64(n);
    let num_xy = count.mul_exact(sxy).add_uncapped(&sx.mul_exact(sy).neg());
    let r = match stat {
        PairStat::CovarPop => num_xy.div(&count.mul_exact(&count))?,
        PairStat::Corr => {
            let num_x = count
                .mul_exact(&sums[1])
                .add_uncapped(&sums[0].mul_exact(&sums[0]).neg());
            let num_y = count
                .mul_exact(&sums[3])
                .add_uncapped(&sums[2].mul_exact(&sums[2]).neg());
            if num_x.is_zero() || num_y.is_zero() {
                return Ok(Value::Null);
            }
            num_xy.div(&num_x.mul_exact(&num_y).dec_sqrt()?)?
        }
        PairStat::CovarSamp => {
            if n < 2 {
                return Ok(Value::Null);
            }
            num_xy.div(&count.mul_exact(&Decimal::from_i64(n - 1)))?
        }
    };
    Ok(Value::Decimal(r))
}

/// PostgreSQL's float8_regr_accum over one (Y, X) pair (f32 widened to f64), skipping a pair with a
/// NULL: the two-variable Youngs-Cramer update of `t` = [N, Σx, Sxx, Σy, Syy, Sxy]. A finite input
/// whose update overflows to ±Inf traps 22003; an infinite or NaN input makes the dependent sums NaN.
fn fold_float_pair(t: &mut [f64; 6], value: Value) -> Result<()> {
    let Some((y, x)) = pair_args(value) else {
        return Ok(());
    };
    let f = |v: Value| match v {
        Value::Float32(f) => f as f64,
        Value::Float64(f) => f,
        _ => unreachable!("the resolver gives a float pair float operands"),
    };
    let (y, x) = (f(y), f(x));
    let [n0, sx0, sxx0, sy0, syy0, sxy0] = *t;
    let n = n0 + 1.0;
    let (sx, sy) = (sx0 + x, sy0 + y);
    let (mut sxx, mut syy, mut sxy) = (sxx0, syy0, sxy0);
    if n0 > 0.0 {
        let tmp_x = x * n - sx;
        let tmp_y = y * n - sy;
        let scale = 1.0 / (n * n0);
        sxx += tmp_x * tmp_x * scale;
        syy += tmp_y * tmp_y * scale;
        sxy += tmp_x * tmp_y * scale;
        if sx.is_infinite()
            || sxx.is_infinite()
            || sy.is_infinite()
            || syy.is_infinite()
            || sxy.is_infinite()
        {
            if ((sx.is_infinite() || sxx.is_infinite()) && !sx0.is_infinite() && !x.is_infinite())
                || ((sy.is_infinite() || syy.is_infinite())
                    && !sy0.is_infinite()
                    && !y.is_infinite())
                || (sxy.is_infinite()
                    && !sx0.is_infinite()
                    && !x.is_infinite()
                    && !sy0.is_infinite()
                    && !y.is_infinite())
            {
                return Err(overflow(ScalarType::Float64));
            }
            if sxx.is_infinite() {
                sxx = f64::NAN;
            }
            if syy.is_infinite() {
                syy = f64::NAN;
            }
            if sxy.is_infinite() {
                sxy = f64::NAN;
            }
        }
    } else {
        if x.is_nan() || x.is_infinite() {
            sxx = f64::NAN;
            sxy = f64::NAN;
        }
        if y.is_nan() || y.is_infinite() {
            syy = f64::NAN;
            sxy = f64::NAN;
        }
    }
    *t = [n, sx, sxx, sy, syy, sxy];
    Ok(())
}

/// PostgreSQL's float8 final function for a two-argument statistic over `t` = [N, Σx, Sxx, Σy,
/// Syy, Sxy] (the table in aggregates.md §23). NULL over no pair.
fn finalize_float_pair(stat: PairStat, t: &[f64; 6]) -> Value {
    let [n, _sx, _sxx, _sy, _syy, sxy] = *t;
    if n < 1.0 {
        return Value::Null;
    }
    Value::Float64(match stat {
        PairStat::CovarPop => sxy / n,
        PairStat::Corr => {
            if t[2] == 0.0 || t[4] == 0.0 {
                return Value::Null;
            }
            sxy / (t[2] * t[4]).sqrt()
        }
        PairStat::CovarSamp => {
            if n < 2.0 {
                return Value::Null;
            }
            sxy / (n - 1.0)
        }
    })
}
