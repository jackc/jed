//! Result emission and post-scan projection (the back half of SELECT execution; mirrors impl/go
//! exec_emit.go): the exec_select_emit driver (projection/GROUP BY/HAVING/DISTINCT/window/ORDER BY),
//! its eager drain, and uncorrelated-subquery folding — as Engine methods.

use super::*;

fn logical_join_row_width(plan: &SelectPlan) -> usize {
    plan.rels.last().map_or(0, |rel| rel.offset + rel.col_count)
}

pub(crate) fn place_physical_relation_row(plan: &SelectPlan, ordinal: usize, row: &Row) -> Row {
    let mut out = vec![Value::Null; logical_join_row_width(plan)];
    let offset = plan.rels[ordinal].offset;
    out[offset..offset + row.len()].clone_from_slice(row);
    out
}

pub(crate) fn combine_physical_relation_rows(
    plan: &SelectPlan,
    outer_ordinal: usize,
    outer: &Row,
    inner_ordinal: usize,
    inner: &Row,
) -> Row {
    let mut out = place_physical_relation_row(plan, outer_ordinal, outer);
    let offset = plan.rels[inner_ordinal].offset;
    out[offset..offset + inner.len()].clone_from_slice(inner);
    out
}

fn physical_step_kind(plan: &SelectPlan, step: &PhysicalJoinStep) -> JoinKind {
    step.on_indices.iter().fold(
        if step.on_indices.is_empty() {
            JoinKind::Cross
        } else {
            JoinKind::Inner
        },
        |kind, on_index| match plan.joins[*on_index].kind {
            JoinKind::Left | JoinKind::Right | JoinKind::Full => plan.joins[*on_index].kind,
            _ => kind,
        },
    )
}

impl Engine {
    pub(crate) fn exec_costed_nway_join(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv<'_>,
        params: &[Value],
        outer_stack: &[&[Value]],
        materialized: &[Vec<Row>],
        step_count: usize,
        stmt_rng: &std::cell::Cell<crate::seam::StmtRng>,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let driver = plan.phys.relation_order[0];
        let mut running: Vec<Row> = materialized[driver]
            .iter()
            .map(|row| place_physical_relation_row(plan, driver, row))
            .collect();
        for (position, step) in plan.phys.join_steps.iter().take(step_count).enumerate() {
            let inner = plan.phys.relation_order[position + 1];
            let inner_inl = plan.phys.rel_inl_bounds[inner].is_some();
            let inner_lateral = plan.rels[inner].lateral;
            let inner_rows = &materialized[inner];
            let step_kind = physical_step_kind(plan, step);
            let emit_left = matches!(step_kind, JoinKind::Left | JoinKind::Full);
            let emit_right = matches!(step_kind, JoinKind::Right | JoinKind::Full);
            let hash_table = step
                .hash_join
                .as_ref()
                .map(|hash| {
                    HashJoinTable::build(hash, plan.rels[inner].offset, 0, inner_rows, meter)
                })
                .transpose()?;
            let mut next = Vec::new();
            let mut right_matched = vec![false; inner_rows.len()];
            for left in &running {
                let inl_rows;
                let lateral_rows;
                let mut lateral_outer_stack;
                let hash_rows;
                let candidates = if inner_inl {
                    inl_rows = self.materialize_rel(
                        plan,
                        inner,
                        params,
                        outer_stack,
                        left,
                        stmt_rng,
                        env.ctes,
                        meter,
                    )?;
                    &inl_rows
                } else if inner_lateral {
                    lateral_outer_stack = outer_stack.to_vec();
                    lateral_outer_stack.push(left);
                    lateral_rows = self.materialize_rel(
                        plan,
                        inner,
                        params,
                        &lateral_outer_stack,
                        &[],
                        stmt_rng,
                        env.ctes,
                        meter,
                    )?;
                    &lateral_rows
                } else if let Some(table) = &hash_table {
                    hash_rows = table
                        .probe(step.hash_join.as_ref().expect("hash step"), left, meter)?
                        .into_iter()
                        .map(|index| inner_rows[index].clone())
                        .collect();
                    &hash_rows
                } else {
                    inner_rows
                };
                let mut left_matched = false;
                for (candidate_index, right) in candidates.iter().enumerate() {
                    let mut combined = left.clone();
                    let offset = plan.rels[inner].offset;
                    combined[offset..offset + right.len()].clone_from_slice(right);
                    let mut keep = true;
                    for &on_index in &step.on_indices {
                        let Some(predicate) = plan.joins[on_index].on.as_ref() else {
                            continue;
                        };
                        if !predicate.eval(&combined, env, meter)?.is_true() {
                            keep = false;
                            break;
                        }
                    }
                    if keep {
                        next.push(combined);
                        left_matched = true;
                        if emit_right {
                            right_matched[candidate_index] = true;
                        }
                    }
                }
                if emit_left && !left_matched {
                    next.push(left.clone());
                }
            }
            if emit_right {
                for (index, right) in inner_rows.iter().enumerate() {
                    if right_matched[index] {
                        continue;
                    }
                    let mut combined = vec![Value::Null; logical_join_row_width(plan)];
                    let offset = plan.rels[inner].offset;
                    combined[offset..offset + right.len()].clone_from_slice(right);
                    next.push(combined);
                }
            }
            running = next;
        }
        Ok(running)
    }

    fn exec_costed_two_relation_join(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv<'_>,
        params: &[Value],
        outer_stack: &[&[Value]],
        materialized: &[Vec<Row>],
        stmt_rng: &std::cell::Cell<crate::seam::StmtRng>,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let outer_ordinal = super::optimize::physical_rel_ordinal(plan, 0);
        let inner_ordinal = super::optimize::physical_rel_ordinal(plan, 1);
        let outer_rows = &materialized[outer_ordinal];
        let inner_inl = plan.phys.rel_inl_bounds[inner_ordinal].is_some();
        let inner_rows = &materialized[inner_ordinal];
        let on = plan.joins[0].on.as_ref();
        let hash_table = if let Some(hash) = &plan.phys.hash_join {
            Some(HashJoinTable::build(
                hash,
                plan.rels[inner_ordinal].offset,
                plan.rels[outer_ordinal].offset,
                inner_rows,
                meter,
            )?)
        } else {
            None
        };

        let mut out = Vec::new();
        for outer_row in outer_rows {
            let candidates: Vec<Row> = if inner_inl {
                let outer_logical = place_physical_relation_row(plan, outer_ordinal, outer_row);
                self.materialize_rel(
                    plan,
                    inner_ordinal,
                    params,
                    outer_stack,
                    &outer_logical,
                    stmt_rng,
                    env.ctes,
                    meter,
                )?
            } else if let Some(table) = &hash_table {
                table
                    .probe(
                        plan.phys.hash_join.as_ref().expect("hash plan"),
                        outer_row,
                        meter,
                    )?
                    .into_iter()
                    .map(|i| inner_rows[i].clone())
                    .collect()
            } else {
                inner_rows.clone()
            };
            for inner_row in &candidates {
                let combined = combine_physical_relation_rows(
                    plan,
                    outer_ordinal,
                    outer_row,
                    inner_ordinal,
                    inner_row,
                );
                let keep = match on {
                    None => true,
                    Some(pred) => pred.eval(&combined, env, meter)?.is_true(),
                };
                if keep {
                    out.push(combined);
                }
            }
        }
        Ok(out)
    }

    /// Run a [`SelectPlan`]'s **blocking part** and return an [`Emitter`] describing how to emit its
    /// output rows (spec/design/streaming.md §4, S4): the scan / join / `WHERE` / window / `ORDER BY`
    /// / `GROUP BY` / `DISTINCT` all run here (charging their cost into `meter`), producing either an
    /// intermediate `Buffer` (windowed, projected lazily on emission) or, for the special
    /// input-streaming paths, a fully-formed `Final` result. The caller drives the emission — eagerly
    /// ([`exec_select_plan`], the materialized `execute()` path) or lazily (`BufferedScan`, the
    /// `query()` path) — so the output `Vec` is built once, by whichever drive is in use. The shared
    /// `stmt_rng` threads the per-statement entropy through both the blocking part and the (possibly
    /// deferred) projection, so a projection-list `uuidv7()`/`now()` draws the identical sequence
    /// whichever drive runs it (streaming.md §6).
    pub(crate) fn exec_select_emit(
        &self,
        plan: &SelectPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
        stmt_rng: &std::cell::Cell<crate::seam::StmtRng>,
        meter: &mut Meter,
    ) -> Result<Emitter> {
        let env = EvalEnv {
            exec: self,
            params,
            outer,
            rng: stmt_rng,
            ctes,
        };

        // Vectorized single-table aggregate (batch, the PAX/vectorization program's executor track): a
        // SUM/COUNT/MIN/MAX/AVG with no DISTINCT / FILTER / HAVING / window / ORDER BY, either
        // whole-table or grouped by a single integer column, folds columnar / int64-bucketed instead
        // of the row-at-a-time group machinery below. Gated to the unmetered lane so a metered query's
        // deterministic abort row stays the scalar path's; results and accrued cost are byte-identical
        // either way (the conformance corpus proves both). Ineligible / metered ⇒ this is skipped and
        // the general aggregate branch runs unchanged. (An aggregate plan skips every streaming
        // fast-path below — they all require `!is_agg` — so this front-position placement is only for
        // clarity, mirroring the Go core's ordering.)
        if meter.is_unmetered() && self.vectorized_agg_eligible(plan) {
            return self.exec_vectorized_agg(plan, &env, meter);
        }

        // Bounded streaming scan (spec/design/cost.md §3): a single-table query whose chosen access
        // path supplies its observable order stops row fetch/filter/project work at the LIMIT window.
        // This covers PK and compatible ordered-index intervals plus GIN/GiST candidate sets (whose
        // gather remains complete). Generalized bounds reach this dispatch from BufferedScan's first
        // pull; the older full/contiguous-PK shape also has a direct pull cursor.
        if streaming_scan_eligible(plan) {
            return Ok(Emitter::Final {
                rows: self.exec_streaming_scan(plan, &env, meter, params)?.rows,
            });
        }

        // Streaming secondary-index-order scan (cost.md §3 "secondary-index order"): compatible
        // bounded plans were caught above, so this fallback handles the no-bound LIMIT shape. Walk
        // the ordering index + point-lookup; the eager sort is elided.
        if let Some(io) = &plan.phys.index_order {
            return Ok(Emitter::Final {
                rows: self.exec_index_order_scan(plan, io, &env, meter)?.rows,
            });
        }

        // Streaming external sort (spec/design/spill.md §5): a single-table, no-join,
        // non-aggregate, non-DISTINCT query with an ORDER BY the scan does NOT already satisfy
        // (`!plan.phys.pk_ordered` — caught above) streams scan→filter→Sorter, so the input is never
        // materialized in the executor heap and the sort spills sorted runs to disk under work_mem
        // (file-backed databases). DISTINCT/aggregate/join take the eager path below, and an
        // incompatible index bound does not stream through this sorter. Results + cost are identical
        // to the eager sort (the sort is unmetered — cost.md §3; spill.md §6).
        if !plan.order.is_empty()
            && !plan.phys.pk_ordered
            && plan.order_exprs.is_empty() // a materialized expression key takes the eager path below
            && plan.rels.len() == 1
            && plan.joins.is_empty()
            && !plan.is_agg
            && !plan.has_window
            && !plan.distinct
            && !matches!(
                plan.phys.rel_bounds[0],
                Some(ScanBound::Index(_))
                | Some(ScanBound::Gin(_))
                | Some(ScanBound::Gist(_))
                | Some(ScanBound::PkSet(_))
                | Some(ScanBound::IndexSet(_))
            )
            // A set-returning relation takes the eager path (functions.md §10).
            && plan.rels[0].srf.is_none()
            // A CTE reference takes the eager path (cte.md §5).
            && plan.rels[0].cte.is_none()
            // A derived table takes the eager path (grammar.md §42).
            && plan.rels[0].derived.is_none()
        {
            // The streaming sort yields its output LAZILY (streaming.md §4/§7) — `exec_streaming_sort`
            // runs the scan + sort + OFFSET skip and returns an `Emitter::Sorted` over the `SortedRows`
            // pull iterator; the window's row_produced + projection is charged by the emitter drive.
            return self.exec_streaming_sort(plan, &env, meter, params);
        }

        // Streaming two-table join (cost.md §3 "JOIN"): the planner set `join_pk_ordered` only for a
        // two-table INNER/CROSS join whose ORDER BY the OUTER relation's PK scan order satisfies, with
        // a LIMIT. The join drives/probes the outer in PK order so the output is already ordered — the
        // sort is elided and the loop short-circuits a top-N.
        if plan.phys.join_pk_ordered {
            return Ok(Emitter::Final {
                rows: if plan.phys.join_steps.len() + 1 == plan.rels.len() && plan.rels.len() >= 3 {
                    self.exec_streaming_nway_join(plan, &env, meter, params, outer, stmt_rng)?
                        .rows
                } else {
                    self.exec_streaming_join(plan, &env, meter, params, outer, stmt_rng)?
                        .rows
                },
            });
        }

        // Windowed top-N (spec/design/window.md §5.2, cost.md §3): a plain window query whose LIMIT is
        // answerable from the first OFFSET+LIMIT PK-scan rows (a backward window over the PK-ordered
        // scan) scans only that prefix instead of the whole table — the window analog of the streaming
        // LIMIT short-circuit. Ineligible window queries fall through to the eager materialize below.
        if self.window_top_n_eligible(plan) {
            return self.exec_window_top_n(plan, &env, meter, params);
        }

        // Columnar projection fast path (batch, packed-leaf.md §11 Track A2/A3): a bare-column projection
        // over a single-table full/PK-bounded scan with no ORDER BY / LIMIT / OFFSET / blocking operator
        // gathers only its touched columns into dense lanes and emits from them — never the full-width row
        // the materialize path below allocates per record (the allocation dividend on a wide table). A
        // WHERE predicate (A3) is applied over the lanes into a selection vector rather than forcing the row
        // path. Gated to the unmetered lane (so a metered query's per-eval guards stay the row path's) and
        // to file-backed stores with no spillable touched column (project_columnar declines otherwise,
        // falling through to the identical-cost row path). Cost-neutral by construction.
        if meter.is_unmetered() && vectorized_project_eligible(plan) {
            if let Some(em) = self.project_columnar(plan, &env, params, outer, meter)? {
                return Ok(em);
            }
        }

        // Materialize each relation once, in primary-key order (base tables drain a ScanSource — the
        // page_read block + per-row storage_row_read accrue inside next(), cost.md §3). The nested
        // loop re-reads from these in-memory buffers, which are not stores and charge nothing. A
        // CORRELATED `LATERAL` relation (§44) depends on the left-hand row, so it cannot be
        // materialized up front — a placeholder holds its slot and the join loop re-materializes it
        // per combined left row.
        // An INDEX-NESTED-LOOP relation (cost.md §3 "JOIN") likewise depends on the left-hand row
        // (its bound seeks per outer row), so it is not materialized up front either — a placeholder
        // holds its slot and the join loop re-materializes it per left row.
        let mut materialized: Vec<Vec<Row>> = Vec::with_capacity(plan.rels.len());
        for (ri, rel) in plan.rels.iter().enumerate() {
            if rel.lateral || plan.phys.rel_inl_bounds[ri].is_some() {
                materialized.push(Vec::new());
                continue;
            }
            materialized.push(self.materialize_rel(
                plan,
                ri,
                params,
                outer,
                &[],
                stmt_rng,
                env.ctes,
                meter,
            )?);
        }

        // Left-deep nested-loop join. `running` holds the combined rows over the relations
        // joined so far (starting with the first table's rows). For each join, concatenate
        // every running row with every right-table row; CROSS keeps all pairs, INNER keeps a
        // pair iff its ON predicate is TRUE (three-valued — a NULL join key never matches).
        // LEFT/FULL additionally emit each unmatched left row NULL-extended over the right
        // side; RIGHT/FULL emit each unmatched right row NULL-extended over the left side.
        // The NULL-extension pushes evaluate no ON (no operator_eval — spec/design/cost.md §3).
        // Output order is deterministic: running order (outer) then right key order (inner),
        // each unmatched left row after its (empty) match run, all unmatched right rows last in
        // right key order — so a join is deterministic even with no ORDER BY (CLAUDE.md §10).
        // A FROM-less SELECT has no relations: seed `running` with ONE virtual zero-column row
        // instead of a table's rows (grammar.md §34). No scan ran, so no scan cost accrued.
        let mut running: Vec<Row>;
        if plan.phys.join_steps.len() + 1 == plan.rels.len()
            && plan.phys.relation_order.len() == plan.rels.len()
            && plan.rels.len() >= 3
        {
            running = self.exec_costed_nway_join(
                plan,
                &env,
                params,
                outer,
                &materialized,
                plan.phys.join_steps.len(),
                stmt_rng,
                meter,
            )?;
        } else if plan.phys.relation_order.len() == 2 {
            running = self.exec_costed_two_relation_join(
                plan,
                &env,
                params,
                outer,
                &materialized,
                stmt_rng,
                meter,
            )?;
        } else {
            running = if plan.rels.is_empty() {
                vec![Vec::new()]
            } else {
                std::mem::take(&mut materialized[0])
            };
            for (k, pj) in plan.joins.iter().enumerate() {
                let on = &pj.on;
                let emit_left = matches!(pj.kind, JoinKind::Left | JoinKind::Full);
                let emit_right = matches!(pj.kind, JoinKind::Right | JoinKind::Full);
                // NULL-pad widths come from the PLAN, never a sampled row, so they are correct even
                // when `running`/`right_rows` is empty: the right table begins at flat offset
                // rels[k+1].offset (= the width of every running row) and is that many columns wide.
                let left_pad = plan.rels[k + 1].offset;
                let right_pad = plan.rels[k + 1].col_count;
                let mut next: Vec<Row> = Vec::new();
                // A CORRELATED LATERAL relation (§44): re-materialize it ONCE PER combined left-hand row,
                // with that row pushed onto the outer-row stack as the body's immediate outer (the
                // correlated-subquery mechanism). The plan guarantees INNER/CROSS/LEFT here (RIGHT/FULL
                // to a correlated lateral is 42P10), so there is no unmatched-right emission.
                if plan.rels[k + 1].lateral {
                    for left in &running {
                        let mut lat_outer: Vec<&[Value]> = outer.to_vec();
                        lat_outer.push(left);
                        let right_rows = self.materialize_rel(
                            plan,
                            k + 1,
                            params,
                            &lat_outer,
                            &[],
                            stmt_rng,
                            env.ctes,
                            meter,
                        )?;
                        let mut left_matched = false;
                        for right in &right_rows {
                            let mut combined = left.clone();
                            combined.extend_from_slice(right);
                            let keep = match on {
                                None => true,
                                Some(pred) => pred.eval(&combined, &env, meter)?.is_true(),
                            };
                            if keep {
                                next.push(combined);
                                left_matched = true;
                            }
                        }
                        if emit_left && !left_matched {
                            let mut combined = left.clone();
                            combined.resize(combined.len() + right_pad, Value::Null);
                            next.push(combined);
                        }
                    }
                    running = next;
                    continue;
                }
                // An INDEX-NESTED-LOOP inner relation (cost.md §3 "JOIN"): re-materialize it ONCE PER
                // combined left-hand row, its scan bounded per outer row by the `Sibling` columns of that
                // row (a per-outer-row seek instead of a full scan). Detection restricts this to the
                // RIGHT/nullable side of an INNER/CROSS/LEFT join, so there is never an unmatched-RIGHT
                // emission (RIGHT/FULL are excluded — a preserved side cannot be bounded per outer row).
                // The whole ON/WHERE stays applied (the ON here, the WHERE below), so rows are unchanged.
                if plan.phys.rel_inl_bounds[k + 1].is_some() {
                    debug_assert!(!emit_right, "index-nested-loop excludes RIGHT/FULL joins");
                    for left in &running {
                        let right_rows = self.materialize_rel(
                            plan,
                            k + 1,
                            params,
                            outer,
                            left,
                            stmt_rng,
                            env.ctes,
                            meter,
                        )?;
                        let mut left_matched = false;
                        for right in &right_rows {
                            let mut combined = left.clone();
                            combined.extend_from_slice(right);
                            let keep = match on {
                                None => true,
                                Some(pred) => pred.eval(&combined, &env, meter)?.is_true(),
                            };
                            if keep {
                                next.push(combined);
                                left_matched = true;
                            }
                        }
                        if emit_left && !left_matched {
                            let mut combined = left.clone();
                            combined.resize(combined.len() + right_pad, Value::Null);
                            next.push(combined);
                        }
                    }
                    running = next;
                    continue;
                }
                let right_rows = &materialized[k + 1];
                // The hash rule is exactly two inputs, so it can only own join 0. Build the right input
                // once, then probe running rows in left order. Bucket indices retain right order and the
                // full ON is rechecked, reproducing nested-loop enumeration and LEFT null-extension.
                if k == 0
                    && let Some(hash_plan) = &plan.phys.hash_join
                {
                    let table = HashJoinTable::build(
                        hash_plan,
                        plan.rels[1].offset,
                        plan.rels[0].offset,
                        right_rows,
                        meter,
                    )?;
                    let pred = on.as_ref().expect("a hash join always has an ON predicate");
                    for left in &running {
                        let candidates = table.probe(hash_plan, left, meter)?;
                        let mut left_matched = false;
                        for ri in candidates {
                            let mut combined = left.clone();
                            combined.extend_from_slice(&right_rows[ri]);
                            if pred.eval(&combined, &env, meter)?.is_true() {
                                next.push(combined);
                                left_matched = true;
                            }
                        }
                        if emit_left && !left_matched {
                            let mut combined = left.clone();
                            combined.resize(combined.len() + right_pad, Value::Null);
                            next.push(combined);
                        }
                    }
                    running = next;
                    continue;
                }
                let mut right_matched = vec![false; right_rows.len()];
                for left in &running {
                    let mut left_matched = false;
                    for (ri, right) in right_rows.iter().enumerate() {
                        let mut combined = left.clone();
                        combined.extend_from_slice(right);
                        let keep = match on {
                            None => true,
                            Some(pred) => pred.eval(&combined, &env, meter)?.is_true(),
                        };
                        if keep {
                            next.push(combined);
                            left_matched = true;
                            right_matched[ri] = true;
                        }
                    }
                    if emit_left && !left_matched {
                        let mut combined = left.clone();
                        combined.resize(combined.len() + right_pad, Value::Null);
                        next.push(combined);
                    }
                }
                if emit_right {
                    for (ri, right) in right_rows.iter().enumerate() {
                        if !right_matched[ri] {
                            let mut combined: Row = vec![Value::Null; left_pad];
                            combined.extend_from_slice(right);
                            next.push(combined);
                        }
                    }
                }
                running = next;
            }
        }

        // WHERE over the combined rows (consume `running`, no extra clone). A WHERE arithmetic
        // can trap (22003/22012); each surviving combined row's filter accrues operator_eval.
        let mut rows: Vec<Row> = Vec::new();
        for row in running {
            let keep = match &plan.filter {
                None => true,
                Some(f) => f.eval(&row, &env, meter)?.is_true(),
            };
            if keep {
                rows.push(row);
            }
        }

        // WINDOW stage (spec/design/window.md §5.2): a blocking operator over the post-WHERE rows,
        // running BEFORE the query ORDER BY / DISTINCT / LIMIT. Each window function's per-row
        // result is APPENDED to its row (so the projection reads result `i` at flat slot
        // `input_width + i`); the rows keep their scan order, and the query ORDER BY below re-sorts
        // the extended rows. A window query never enters the streaming fast-paths above. A GROUPED
        // window query (§2) runs the window stage over the GROUPED rows instead, inside the aggregate
        // branch below (after GROUP BY/HAVING) — so it is gated to plain (non-aggregate) windows here.
        if plan.has_window && !plan.is_agg {
            apply_window_stage(
                &mut rows,
                &plan.window_specs,
                &plan.window_keys,
                &env,
                meter,
            )?;
        }

        // ORDER BY: a stable sort applying each key left to right — the first non-equal key
        // decides, and a full tie keeps the scan order (the sort is stable). Each key's NULL
        // placement is decoupled from its value-direction flip, so an explicit NULLS
        // FIRST|LAST overrides the default (spec/design/grammar.md §10).
        // (Aggregate queries sort their GROUP rows in the aggregate branch below — not these
        // pre-aggregation rows — so the sort here is gated to plain queries.)
        if !plan.is_agg && !plan.order.is_empty() {
            // Materialize each general-expression ORDER BY key (grammar.md §10): evaluate it against the
            // post-WHERE (post-window) row and append the value, so its sort slot `final_width + k` reads
            // the appended column and the slot-based sort below is unchanged — the window-key precedent.
            // The evaluation is metered per node (cost.md §3); empty for a column/ordinal-only ORDER BY.
            materialize_order_exprs(&mut rows, &plan.order_exprs, &env, meter)?;
            if let Some(k) = plan.phys.top_k {
                rows = top_k_rows(rows, &plan.order, k)?;
            } else {
                sort_rows(&mut rows, &plan.order)?;
            }
        }

        // LIMIT / OFFSET window bounds over a result of `len` rows. Clamp in the integer
        // domain against the row count before indexing — never truncate a huge count into
        // usize (CLAUDE.md §8; spec/design/grammar.md §9). The counts are already
        // non-negative (parser).
        let window_bounds = |len: usize| -> (usize, usize) {
            let start = plan.offset.unwrap_or(0).min(len as i64) as usize;
            let end = match plan.limit {
                Some(lim) if lim < (len - start) as i64 => start + lim as usize,
                _ => len,
            };
            (start, end)
        };

        // Build the output rows. The two paths differ in pipeline order
        // (spec/design/grammar.md §11): without DISTINCT the window slices the sorted
        // source rows and ONLY the windowed rows are projected; with DISTINCT every
        // (sorted) filtered row is projected — dedup must see them all — duplicates drop
        // by first occurrence, and the window then slices the DISTINCT rows.
        let emitter = if plan.is_agg {
            // Aggregate query — group + accumulate (aggregates.md §5). Fold every filtered row into
            // the accumulators — charging aggregate_accumulate per (row × aggregate) and the
            // operand's own operator_evals — then finalize to the synthetic row [agg_0..] and
            // project it. Even an empty input yields ONE group row (COUNT 0, others NULL —
            // spec/design/aggregates.md §4). The bucketing/finalize is unmetered (cost.md §3).
            // Bucket the post-WHERE rows by their group-key values. The bucket key is the
            // value-canonical Vec<Value> (its Eq/Hash collapse 1.5/1.50 and group NULL with
            // NULL — value.rs); the map is only an index, never iterated, so output order comes
            // from the insertion-ordered `groups` (no hashmap-order leak — CLAUDE.md §8/§10).
            // Whole-table aggregation (no GROUP BY) is one pre-created empty-key group, so it
            // emits ONE row even over zero input (COUNT 0, others NULL); GROUP BY over an empty
            // table creates no groups -> zero rows.
            // Per group: its key values, one Acc per aggregate, and one DISTINCT dedup set per
            // DISTINCT aggregate (`None` for a plain aggregate — `COUNT(DISTINCT x)`,
            // aggregates.md §5). The set is value-canonical (`Value`'s own Eq/Hash, so `1.5`/`1.50`
            // and `-0`/`+0` collapse, identical to the group-key bucketing).
            let new_accs = || -> Vec<Acc> { plan.agg_specs.iter().map(Acc::from_spec).collect() };
            let new_seen = || -> Vec<Option<HashSet<Value>>> {
                plan.agg_specs
                    .iter()
                    .map(|s| s.distinct.then(HashSet::new))
                    .collect()
            };
            // One grouping set per `plan.group_sets` (spec/design/aggregates.md §12): a plain GROUP BY
            // / whole-table aggregate has exactly one; ROLLUP / CUBE / GROUPING SETS have several. Each
            // set is bucketed independently over the SAME post-WHERE rows and its groups projected into
            // the shared synthetic row `[master_columns…, aggregate_results…, GROUPING_results…]`: a
            // column not grouped in this set is NULL, and each GROUPING() call's value comes from this
            // set's mask. The scan (storage_row_read) is upstream and counted once; aggregate_accumulate
            // + operand evals accrue per (set × row × passing aggregate). The per-set bucket index is
            // never iterated — output order comes from the insertion-ordered `groups` then the set order
            // (no hashmap-order leak — CLAUDE.md §8/§10).
            // Materialize the general-expression GROUP BY keys (aggregates.md §15): evaluate each per
            // post-WHERE row ONCE (charging its operator_evals, like an aggregate operand) and append
            // the value at flat slot `input_width + k`, so a master grouping-key index pointing there
            // reads it. Done before the (possibly multi-) grouping-set loop so each row is extended
            // once and the values are shared across sets. A plain column GROUP BY appends nothing.
            if !plan.group_exprs.is_empty() {
                for row in rows.iter_mut() {
                    meter.guard()?;
                    for ge in &plan.group_exprs {
                        let v = ge.eval(row, &env, meter)?;
                        row.push(v);
                    }
                }
            }
            let mut group_rows: Vec<Vec<Value>> = Vec::new();
            for gset in &plan.group_sets {
                let mut index: HashMap<Vec<Value>, usize> = HashMap::new();
                let mut groups: Vec<(Vec<Value>, Vec<Acc>, Vec<Option<HashSet<Value>>>)> =
                    Vec::new();
                // An empty grouping set (the `()` / whole-table grand total) is one pre-created group,
                // so it emits ONE row even over zero input; a non-empty set over an empty input emits
                // nothing (spec/design/aggregates.md §4/§12).
                if gset.key_cols.is_empty() {
                    groups.push((Vec::new(), new_accs(), new_seen()));
                    index.insert(Vec::new(), 0);
                }
                for row in &rows {
                    meter.guard()?; // enforce the cost ceiling per folded row (CLAUDE.md §13)
                    let key: Vec<Value> = gset.key_cols.iter().map(|&gk| row[gk].clone()).collect();
                    let gi = match index.get(&key) {
                        Some(&i) => i,
                        None => {
                            let i = groups.len();
                            index.insert(key.clone(), i);
                            groups.push((key, new_accs(), new_seen()));
                            i
                        }
                    };
                    for (si, spec) in plan.agg_specs.iter().enumerate() {
                        // FILTER (WHERE cond): a row for which the filter is not TRUE (FALSE or NULL)
                        // contributes nothing to THIS aggregate — its operand is not evaluated and it
                        // is not accumulated (aggregates.md §11). The filter's own operator_evals are
                        // charged (it is evaluated per row, like the operand); aggregate_accumulate is
                        // charged only for a row that passes. The pass/fold decision is deterministic
                        // (scan order is cross-core identical), so the metered cost is identical.
                        if let Some(f) = &spec.filter
                            && !f.eval(row, &env, meter)?.is_true()
                        {
                            continue;
                        }
                        meter.charge(COSTS.aggregate_accumulate);
                        // A hypothetical-set aggregate (rank/dense_rank/… — aggregates.md §19) buffers
                        // the row's WITHIN GROUP key TUPLE (no NULL-skip — every row counts, sorted by
                        // NULLS FIRST/LAST). The hypothetical row itself is evaluated per group at
                        // finalize. No DISTINCT (rejected at resolve).
                        if let Some(hp) = &spec.hypo {
                            let tuple = hp
                                .keys
                                .iter()
                                .map(|k| k.eval(row, &env, meter))
                                .collect::<Result<Vec<Value>>>()?;
                            if let Acc::Hypothetical { rows, .. } = &mut groups[gi].1[si] {
                                rows.push(tuple);
                            }
                            continue;
                        }
                        let v = match &spec.operand {
                            Some(op) => op.eval(row, &env, meter)?,
                            None => Value::Null, // COUNT(*) ignores the value
                        };
                        // DISTINCT: skip a NULL (never folded by any aggregate) and any value already
                        // folded into this group — the FIRST occurrence in scan order wins, so the set
                        // of folded values (and the decimal_work fold charges) is order-deterministic
                        // and cross-core identical. `insert` returns false when the value is a repeat.
                        if let Some(seen) = &mut groups[gi].2[si]
                            && (matches!(v, Value::Null) || !seen.insert(v.clone()))
                        {
                            continue;
                        }
                        groups[gi].1[si].fold(v, meter)?;
                    }
                }
                // Build one synthetic row per group of this set: each master grouping column's value
                // (NULL where this set doesn't group it), then the aggregate results, then each
                // GROUPING() value (computed from this set's mask — spec/design/aggregates.md §12).
                for (key, accs, _seen) in groups {
                    let mut srow: Vec<Value> = Vec::with_capacity(
                        plan.group_keys.len() + plan.agg_specs.len() + plan.grouping_specs.len(),
                    );
                    for src in &gset.slot_src {
                        srow.push(match src {
                            Some(j) => key[*j].clone(),
                            None => Value::Null,
                        });
                    }
                    for (si, mut acc) in accs.into_iter().enumerate() {
                        // An ordered-set aggregate's percentile fraction (the direct argument) is
                        // evaluated PER GROUP here, against the synthetic row's grouping-key values
                        // (aggregates.md §13/§17) — so it may reference grouping columns. Unmetered
                        // (the finalize step, like the sort), via a scratch meter. `mode` has none.
                        if let Acc::OrderedSet { frac, .. } = &mut acc
                            && let Some(osa) = &plan.agg_specs[si].osa
                            && let Some(fe) = &osa.frac
                        {
                            *frac = Some(fe.eval(&srow, &env, &mut Meter::new())?);
                        }
                        // A hypothetical-set aggregate is finalized here (not via `Acc::finalize`)
                        // because it needs the spec's per-key sort specs: evaluate the hypothetical
                        // row's direct args per group (against the synthetic row, like a fraction),
                        // then count its rank among the buffered key tuples (aggregates.md §19).
                        let result = if let Acc::Hypothetical { kind, rows, .. } = &acc {
                            let hp = plan.agg_specs[si]
                                .hypo
                                .as_ref()
                                .expect("a hypothetical plan carries its HypoParams");
                            let hyp = hp
                                .args
                                .iter()
                                .map(|a| a.eval(&srow, &env, &mut Meter::new()))
                                .collect::<Result<Vec<Value>>>()?;
                            finalize_hypothetical(*kind, rows, &hyp, &hp.sorts)?
                        } else {
                            acc.finalize()?
                        };
                        srow.push(result);
                    }
                    for positions in &plan.grouping_specs {
                        srow.push(Value::Int(grouping_value(positions, gset.mask)));
                    }
                    group_rows.push(srow);
                }
            }
            // HAVING: filter the grouped rows (after aggregation, before ORDER BY). The
            // predicate is evaluated against each group's synthetic row (charging its
            // operator_evals per group); only a TRUE result keeps the group. A dropped group
            // then charges no row_produced (spec/design/aggregates.md §8).
            if let Some(h) = &plan.having {
                let mut kept: Vec<Vec<Value>> = Vec::with_capacity(group_rows.len());
                for srow in group_rows {
                    if h.eval(&srow, &env, meter)?.is_true() {
                        kept.push(srow);
                    }
                }
                group_rows = kept;
            }
            // WINDOW stage over the grouped rows (spec/design/window.md §2): runs AFTER GROUP
            // BY/HAVING and BEFORE the query ORDER BY. It appends each window result to the grouped
            // row [group_keys…, agg_results…], so the projection reads window result `w` at slot
            // group_keys.len()+agg_specs.len()+w (the rebased slot — §5.1). The group-key slots the
            // ORDER BY below sorts on are unchanged (they precede the appended results).
            if plan.has_window {
                apply_window_stage(
                    &mut group_rows,
                    &plan.window_specs,
                    &plan.window_keys,
                    &env,
                    meter,
                )?;
            }
            // ORDER BY over the grouped output (a column/ordinal key is a synthetic group-key slot; an
            // expression key is materialized against the grouped row and appended — grammar.md §10).
            if !plan.order.is_empty() {
                materialize_order_exprs(&mut group_rows, &plan.order_exprs, &env, meter)?;
                sort_rows(&mut group_rows, &plan.order)?;
            }
            if plan.distinct {
                // SELECT DISTINCT: project EVERY grouped row (charging its projection operator_evals,
                // the §3 asymmetry — like the non-aggregate DISTINCT path below), dedup by equality
                // keeping the first occurrence in the (already ORDER-BY-sorted) order, then apply
                // LIMIT/OFFSET. `seen` is membership-only; output order comes from the deterministic
                // group iteration / sort, never set iteration (no hashmap-order leak — CLAUDE.md §8/§10).
                let mut seen: std::collections::HashSet<Vec<Value>> =
                    std::collections::HashSet::new();
                let mut distinct_rows: Vec<Vec<Value>> = Vec::new();
                for srow in &group_rows {
                    let mut out = Vec::with_capacity(plan.projections.len());
                    for p in &plan.projections {
                        out.push(p.eval(srow, &env, meter)?);
                    }
                    if seen.insert(out.clone()) {
                        distinct_rows.push(out);
                    }
                }
                // The dedup already projected every grouped row (the §3 asymmetry, charged above), so
                // emission is Identity — window + charge row_produced, deferred to the drive (streaming.md §4).
                let (start, end) = window_bounds(distinct_rows.len());
                Emitter::Buffer {
                    rows: distinct_rows,
                    start,
                    end,
                    mode: EmitMode::Identity,
                }
            } else {
                // Window then project on emission; only an emitted row charges row_produced +
                // projection cost. Deferred to the drive (streaming.md §4).
                let (start, end) = window_bounds(group_rows.len());
                Emitter::Buffer {
                    rows: group_rows,
                    start,
                    end,
                    mode: EmitMode::Project,
                }
            }
        } else if plan.distinct {
            // Project every filtered row (charging projection cost per row, the §3
            // asymmetry), keeping first occurrences. `seen` is membership-only: the
            // output order comes from the deterministic source iteration, never from set
            // iteration (no hashmap-order leak — CLAUDE.md §8/§10).
            let mut seen: std::collections::HashSet<Vec<Value>> = std::collections::HashSet::new();
            let mut distinct_rows: Vec<Vec<Value>> = Vec::new();
            for row in &rows {
                let mut out = Vec::with_capacity(plan.projections.len());
                for p in &plan.projections {
                    out.push(p.eval(row, &env, meter)?);
                }
                if seen.insert(out.clone()) {
                    distinct_rows.push(out);
                }
            }
            // LIMIT / OFFSET applies to the DISTINCT rows; only the emitted rows charge row_produced
            // (spec/design/cost.md §3). The rows were already projected for their dedup key (the §3
            // asymmetry, charged above), so emission is Identity — deferred to the drive (streaming.md §4).
            let (start, end) = window_bounds(distinct_rows.len());
            Emitter::Buffer {
                rows: distinct_rows,
                start,
                end,
                mode: EmitMode::Identity,
            }
        } else {
            // Window the sorted rows BEFORE projection, so rows skipped by OFFSET or excluded by LIMIT
            // accrue no row_produced/projection cost (they were still scanned + filtered above).
            // Producing a row, and each projection-list evaluation, accrue cost on emission — deferred
            // to the drive (streaming.md §4). (ORDER BY's sort comparisons are not metered — cost.md §3.)
            let (start, end) = window_bounds(rows.len());
            Emitter::Buffer {
                rows,
                start,
                end,
                mode: EmitMode::Project,
            }
        };
        Ok(emitter)
    }

    // ---- Uncorrelated subquery folding (spec/design/grammar.md §26) ----------------------
    //
    // After the whole statement tree is planned + the parameters bound, this bottom-up pass
    // walks every `RExpr::Subquery` node in the plan tree: it first folds within the node's own
    // sub-plan, then — if the subquery references NO enclosing scope (a global constant, PG's
    // "initplan") — executes it ONCE and replaces it with a constant (scalar -> its value;
    // EXISTS -> a boolean; IN -> an `InValues` over the result column), accruing the subquery's
    // cost once (preserving the committed once-only cost — cost.md §3). A CORRELATED subquery is
    // left in place; the evaluator re-executes it per outer row. So after this pass the only
    // surviving `Subquery` nodes are correlated.

    pub(crate) fn fold_uncorrelated_in_plan(
        &self,
        plan: &mut QueryPlan,
        bound: &[Value],
        ctes: CteCtx,
        cost: &mut i64,
    ) -> Result<()> {
        match plan {
            QueryPlan::Select(sp) => self.fold_uncorrelated_in_select(sp, bound, ctes, cost),
            QueryPlan::SetOp(sop) => {
                self.fold_uncorrelated_in_plan(&mut sop.lhs, bound, ctes, cost)?;
                self.fold_uncorrelated_in_plan(&mut sop.rhs, bound, ctes, cost)
            }
            // A VALUES-body value may itself hold an (uncorrelated) scalar subquery to fold once
            // before the rows are produced (grammar.md §42; the §26 fold).
            QueryPlan::Values(vp) => {
                for row in &mut vp.rows {
                    for e in row {
                        self.fold_uncorrelated_in_rexpr(e, bound, ctes, cost)?;
                    }
                }
                Ok(())
            }
            // A nested `WITH` body is not folded here against the enclosing `ctes` — its inner
            // subqueries reference the nested CTEs (a different scope, materialized only when the
            // node runs), so they are left to the evaluator, exactly like a derived table's body
            // (spec/design/cte.md §7). The whole nested-WITH subquery is itself folded by the caller
            // if it is uncorrelated (executed once via `exec_with_plan`).
            QueryPlan::With(_) => Ok(()),
        }
    }

    pub(crate) fn fold_uncorrelated_in_select(
        &self,
        sp: &mut SelectPlan,
        bound: &[Value],
        ctes: CteCtx,
        cost: &mut i64,
    ) -> Result<()> {
        for j in &mut sp.joins {
            if let Some(on) = &mut j.on {
                self.fold_uncorrelated_in_rexpr(on, bound, ctes, cost)?;
            }
        }
        if let Some(f) = &mut sp.filter {
            self.fold_uncorrelated_in_rexpr(f, bound, ctes, cost)?;
        }
        if let Some(h) = &mut sp.having {
            self.fold_uncorrelated_in_rexpr(h, bound, ctes, cost)?;
        }
        for s in &mut sp.agg_specs {
            if let Some(op) = &mut s.operand {
                self.fold_uncorrelated_in_rexpr(op, bound, ctes, cost)?;
            }
        }
        for p in &mut sp.projections {
            self.fold_uncorrelated_in_rexpr(p, bound, ctes, cost)?;
        }
        // A set-returning relation's arguments may themselves contain an (uncorrelated) subquery
        // to fold once before the generator runs (functions.md §10).
        for r in &mut sp.rels {
            if let Some(srf) = &mut r.srf {
                for a in &mut srf.args {
                    self.fold_uncorrelated_in_rexpr(a, bound, ctes, cost)?;
                }
            }
        }
        Ok(())
    }

    /// Fold this node if it is an uncorrelated `Subquery`, else recurse into its children.
    pub(crate) fn fold_uncorrelated_in_rexpr(
        &self,
        e: &mut RExpr,
        bound: &[Value],
        ctes: CteCtx,
        cost: &mut i64,
    ) -> Result<()> {
        if matches!(e, RExpr::Subquery { .. }) {
            // Bottom-up: fold within this subquery's own sub-plan (and its IN lhs) first, so a
            // globally-uncorrelated subquery nested inside it is already a constant before we run
            // it. Then leave it untouched if it is correlated (re-run per outer row at eval).
            if let RExpr::Subquery { plan, lhs, .. } = e {
                if let Some(l) = lhs {
                    self.fold_uncorrelated_in_rexpr(l, bound, ctes, cost)?;
                }
                self.fold_uncorrelated_in_plan(plan, bound, ctes, cost)?;
                if query_plan_references_outer(plan, 0) {
                    return Ok(());
                }
            }
            // Uncorrelated: execute ONCE and fold to a constant / InValues. Take ownership so the
            // sub-plan can be moved/run without aliasing the node we are about to overwrite.
            let taken = std::mem::replace(e, RExpr::ConstNull);
            let RExpr::Subquery {
                plan,
                kind,
                lhs,
                negated,
            } = taken
            else {
                unreachable!("guarded by matches! above")
            };
            let r = self.exec_query_plan(&plan, &[], bound, ctes)?;
            *cost += r.cost;
            *e = match kind {
                SubqueryKind::Scalar => {
                    if r.rows.len() > 1 {
                        return Err(EngineError::new(
                            SqlState::CardinalityViolation,
                            "more than one row returned by a subquery used as an expression",
                        ));
                    }
                    let value = r
                        .rows
                        .into_iter()
                        .next()
                        .map(|mut row| row.swap_remove(0))
                        .unwrap_or(Value::Null);
                    value_to_rexpr(&value)
                }
                SubqueryKind::Exists => RExpr::ConstBool(!r.rows.is_empty() != negated),
                SubqueryKind::In => {
                    let list: Vec<Value> = r
                        .rows
                        .into_iter()
                        .map(|mut row| row.swap_remove(0))
                        .collect();
                    RExpr::InValues {
                        lhs: lhs.expect("an IN subquery carries its resolved lhs"),
                        list,
                        negated,
                    }
                }
                // An uncorrelated quantified subquery folds to a constant-array `Quantified`
                // (array-functions.md §11.6): its single column becomes a 1-D array and the node
                // reuses the array form's 3VL fold — no per-row re-execution.
                SubqueryKind::Quantified { op, all } => {
                    let elements: Vec<Value> = r
                        .rows
                        .into_iter()
                        .map(|mut row| row.swap_remove(0))
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
                    RExpr::Quantified {
                        op,
                        all,
                        lhs: lhs.expect("a quantified subquery carries its resolved lhs"),
                        array: Box::new(RExpr::ConstArray(Box::new(arr))),
                    }
                }
            };
            return Ok(());
        }
        match e {
            RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => {
                self.fold_uncorrelated_in_rexpr(inner, bound, ctes, cost)
            }
            RExpr::Neg { operand, .. } => {
                self.fold_uncorrelated_in_rexpr(operand, bound, ctes, cost)
            }
            RExpr::Not(x) => self.fold_uncorrelated_in_rexpr(x, bound, ctes, cost),
            RExpr::Casing { arg, .. } => self.fold_uncorrelated_in_rexpr(arg, bound, ctes, cost),
            RExpr::AtTimeZone { zone, value, .. } => {
                self.fold_uncorrelated_in_rexpr(zone, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(value, bound, ctes, cost)
            }
            RExpr::DateTrunc { unit, value, zone } => {
                self.fold_uncorrelated_in_rexpr(unit, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(value, bound, ctes, cost)?;
                if let Some(z) = zone {
                    self.fold_uncorrelated_in_rexpr(z, bound, ctes, cost)?;
                }
                Ok(())
            }
            RExpr::Extract { value, .. } => {
                self.fold_uncorrelated_in_rexpr(value, bound, ctes, cost)
            }
            RExpr::DateConvert { inner, .. } => {
                self.fold_uncorrelated_in_rexpr(inner, bound, ctes, cost)
            }
            RExpr::Arith { lhs, rhs, .. }
            | RExpr::Compare { lhs, rhs, .. }
            | RExpr::Distinct { lhs, rhs, .. }
            | RExpr::Like { lhs, rhs, .. }
            | RExpr::Regex { lhs, rhs, .. } => {
                self.fold_uncorrelated_in_rexpr(lhs, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(rhs, bound, ctes, cost)
            }
            RExpr::And(l, r) | RExpr::Or(l, r) => {
                self.fold_uncorrelated_in_rexpr(l, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(r, bound, ctes, cost)
            }
            RExpr::JsonGet { base, arg, .. }
            | RExpr::JsonHasKey { base, arg, .. }
            | RExpr::JsonDelete { base, arg, .. } => {
                self.fold_uncorrelated_in_rexpr(base, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(arg, bound, ctes, cost)
            }
            RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
                self.fold_uncorrelated_in_rexpr(a, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(b, bound, ctes, cost)
            }
            RExpr::IsNull { operand, .. }
            | RExpr::IsJson { operand, .. }
            | RExpr::JsonCtor { operand, .. } => {
                self.fold_uncorrelated_in_rexpr(operand, bound, ctes, cost)
            }
            RExpr::Case { arms, els, .. } => {
                for (c, res) in arms {
                    self.fold_uncorrelated_in_rexpr(c, bound, ctes, cost)?;
                    self.fold_uncorrelated_in_rexpr(res, bound, ctes, cost)?;
                }
                self.fold_uncorrelated_in_rexpr(els, bound, ctes, cost)
            }
            RExpr::Coalesce { args, .. }
            | RExpr::GreatestLeast { args, .. }
            | RExpr::ScalarFunc { args, .. }
            | RExpr::ArrayFunc { args, .. }
            | RExpr::RangeFunc { args, .. }
            | RExpr::RegexFunc { args, .. }
            | RExpr::RangeCtor { args, .. }
            | RExpr::RangeOp { args, .. }
            | RExpr::RangeSetOp { args, .. }
            | RExpr::Variadic { args, .. }
            | RExpr::JsonBuild { args, .. }
            | RExpr::JsonSetInsert { args, .. }
            | RExpr::JsonObjectFromArrays { args, .. }
            | RExpr::JsonPathFn { args, .. } => {
                for a in args {
                    self.fold_uncorrelated_in_rexpr(a, bound, ctes, cost)?;
                }
                Ok(())
            }
            RExpr::JsonSqlFn { ctx, path, .. } => {
                self.fold_uncorrelated_in_rexpr(ctx, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(path, bound, ctes, cost)?;
                Ok(())
            }
            RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
                for f in fields {
                    self.fold_uncorrelated_in_rexpr(f, bound, ctes, cost)?;
                }
                Ok(())
            }
            RExpr::Field { base, .. } => self.fold_uncorrelated_in_rexpr(base, bound, ctes, cost),
            RExpr::Subscript {
                base, subscripts, ..
            } => {
                self.fold_uncorrelated_in_rexpr(base, bound, ctes, cost)?;
                for s in subscripts {
                    match s {
                        RSubscript::Index(i) => {
                            self.fold_uncorrelated_in_rexpr(i, bound, ctes, cost)?
                        }
                        RSubscript::Slice { lower, upper } => {
                            if let Some(l) = lower {
                                self.fold_uncorrelated_in_rexpr(l, bound, ctes, cost)?;
                            }
                            if let Some(u) = upper {
                                self.fold_uncorrelated_in_rexpr(u, bound, ctes, cost)?;
                            }
                        }
                    }
                }
                Ok(())
            }
            RExpr::InValues { lhs, .. } => self.fold_uncorrelated_in_rexpr(lhs, bound, ctes, cost),
            RExpr::Quantified { lhs, array, .. } => {
                self.fold_uncorrelated_in_rexpr(lhs, bound, ctes, cost)?;
                self.fold_uncorrelated_in_rexpr(array, bound, ctes, cost)
            }
            // Leaves and the (already-handled) Subquery: nothing to recurse into.
            RExpr::Subquery { .. }
            | RExpr::Column(_)
            | RExpr::OuterColumn { .. }
            | RExpr::Param(_)
            | RExpr::ConstInt(_)
            | RExpr::ConstBool(_)
            | RExpr::ConstText(_)
            | RExpr::ConstDecimal(_)
            | RExpr::ConstFloat32(_)
            | RExpr::ConstFloat64(_)
            | RExpr::ConstBytea(_)
            | RExpr::ConstUuid(_)
            | RExpr::ConstJson(_)
            | RExpr::ConstJsonPath(_)
            | RExpr::ConstJsonb(_)
            | RExpr::ConstTimestamp(_)
            | RExpr::ConstTimestamptz(_)
            | RExpr::ConstDate(_)
            | RExpr::ConstInterval(_)
            | RExpr::ConstArray(_)
            | RExpr::ConstRange(_)
            // A DateClock leaf reads only the statement clock/session zone — nothing to fold.
            | RExpr::DateClock { .. }
            | RExpr::ConstNull => Ok(()),
        }
    }

    /// Shared read access to a table's store (the table is known to exist). Routes by the resolution
    /// walk (temp-tables.md §2): session-local temp → visible main snapshot.
    pub(crate) fn store(&self, name: &str) -> &TableStore {
        if self.is_temp_table(name) {
            return self.temp_read_snap().store(name);
        }
        self.read_snap().store(name)
    }

    /// Mutable access to a table's store (the table is known to exist; a write runs within a
    /// transaction). Routes a session-local temp write to `temp_working`, leaving the main image
    /// untouched.
    pub(crate) fn store_mut(&mut self, name: &str) -> &mut TableStore {
        if self.is_temp_table(name) {
            return self.temp_working_mut().store_mut(name);
        }
        self.working_mut().store_mut(name)
    }

    /// Shared read access to a secondary index's store (the index is known to exist). `name_key` is
    /// the lowercased index name; a temp table's index lives in the temp snapshot (temp-tables.md §8).
    /// Walks session-local → main.
    pub(crate) fn index_store(&self, name_key: &str) -> &TableStore {
        if self.temp_read_snap().has_index_store(name_key) {
            return self.temp_read_snap().index_store(name_key);
        }
        self.read_snap().index_store(name_key)
    }

    /// The resident GiST R-tree of the named index, for the planner gather (spec/design/gist.md
    /// §5). GiST on a temp table is `0A000` this slice (gist.md §11), so only the main snapshot
    /// carries trees — read straight from `read_snap`.
    pub(crate) fn gist_tree(
        &self,
        name_key: &str,
    ) -> Option<&std::sync::Arc<crate::gist::GistTree>> {
        self.read_snap().gist_tree(name_key)
    }

    /// Mutable access to a secondary index's store; routes a temp table's index to the working temp
    /// snapshot (session-local → main).
    pub(crate) fn index_store_mut(&mut self, name_key: &str) -> &mut TableStore {
        if self.temp_read_snap().has_index_store(name_key) {
            return self.temp_working_mut().index_store_mut(name_key);
        }
        self.working_mut().index_store_mut(name_key)
    }

    /// Whether the parent currently holds the key/prefix `probe` (committed + working state) — the
    /// child-side foreign-key existence test (spec/design/constraints.md §6.4). `parent_table` is
    /// the referenced table's name. Unmetered, like the PK/UNIQUE probes (cost.md §3).
    pub(crate) fn fk_probe_hits(&self, probe: &FkProbe, parent_table: &str) -> Result<bool> {
        match probe {
            FkProbe::Pk(key) => Ok(self.store(parent_table).get(key)?.is_some()),
            FkProbe::Unique { index, prefix } => Ok(!self
                .index_store(index)
                .range_entries(&unique_probe_bound(prefix))?
                .is_empty()),
        }
    }

    /// Whether any row of `child_table` references the parent tuple `target` (the parent key bytes,
    /// in the byte space [`fk_probe`] produces) via `fk` — the reverse of the child-side probe, a
    /// full scan since child FK columns are not index-backed (spec/design/constraints.md §6.5).
    /// MATCH SIMPLE: a child row with any NULL FK column references nothing. Rows whose storage key
    /// is in `exclude` are skipped — the END STATE for a self-reference, whose child IS the table
    /// being mutated (so its deleted/updated rows must not count). `parent` is the referenced
    /// table's catalog. Unmetered validation.
    pub(crate) fn fk_child_references(
        &self,
        child_table: &str,
        fk: &ForeignKeyConstraint,
        parent: &Table,
        target: &[u8],
        exclude: &HashSet<Vec<u8>>,
    ) -> Result<bool> {
        // `target` is in the parent's stored-key byte space, so the child probe encodes a collated
        // parent key column with the PARENT's collation (§2.12).
        let parent_colls = self.column_collations(&parent.columns);
        for (k, row) in self.store(child_table).iter_entries()? {
            if exclude.contains(&k) {
                continue;
            }
            if let Some(probe) = fk_probe(fk, parent, &parent_colls, &row, &fk.columns)? {
                if probe.bytes() == target {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Every (child table name, FK) pair in the visible snapshot whose FK references `parent_name`
    /// (case-insensitive), including a self-reference — the inbound FKs a parent DELETE/UPDATE must
    /// not strand (spec/design/constraints.md §6.5). Sorted by (lowercased child table, FK name) for
    /// a deterministic report order; cloned so the caller can probe stores without a snapshot borrow.
    pub(crate) fn fk_referencers(&self, parent_name: &str) -> Vec<(String, ForeignKeyConstraint)> {
        let snap = self.read_snap();
        let key = parent_name.to_ascii_lowercase();
        let mut out: Vec<(String, ForeignKeyConstraint)> = Vec::new();
        let mut tkeys: Vec<&String> = snap.tables.keys().collect();
        tkeys.sort();
        for tk in tkeys {
            let t = &snap.tables[tk];
            for fk in &t.foreign_keys {
                if fk.ref_table.eq_ignore_ascii_case(&key) {
                    out.push((t.name.clone(), fk.clone()));
                }
            }
        }
        out
    }

    /// Find the table owning the named index in the visible snapshot (case-insensitive).
    pub(crate) fn find_index(&self, name: &str) -> Option<(&str, &IndexDef)> {
        self.read_snap().find_index(name)
    }

    /// Whether `name` is taken in the shared relation namespace (a table OR an index —
    /// spec/design/indexes.md §2), case-insensitively.
    pub(crate) fn relation_exists(&self, name: &str) -> bool {
        // `self.table` already covers session-local temp tables (the resolution walk); the temp
        // snapshot's index names join the namespace too, so a name colliding with a temp table's
        // UNIQUE index is also 42P07 (preclude-overlaps — spec/design/temp-tables.md §3).
        self.table(name).is_some()
            || self.find_index(name).is_some()
            || self.temp_read_snap().find_index(name).is_some()
            || self.sequence(name).is_some()
    }

    /// Choose the auto-generated name for a `serial` column's OWNED sequence (sequences.md §12),
    /// matching PostgreSQL: `lower(table)_lower(column)_seq`, with the smallest integer suffix `1`,
    /// `2`, … appended until the name is free in the relation namespace — not taken by an existing
    /// relation, not equal to the table being created, not already chosen by an earlier `serial`
    /// column of the same statement (`pending`), and not held by a caller-known pending relation.
    /// All-lowercase identifier-derived, so deterministic.
    pub(crate) fn choose_serial_seq_name(
        &self,
        table: &str,
        column: &str,
        pending: &[SequenceDef],
        reserved: &[String],
    ) -> String {
        let base = format!(
            "{}_{}_seq",
            table.to_ascii_lowercase(),
            column.to_ascii_lowercase()
        );
        let taken = |c: &str| {
            self.relation_exists(c)
                || c.eq_ignore_ascii_case(table)
                || pending.iter().any(|s| s.name.eq_ignore_ascii_case(c))
                || reserved.iter().any(|name| name.eq_ignore_ascii_case(c))
        };
        if !taken(&base) {
            return base;
        }
        let mut n = 1u32;
        loop {
            let cand = format!("{base}{n}");
            if !taken(&cand) {
                return cand;
            }
            n += 1;
        }
    }
}
