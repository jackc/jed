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
    pub(crate) fn from_spec(s: &AggSpec) -> Acc {
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
            _ => Acc::new(s.plan),
        }
    }

    pub(crate) fn new(plan: AggPlan) -> Acc {
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
            },
            AggPlan::Max => Acc::MinMax {
                cur: None,
                is_min: false,
            },
            AggPlan::JsonAgg { compact, strict } => Acc::JsonAgg {
                nodes: Vec::new(),
                compact,
                strict,
                seen: false,
            },
            AggPlan::JsonObjectAgg { json, unique } => Acc::JsonObjectAgg {
                pairs: Vec::new(),
                json,
                unique,
                seen: false,
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
    /// are direct Value compares like the sort's and stay unmetered.
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
            Acc::MinMax { cur, is_min } => {
                if !matches!(value, Value::Null) {
                    let next = match cur.take() {
                        None => value,
                        Some(c) => {
                            let ord = value_cmp(&c, &value);
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
                nodes,
                strict,
                seen,
                ..
            } => {
                // Mark the group non-empty even when the strict filter drops this row (an all-skipped
                // group still finalizes to `[]`, not NULL — only a zero-row group is NULL).
                *seen = true;
                // Non-strict: a NULL input contributes a JSON null; `_strict` skips it. Each input's
                // JSON image is the `to_jsonb` kernel (deferred 0A000 sources propagate here).
                if !(*strict && matches!(value, Value::Null)) {
                    m.charge(COSTS.generated_row);
                    m.guard()?;
                    nodes.push(value_to_node(&value)?);
                }
            }
            Acc::JsonObjectAgg {
                pairs,
                unique,
                seen,
                ..
            } => {
                *seen = true;
                m.charge(COSTS.generated_row);
                m.guard()?;
                // The operand is a Row(key, value); split it back out.
                let (kv, vv) = match value {
                    Value::Composite(mut fields) if fields.len() == 2 => {
                        let v = fields.pop().unwrap();
                        let k = fields.pop().unwrap();
                        (k, v)
                    }
                    _ => unreachable!("object_agg operand is a 2-field Row"),
                };
                // The key is coerced to text (text/integer/decimal/boolean); a NULL key → 22023.
                let key = match &kv {
                    Value::Null => {
                        return Err(EngineError::new(
                            SqlState::InvalidParameterValue,
                            "field name must not be null",
                        ));
                    }
                    _ => object_key_text(&kv, 1)?,
                };
                if *unique && pairs.iter().any(|(k, _)| k == &key) {
                    return Err(EngineError::new(
                        SqlState::DuplicateJsonObjectKeyValue,
                        "duplicate JSON object key value",
                    ));
                }
                pairs.push((key, vv));
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
            // json_agg/jsonb_agg: NULL over an empty group; else the JSON array (json compact /
            // jsonb canonical).
            Acc::JsonAgg {
                nodes,
                compact,
                strict: _,
                seen,
            } => {
                if !seen {
                    // A zero-row group → SQL NULL. (A non-empty group the strict filter emptied still
                    // finalizes to `[]` below — `seen` is true, `nodes` is empty.)
                    Value::Null
                } else {
                    let arr = JsonNode::Array(nodes);
                    // Both json_agg and jsonb_agg render the spaced canonical form (PG joins the
                    // element texts with ", "); the json variant is just typed `json`. (A json input
                    // element is canonicalized by `value_to_node`, a documented divergence from PG's
                    // verbatim — the json_array_elements precedent.) `compact` = the json result.
                    let text = json::jsonb_out(&arr);
                    if compact {
                        Value::Json(text)
                    } else {
                        Value::Jsonb(arr)
                    }
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
                    let mut parts = Vec::with_capacity(pairs.len());
                    for (k, v) in &pairs {
                        parts.push(format!(
                            "{} : {}",
                            json::json_compact_out(&JsonNode::String(k.clone())),
                            elem_json_text(v)?
                        ));
                    }
                    // PG's json_object_agg PADS the braces (`{ … }`) — distinct from json_build_object.
                    Value::Json(format!("{{ {} }}", parts.join(", ")))
                } else {
                    let mut members = Vec::with_capacity(pairs.len());
                    for (k, v) in pairs {
                        members.push((k, value_to_node(&v)?));
                    }
                    Value::Jsonb(json::make_object(members))
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
        })
    }
}
