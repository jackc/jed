//! Row production — the scan/stream/join execution engine (front half of SELECT execution; mirrors
//! impl/go exec_scan.go): the streaming scan/sort/join operators, index-order and window-top-N scans,
//! relation materialization, and the exec_select_plan entry — as Engine methods.

use super::*;

#[allow(clippy::too_many_arguments)]
fn process_streaming_row(
    store: &TableStore,
    row: &Row,
    plan: &SelectPlan,
    env: &EvalEnv,
    meter: &mut Meter,
    offset: i64,
    passed: &mut i64,
    seen: &mut std::collections::HashSet<Vec<Value>>,
    out: &mut Vec<Vec<Value>>,
    guarded: bool,
) -> Result<bool> {
    if !guarded {
        meter.guard()?;
    }
    meter.charge(COSTS.storage_row_read);
    let resolved;
    let row = if TableStore::needs_resolution(row, &plan.rel_masks[0]) {
        let mut r = row.clone();
        store.resolve_columns(&mut r, &plan.rel_masks[0])?;
        resolved = r;
        &resolved
    } else {
        row
    };
    if let Some(filter) = &plan.filter {
        if !filter.eval(row, env, meter)?.is_true() {
            return Ok(true);
        }
    }
    if plan.distinct {
        let mut projected = Vec::with_capacity(plan.projections.len());
        for p in &plan.projections {
            projected.push(p.eval(row, env, meter)?);
        }
        if !seen.insert(projected.clone()) {
            return Ok(true);
        }
        *passed += 1;
        if *passed <= offset {
            return Ok(true);
        }
        meter.charge(COSTS.row_produced);
        out.push(projected);
    } else {
        *passed += 1;
        if *passed <= offset {
            return Ok(true);
        }
        meter.charge(COSTS.row_produced);
        let mut projected = Vec::with_capacity(plan.projections.len());
        for p in &plan.projections {
            projected.push(p.eval(row, env, meter)?);
        }
        out.push(projected);
    }
    Ok(plan.limit.is_none_or(|limit| (out.len() as i64) < limit))
}

fn index_row_key<'a>(entry_key: &'a [u8], prefix_len: usize, suffix: &[ScalarType]) -> &'a [u8] {
    let mut at = prefix_len;
    for ty in suffix {
        at += if entry_key.get(at) == Some(&0x01) {
            1
        } else {
            1 + ty.width_bytes()
        };
    }
    &entry_key[at..]
}

#[allow(clippy::too_many_arguments)]
fn scan_stream_table_interval(
    store: &TableStore,
    bound: &KeyBound,
    reverse: bool,
    plan: &SelectPlan,
    env: &EvalEnv,
    meter: &mut Meter,
    offset: i64,
    passed: &mut i64,
    seen: &mut std::collections::HashSet<Vec<Value>>,
    out: &mut Vec<Vec<Value>>,
    can_pull: bool,
) -> Result<bool> {
    let (overlap, slabs) = store.overlap_scan_units(bound, &plan.rel_masks[0])?;
    meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);
    if !can_pull {
        return Ok(false);
    }
    let mut more = true;
    let mut visit = |_key: &[u8], row: &Row| -> Result<bool> {
        more = process_streaming_row(
            store, row, plan, env, meter, offset, passed, seen, out, false,
        )?;
        Ok(more)
    };
    if reverse {
        store.scan_range_rev(bound, &mut visit)?;
    } else {
        store.scan_range(bound, &mut visit)?;
    }
    Ok(more)
}

impl Engine {
    /// The bounded streaming scan path (spec/design/cost.md §3): full/contiguous PK scans,
    /// canonical PK/index interval sets, and compatible ordered-index scans stop at the
    /// LIMIT/OFFSET window. GIN/GiST complete their candidate gather, then stop table point-lookups
    /// at that window. Only started interval blocks are charged; an opclass gather is charged in full.
    pub(crate) fn exec_streaming_scan(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
        params: &[Value],
    ) -> Result<SelectResult> {
        let store = self.store_scoped(plan.rels[0].db.as_deref(), &plan.rels[0].table_name);
        let offset = plan.offset.unwrap_or(0);
        let mut out: Vec<Vec<Value>> = Vec::new();
        let mut passed = 0i64;
        let mut seen = std::collections::HashSet::new();
        let can_pull = plan.limit != Some(0);

        match &plan.phys.rel_bounds[0] {
            None => {
                scan_stream_table_interval(
                    store,
                    &KeyBound::unbounded(),
                    plan.phys.pk_reverse,
                    plan,
                    env,
                    meter,
                    offset,
                    &mut passed,
                    &mut seen,
                    &mut out,
                    can_pull,
                )?;
            }
            Some(ScanBound::Pk(bp)) => {
                if let Some(bound) = build_key_bound(bp, params, env.outer, &[]) {
                    scan_stream_table_interval(
                        store,
                        &bound,
                        plan.phys.pk_reverse,
                        plan,
                        env,
                        meter,
                        offset,
                        &mut passed,
                        &mut seen,
                        &mut out,
                        can_pull,
                    )?;
                }
            }
            Some(ScanBound::PkSet(ks)) => {
                if can_pull {
                    let intervals = canonical_interval_set(
                        ks.pk_type,
                        &ks.specs,
                        &ks.clip,
                        params,
                        env.outer,
                        ks.coll.as_deref(),
                        &[],
                    );
                    if plan.phys.pk_reverse {
                        for bound in intervals.iter().rev() {
                            if !scan_stream_table_interval(
                                store,
                                bound,
                                true,
                                plan,
                                env,
                                meter,
                                offset,
                                &mut passed,
                                &mut seen,
                                &mut out,
                                can_pull,
                            )? {
                                break;
                            }
                        }
                    } else {
                        for bound in &intervals {
                            if !scan_stream_table_interval(
                                store,
                                bound,
                                false,
                                plan,
                                env,
                                meter,
                                offset,
                                &mut passed,
                                &mut seen,
                                &mut out,
                                can_pull,
                            )? {
                                break;
                            }
                        }
                    }
                }
            }
            Some(ScanBound::Index(ib)) => {
                if let Some((bound, prefix_len)) = build_index_bound(ib, params, env.outer, &[]) {
                    let istore = self.index_store(&ib.name_key);
                    let (overlap, slabs) = istore.overlap_scan_units(&bound, &[])?;
                    meter.charge(
                        COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64,
                    );
                    if can_pull {
                        let mut visit = |entry_key: &[u8], _row: &Row| -> Result<bool> {
                            meter.guard()?;
                            let row_key = index_row_key(entry_key, prefix_len, &ib.suffix_types);
                            let (row, pages, slabs) =
                                store.get_with_units(row_key, &plan.rel_masks[0])?;
                            let row = row.expect("an index entry references a stored row");
                            meter.charge(
                                COSTS.page_read * pages as i64
                                    + COSTS.value_decompress * slabs as i64,
                            );
                            process_streaming_row(
                                store,
                                &row,
                                plan,
                                env,
                                meter,
                                offset,
                                &mut passed,
                                &mut seen,
                                &mut out,
                                true,
                            )
                        };
                        istore.scan_range(&bound, &mut visit)?;
                    }
                }
            }
            Some(ScanBound::IndexSet(ks)) => {
                if can_pull {
                    for logical in canonical_interval_set(
                        ks.col_type,
                        &ks.specs,
                        &ks.clip,
                        params,
                        env.outer,
                        ks.coll.as_deref(),
                        &[],
                    ) {
                        let physical = index_logical_interval(&logical);
                        let point = logical.lo.is_some()
                            && logical.lo == logical.hi
                            && logical.lo_inc
                            && logical.hi_inc;
                        let mut suffix = ks.tail_types.clone();
                        let prefix_len = if point {
                            1 + logical.lo.as_ref().unwrap().len()
                        } else {
                            suffix.insert(0, ks.col_type);
                            0
                        };
                        let istore = self.index_store(&ks.name_key);
                        let (overlap, slabs) = istore.overlap_scan_units(&physical, &[])?;
                        meter.charge(
                            COSTS.page_read * overlap as i64
                                + COSTS.value_decompress * slabs as i64,
                        );
                        let mut more = true;
                        let mut visit = |entry_key: &[u8], _row: &Row| -> Result<bool> {
                            meter.guard()?;
                            let row_key = index_row_key(entry_key, prefix_len, &suffix);
                            let (row, pages, slabs) =
                                store.get_with_units(row_key, &plan.rel_masks[0])?;
                            let row = row.expect("an index entry references a stored row");
                            meter.charge(
                                COSTS.page_read * pages as i64
                                    + COSTS.value_decompress * slabs as i64,
                            );
                            more = process_streaming_row(
                                store,
                                &row,
                                plan,
                                env,
                                meter,
                                offset,
                                &mut passed,
                                &mut seen,
                                &mut out,
                                true,
                            )?;
                            Ok(more)
                        };
                        istore.scan_range(&physical, &mut visit)?;
                        if !more {
                            break;
                        }
                    }
                }
            }
            Some(ScanBound::Gin(gb)) => {
                let query = plan
                    .filter
                    .as_ref()
                    .and_then(|filter| gin_match(filter, gb.col_global).map(|(_, q)| q));
                let (mut candidates, (pages, slabs)) = self.gin_bound_rows(
                    &plan.rels[0].table_name,
                    gb,
                    query,
                    &[],
                    env,
                    meter,
                    &plan.rel_masks[0],
                    true,
                )?;
                meter
                    .charge(COSTS.page_read * pages as i64 + COSTS.value_decompress * slabs as i64);
                if can_pull {
                    if plan.phys.pk_reverse {
                        candidates.reverse();
                    }
                    for (key, candidate) in candidates {
                        meter.guard()?;
                        let row = if candidate.is_empty() {
                            let (row, pages, slabs) =
                                store.get_with_units(&key, &plan.rel_masks[0])?;
                            meter.charge(
                                COSTS.page_read * pages as i64
                                    + COSTS.value_decompress * slabs as i64,
                            );
                            row.expect("a GIN entry references a stored row")
                        } else {
                            candidate
                        };
                        if !process_streaming_row(
                            store,
                            &row,
                            plan,
                            env,
                            meter,
                            offset,
                            &mut passed,
                            &mut seen,
                            &mut out,
                            true,
                        )? {
                            break;
                        }
                    }
                }
            }
            Some(ScanBound::Gist(gb)) => {
                let query = plan
                    .filter
                    .as_ref()
                    .and_then(|filter| gist_query_operand(filter, gb));
                let (mut candidates, (pages, slabs)) = self.gist_bound_rows(
                    &plan.rels[0].table_name,
                    gb,
                    query,
                    &[],
                    env,
                    meter,
                    &plan.rel_masks[0],
                    true,
                )?;
                meter
                    .charge(COSTS.page_read * pages as i64 + COSTS.value_decompress * slabs as i64);
                if can_pull {
                    if plan.phys.pk_reverse {
                        candidates.reverse();
                    }
                    for (key, candidate) in candidates {
                        meter.guard()?;
                        let row = if candidate.is_empty() {
                            let (row, pages, slabs) =
                                store.get_with_units(&key, &plan.rel_masks[0])?;
                            meter.charge(
                                COSTS.page_read * pages as i64
                                    + COSTS.value_decompress * slabs as i64,
                            );
                            row.expect("a GiST entry references a stored row")
                        } else {
                            candidate
                        };
                        if !process_streaming_row(
                            store,
                            &row,
                            plan,
                            env,
                            meter,
                            offset,
                            &mut passed,
                            &mut seen,
                            &mut out,
                            true,
                        )? {
                            break;
                        }
                    }
                }
            }
        }
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows: out,
            cost: meter.accrued,
        })
    }

    /// Whether a plain (non-grouped) window query can serve its LIMIT with a TOP-N over the primary-key
    /// scan — reading only the first OFFSET+LIMIT rows instead of the whole table (spec/design/window.md
    /// §5.2 "windowed top-N", cost.md §3). It is the window analog of the streaming-scan LIMIT
    /// short-circuit ([`streaming_scan_eligible`]), sound only when every window value at scan position
    /// `k` depends solely on rows at positions `<= k` (a "backward" window over the scan order): then
    /// the first OFFSET+LIMIT scan rows determine the first OFFSET+LIMIT output rows exactly.
    ///
    /// The gate (all must hold): a single base-table full/PK-bounded scan (no join/SRF/CTE/derived, no
    /// index/GIN/GiST bound — those read the full admitted set), a plain window (`has_window &&
    /// !is_agg`), not DISTINCT, a LIMIT present, and an outer ORDER BY the PK scan already satisfies
    /// (`pk_ordered`, so the scan order IS the output order and no post-window sort reorders rows). No
    /// compound (materialized) window key (`window_keys`) and no general-expression ORDER BY
    /// (`order_exprs`) — those append synthetic columns; a bare PK-column window is the shape that
    /// streams. Finally EVERY window spec must be prefix-safe ([`Engine::window_spec_prefix_safe`]).
    /// Rows are byte-identical to the eager path; only the accrued cost drops (fewer rows scanned/
    /// folded), the deliberate cost change (like the streaming LIMIT short-circuit — cross-core
    /// identical because every core caps at the same OFFSET+LIMIT).
    pub(crate) fn window_top_n_eligible(&self, plan: &SelectPlan) -> bool {
        if !plan.has_window
            || plan.is_agg
            || plan.distinct
            || plan.limit.is_none()
            || !plan.phys.pk_ordered
        {
            return false;
        }
        if plan.rels.len() != 1 || !plan.joins.is_empty() {
            return false;
        }
        let rel = &plan.rels[0];
        if rel.srf.is_some() || rel.cte.is_some() || rel.derived.is_some() {
            return false;
        }
        if matches!(
            plan.phys.rel_bounds[0],
            Some(ScanBound::Index(_))
                | Some(ScanBound::Gin(_))
                | Some(ScanBound::Gist(_))
                | Some(ScanBound::PkSet(_))
                | Some(ScanBound::IndexSet(_))
        ) {
            return false;
        }
        if !plan.window_keys.is_empty() || !plan.order_exprs.is_empty() {
            return false;
        }
        let Some(table) = self.table_scoped(rel.db.as_deref(), &rel.table_name) else {
            return false;
        };
        plan.window_specs
            .iter()
            .all(|spec| self.window_spec_prefix_safe(spec, plan, table, rel.offset))
    }

    /// Whether one window function's value at scan position `k` depends solely on rows at positions
    /// `<= k`, so truncating the input to the first OFFSET+LIMIT rows is exact (spec/design/window.md
    /// §5.2). It requires: no PARTITION BY (the whole scan is one partition, so scan order = partition
    /// order); a window ORDER BY the PRIMARY KEY satisfies in the SAME direction as the outer
    /// `pk_ordered` scan (so the window's "preceding" is the scan's preceding — the sort is a no-op);
    /// and a backward plan/frame:
    ///
    ///   - `row_number` / `rank` / `dense_rank` / `lag` → backward (position, earlier-peer count, or a
    ///     look-BACK offset); never depend on later rows or the total partition size.
    ///   - an aggregate / `first_value` / `last_value` / `nth_value` window → backward iff its FRAME
    ///     does not look forward ([`frame_backward_safe`]).
    ///   - `percent_rank` / `cume_dist` / `ntile` depend on the total partition size N; `lead` looks
    ///     FORWARD — all rejected.
    pub(crate) fn window_spec_prefix_safe(
        &self,
        spec: &WindowSpec,
        plan: &SelectPlan,
        table: &Table,
        offset: usize,
    ) -> bool {
        if !spec.partition.is_empty() || spec.order.is_empty() {
            return false;
        }
        match order_satisfied_by_pk(table, offset, &spec.order, self) {
            Some(rev) if rev == plan.phys.pk_reverse => {}
            _ => return false,
        }
        // The order covers the full (unique) PK ⇒ singleton peer groups (needed for a RANGE/GROUPS
        // CURRENT-ROW frame end, which otherwise spans forward peers).
        let unique = spec.order.len() >= table.pk_indices().len();
        match spec.plan {
            WindowPlan::RowNumber | WindowPlan::Rank | WindowPlan::DenseRank | WindowPlan::Lag => {
                true
            }
            WindowPlan::Agg(_)
            | WindowPlan::FirstValue
            | WindowPlan::LastValue
            | WindowPlan::NthValue => frame_backward_safe(&spec.frame, unique),
            // PercentRank / CumeDist / Ntile need the total partition size N; Lead looks forward.
            _ => false,
        }
    }

    /// A windowed top-N (spec/design/window.md §5.2, cost.md §3): a plain window query whose LIMIT is
    /// answerable from the first OFFSET+LIMIT primary-key-scan rows (the gate is
    /// [`Engine::window_top_n_eligible`]). It streams the PK scan, applies WHERE, and collects
    /// survivors until it has OFFSET+LIMIT of them — then runs the ordinary window stage over that
    /// PREFIX and emits the OFFSET..OFFSET+LIMIT slice. Because every window value at scan position `k`
    /// depends only on rows at positions `<= k` (`window_spec_prefix_safe`), and the outer ORDER BY is
    /// the PK scan order (`pk_ordered`) so no sort reorders rows, the rows are byte-identical to the
    /// eager whole-table path; only the accrued cost is lower (fewer rows scanned, filtered, and
    /// folded) — the deliberate short-circuit, mirroring [`Engine::exec_streaming_scan`]'s LIMIT stop.
    /// page_read is the full block up front (only per-row work short-circuits, like the streaming scan).
    pub(crate) fn exec_window_top_n(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
        params: &[Value],
    ) -> Result<Emitter> {
        let store = self.store_scoped(plan.rels[0].db.as_deref(), &plan.rels[0].table_name);
        let reverse = plan.phys.pk_reverse;

        let (bound, empty) = match &plan.phys.rel_bounds[0] {
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, params, env.outer, &[]) {
                Some(b) => (b, false),
                None => (KeyBound::unbounded(), true),
            },
            Some(ScanBound::Index(_))
            | Some(ScanBound::Gin(_))
            | Some(ScanBound::Gist(_))
            | Some(ScanBound::PkSet(_))
            | Some(ScanBound::IndexSet(_)) => {
                unreachable!("the windowed top-N path is gated to PK/full scans")
            }
            None => (KeyBound::unbounded(), false),
        };
        let (overlap, slabs) = if empty {
            (0, 0)
        } else {
            store.overlap_scan_units(&bound, &plan.rel_masks[0])?
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);

        let limit = plan.limit.expect("window_top_n_eligible requires a LIMIT");
        let offset = plan.offset.unwrap_or(0);
        let cap = offset.saturating_add(limit); // OFFSET+LIMIT survivors suffice (backward window)

        // Collect the first `cap` surviving rows in PK scan order (respecting `pk_reverse`), charging
        // storage_row_read per scanned row and the WHERE operator_evals — the streaming-scan feed,
        // minus the projection (the window stage runs before projection). Stop the instant `cap`
        // survivors are in hand: a genuine early-out, so the window fold sees only the prefix it needs.
        let mut rows: Vec<Row> = Vec::new();
        if !empty && limit > 0 {
            let mut visit = |_key: &[u8], row: &Row| -> Result<bool> {
                meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
                meter.charge(COSTS.storage_row_read);
                let resolved;
                let row = if TableStore::needs_resolution(row, &plan.rel_masks[0]) {
                    let mut r = row.clone();
                    store.resolve_columns(&mut r, &plan.rel_masks[0])?;
                    resolved = r;
                    &resolved
                } else {
                    row
                };
                let keep = match &plan.filter {
                    Some(f) => f.eval(row, env, meter)?.is_true(),
                    None => true,
                };
                if !keep {
                    return Ok(true);
                }
                rows.push(row.clone());
                Ok((rows.len() as i64) < cap) // stop once the OFFSET+LIMIT window is filled
            };
            if reverse {
                store.scan_range_rev(&bound, &mut visit)?;
            } else {
                store.scan_range(&bound, &mut visit)?;
            }
        }

        // The window stage over the collected prefix — identical to the eager path (§5.2), just fewer
        // rows.
        apply_window_stage(&mut rows, &plan.window_specs, &plan.window_keys, env, meter)?;

        // The prefix is already in outer ORDER BY order (`pk_ordered`), so the sort is elided. Slice
        // the OFFSET..OFFSET+LIMIT window and project on emission — only an emitted row charges
        // row_produced + projection cost (the eager non-DISTINCT window path's Project, streaming.md §4).
        let len = rows.len() as i64;
        let start = offset.min(len) as usize;
        let end = if limit < len - start as i64 {
            start + limit as usize
        } else {
            len as usize
        };
        Ok(Emitter::Buffer {
            rows,
            start,
            end,
            mode: EmitMode::Project,
        })
    }

    /// Build a frozen read-snapshot [`Engine`] for a streaming cursor (spec/design/streaming.md §5):
    /// the VISIBLE main / session-temp snapshots captured **by value** (copy-on-write
    /// `Arc` clones, so this pins the roots cheaply and they stay stable for the cursor's whole life,
    /// isolated from later writes on the live handle) with **no open transaction** — so the owned
    /// engine's reads see exactly the captured frozen state — plus the session envelope the per-row
    /// eval / the cost meter read: the cost ceilings + the **shared** lifetime gauge (`Rc` clone — so
    /// streaming cost still counts against `lifetime_max_cost`), the cancel poll (so mid-drain
    /// cancellation lands), the **shared** entropy/clock seam (`Rc` clone — `uuidv7()`/`now()` draw
    /// from the same injected source as the eager path), session vars, the time zone, and the
    /// `currval`/`lastval` session state. The cursor evaluates its filter/projection against this
    /// owned engine, so the streaming `Rows` is self-contained (`'static`) and never borrows the live
    /// handle (so it survives `Database::query`'s transient session, streaming.md §5).
    pub(crate) fn snapshot_engine(&self) -> Engine {
        let mut e = Engine::from_snapshot(self.read_snap().clone());
        e.page_size = self.page_size;
        e.paging = self.paging.clone();
        e.read_only = self.read_only;
        let src = &self.session;
        let dst = &mut e.session;
        dst.max_cost = src.max_cost;
        dst.lifetime_max_cost = src.lifetime_max_cost;
        dst.lifetime_total = src.lifetime_total.clone(); // shared gauge — streaming cost counts (§5)
        dst.cancel = src.cancel.clone();
        dst.work_mem = src.work_mem;
        dst.seam = src.seam.clone(); // shared seam (Rc) — uuid/clock draw from the injected source
        dst.vars = src.vars.clone();
        dst.time_zone = src.time_zone.clone();
        dst.session_seq = src.session_seq.clone(); // currval/lastval reads stay faithful
        dst.session_last_name = src.session_last_name.clone();
        dst.temp_committed = self.temp_read_snap().clone();
        // The frozen read engine carries the same pinned attachment view so a streaming read of an
        // attached database (attached-databases.md §5) resolves through it; it never commits
        // (read-only), so it needs no core back-ref (`core` stays `None`).
        e.attached_committed = self.attach_read_view();
        e
    }

    /// Serve `ast` as a lazy **streaming** or **buffered** query (spec/design/streaming.md §3/§4),
    /// planning it EXACTLY ONCE and classifying streaming-vs-buffered from that single plan — the
    /// plan-once replacement for the old `try_streaming_query` + `try_buffered_query` pair, each of
    /// which re-planned the same statement. Returns `Some(Rows)` for a top-level read `SELECT`; `None`
    /// for a shape no scan lane covers (a non-`SELECT`, a write — incl. a `nextval`/`setval` SELECT,
    /// [`stmt_is_write`] — or a top-level set-op / VALUES / `WITH`), so the caller falls through to the
    /// deferred / materialized paths. When `cache` is `Some` (a prepared statement) a repeated execute
    /// over an unchanged catalog reuses the cached plan and skips planning + the fold; the ad-hoc
    /// `query()` passes `None` and still plans exactly once. The conformance corpus drives the
    /// materialized `execute()` path, so this lane stays invisible to it (per-core unit-tested to yield
    /// identical rows + total cost under full drain, streaming.md §6).
    pub(crate) fn try_scan_query(
        &self,
        ast: &Statement,
        params: &[Value],
        cache: Option<&std::cell::RefCell<Option<CachedPlan>>>,
    ) -> Result<Option<Rows>> {
        // Only a bare top-level SELECT is a scan lane: a set operation / WITH / VALUES / DML is
        // blocking or not a scan; a write-classified SELECT (a sequence mutator) buffers too.
        let Statement::Select(sel) = ast else {
            return Ok(None);
        };
        if stmt_is_write(ast) {
            return Ok(None);
        }
        let (cur_gen, from_committed) = {
            let snap = self.read_snap();
            (snap.cat_gen, std::ptr::eq(snap, &self.committed))
        };
        // Cache HIT: the plan was resolved against THIS database (same core — `cat_gen` is only
        // monotonic within one core) and the read snapshot's catalog is unchanged since (its `cat_gen`
        // still matches), and no relation name is shadowed by a session-local temp table the committed
        // generation cannot see (`plan_touches_temp` — the statement may have been filled on a
        // different session). Reuse the resolved plan + finalized param types — no `plan_query`, no
        // fold, no param-type walk. A cached plan carries no subquery to fold (`plan_cacheable`
        // rejected any), so the shared `Rc` plan is never mutated; params are still bound per execute.
        if let (Some(cell), Some(core)) = (cache, &self.core) {
            let hit = cell
                .borrow()
                .as_ref()
                .filter(|cp| {
                    cp.cat_gen == cur_gen
                        && std::sync::Weak::ptr_eq(&cp.core, &std::sync::Arc::downgrade(core))
                        && !self.plan_touches_temp(&cp.plan)
                })
                .map(|cp| (std::rc::Rc::clone(&cp.plan), cp.param_types.clone()));
            if let Some((plan, ptys)) = hit {
                let bound_params = bind_params(params, &ptys)?;
                if let Some(rows) = self.build_scan_rows(plan, bound_params, 0)? {
                    return Ok(Some(rows));
                }
            }
        }
        // MISS: plan once.
        let qe = QueryExpr::Select(Box::new(sel.clone()));
        let mut ptypes = ParamTypes::default();
        let QueryPlan::Select(mut sp) = self.plan_query(&qe, None, &[], &mut ptypes)? else {
            return Ok(None); // set-op / VALUES / WITH — a scan lane does not cover it
        };
        let uncacheable = ptypes.uncacheable;
        let ptys = ptypes.finalize()?;
        let bound_params = bind_params(params, &ptys)?;
        // Fold globally-uncorrelated subqueries to constants (at top level every surviving subquery is
        // uncorrelated) so the per-row eval re-enters nothing — keeping the cursor self-contained. The
        // fold's own cost was already charged to the shared lifetime gauge by its sub-executions; it is
        // added to the cursor's reported cost below. A cacheable plan has no subquery, so this is a
        // no-op (and is skipped on a hit) — cost stays identical.
        let mut subquery_cost: i64 = 0;
        self.fold_uncorrelated_in_select(
            &mut sp,
            &bound_params,
            CteCtx::empty(),
            &mut subquery_cost,
        )?;
        // Fill the cache only from committed state — so `committed.cat_gen` is strictly increasing over
        // the core's life and never aliases a rolled-back working generation, making the `cat_gen`
        // equality on a later HIT a sound "same catalog" identity check (a statement first executed
        // inside an open transaction re-plans until the tx commits) — and only for a reusable plan,
        // and only when this engine belongs to a core (the entry's identity key; a bare/transient
        // engine never fills).
        let cacheable = from_committed && !uncacheable && self.plan_cacheable(&sp);
        let plan = std::rc::Rc::new(sp);
        if let (Some(cell), true, Some(core)) = (cache, cacheable, &self.core) {
            *cell.borrow_mut() = Some(CachedPlan {
                core: std::sync::Arc::downgrade(core),
                cat_gen: cur_gen,
                plan: std::rc::Rc::clone(&plan),
                param_types: ptys,
            });
        }
        self.build_scan_rows(plan, bound_params, subquery_cost)
    }

    /// Classify a resolved plan (already folded + params bound) as direct-pull or buffered and build
    /// the matching lazy cursor. The direct branch handles full/contiguous-PK scans; generalized
    /// bounded streams run through the buffered cursor on first pull. `plan` is
    /// shared (`Rc`) — a cache hit hands the SAME allocation here without re-planning. Returns `None`
    /// only in the (unreachable, defensive) case that an eligible plan carries a non-PK scan bound, so
    /// the caller falls through. Under full drain the rows + total cost are byte-identical to the
    /// eager path.
    pub(crate) fn build_scan_rows(
        &self,
        plan: std::rc::Rc<SelectPlan>,
        bound_params: Vec<Value>,
        subquery_cost: i64,
    ) -> Result<Option<Rows>> {
        if pull_streaming_scan_eligible(&plan) {
            // Resolve the scan bound (the PK pushdown, if any) and the up-front cost block — identical
            // to `exec_streaming_scan`. An empty bound (e.g. `pk = NULL`) admits no row.
            let reverse = plan.phys.pk_reverse;
            let (bound, empty) = match &plan.phys.rel_bounds[0] {
                Some(ScanBound::Pk(bp)) => match build_key_bound(bp, &bound_params, &[], &[]) {
                    Some(b) => (b, false),
                    None => (KeyBound::unbounded(), true),
                },
                // Eligibility already excludes index/GIN/GiST bounds; this is defensive.
                Some(_) => return Ok(None),
                None => (KeyBound::unbounded(), false),
            };
            let snap = self.snapshot_engine();
            let (overlap, slabs) = if empty {
                (0, 0)
            } else {
                snap.store_scoped(plan.rels[0].db.as_deref(), &plan.rels[0].table_name)
                    .overlap_scan_units(&bound, &plan.rel_masks[0])?
            };
            // cursor is `'static`, so it owns the mask.
            let scan = snap
                .store_scoped(plan.rels[0].db.as_deref(), &plan.rels[0].table_name)
                .store_scan(bound, reverse);
            let mut meter = snap.session.new_meter();
            meter.accrued = subquery_cost; // the folded constant cost (lifetime already charged)
            meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);

            let limit = plan.limit;
            let offset = plan.offset.unwrap_or(0);
            let distinct = plan.distinct;
            let column_names = plan.column_names.clone();
            let column_types = type_names(&plan.column_types);
            let done = empty || limit == Some(0);
            let stream = StreamingScan {
                engine: snap,
                plan,
                params: bound_params,
                rng: std::cell::Cell::new(crate::seam::StmtRng::new()),
                scan,
                meter,
                offset,
                limit,
                distinct,
                seen: std::collections::HashSet::new(),
                passed: 0,
                produced: 0,
                done,
            };
            return Ok(Some(Rows::from_streaming(
                column_names,
                column_types,
                Box::new(stream),
            )));
        }

        // Blocking (buffered) shape: buffers its input but yields the output one row at a time.
        let snap = self.snapshot_engine();
        let mut meter = snap.session.new_meter();
        meter.accrued = subquery_cost; // the folded constant cost (lifetime already charged)
        let column_names = plan.column_names.clone();
        let column_types = type_names(&plan.column_types);
        let stream = BufferedScan {
            engine: snap,
            plan,
            params: bound_params,
            rng: std::cell::Cell::new(crate::seam::StmtRng::new()),
            meter,
            state: BufState::Pending,
        };
        Ok(Some(Rows::from_streaming(
            column_names,
            column_types,
            Box::new(stream),
        )))
    }

    /// Whether a resolved scan plan may be memoized on a prepared statement (spec/design/api.md §2.4).
    /// The subquery / precompiled-regex exclusion is tracked separately ([`ParamTypes::uncacheable`],
    /// set at the node's birth). Here the relations are vetted: a set-returning / CTE / derived
    /// relation carries a nested plan or generator we do not vet for reuse, and a temp table lives in a
    /// snapshot the cache key (`committed.cat_gen`) does not track — so a plan referencing any of those
    /// is never cached (a point lookup / plain join over persistent base tables has none).
    pub(crate) fn plan_cacheable(&self, sp: &SelectPlan) -> bool {
        sp.rels
            .iter()
            .all(|r| r.srf.is_none() && r.cte.is_none() && r.derived.is_none())
            && !self.plan_touches_temp(sp)
    }

    /// Whether any of the plan's relations currently resolves to a SESSION-LOCAL temporary table in
    /// THIS session's visible temp domain. Checked at cache fill (a temp plan is never cached) and
    /// re-checked on every cache HIT: a statement is shared across sessions, and a plan cached where
    /// a name was persistent must not be served on a session whose temp table shadows that name — the
    /// temp domain is session-local, so the committed `cat_gen` the cache is keyed on cannot see it.
    /// Cheap: one map lookup per relation, against a usually-empty temp catalog.
    pub(crate) fn plan_touches_temp(&self, sp: &SelectPlan) -> bool {
        sp.rels.iter().any(|r| self.is_temp_table(&r.table_name))
    }

    /// Try to serve `ast` as a lazy **deferred** query (spec/design/streaming.md §4/§7) — the
    /// `query()` path for a top-level **set operation** (`UNION`/`INTERSECT`/`EXCEPT`) or a
    /// **pure-query `WITH`**. These are blocking shapes whose output is already projected AND charged
    /// (no per-row top-level projection to defer), so the only streaming win is **lazy-yield**
    /// (streaming.md §7): the cursor defers the whole `run_set_op` / `run_with` to its FIRST pull — so a
    /// `54P01` cost abort, a `54P02` lifetime abort, a `57014` cancellation, or an arithmetic trap
    /// surfaces *during iteration*, not at `query()` (§6) — then yields the buffered result one row at a
    /// time over a frozen snapshot (§5). Returns `None` for any non-set-op/WITH statement, or a
    /// write-classified one (a data-modifying `WITH`, or a `nextval`/`setval` call — [`stmt_is_write`]),
    /// which falls back to the materialized `dispatch` path. Under **full drain** the rows + total cost
    /// are byte-identical to the eager `execute()` path (it drives the SAME `run_set_op` / `run_with`,
    /// §6), so the corpus — which drives `execute()` — stays green by construction; per-core unit tests
    /// pin `query()` == `execute()`.
    pub(crate) fn try_deferred_query(
        &self,
        ast: &Statement,
        params: &[Value],
    ) -> Result<Option<Rows>> {
        // A write-classified statement (a data-modifying WITH, a sequence mutator) must take the write
        // gate and never streams (streaming.md §7 / sequences.md §4).
        if stmt_is_write(ast) {
            return Ok(None);
        }
        let query = match ast {
            Statement::SetOp(so) => DeferredQuery::SetOp(so.clone()),
            Statement::With(wq) => DeferredQuery::With(wq.clone()),
            _ => return Ok(None),
        };
        // Resolve the output column names up front (the `Rows` cursor exposes them before the first
        // pull). Planning is unmetered + deterministic, so the names read here are the IDENTICAL names
        // the deferred run produces (the run on first pull reuses `run_set_op`/`run_with` verbatim, so
        // there is no rows/cost drift). A planning error (42P01/42804/…) surfaces at `query()`,
        // matching the eager path.
        let (column_names, column_types) = self.deferred_column_names(ast)?;
        let stream = DeferredResult {
            engine: self.snapshot_engine(),
            query: Some(query),
            params: params.to_vec(),
            state: DeferredState::Pending,
            cost: 0,
        };
        Ok(Some(Rows::from_streaming(
            column_names,
            column_types,
            Box::new(stream),
        )))
    }

    /// The output column names of a top-level set operation / pure-query `WITH`, resolved by planning
    /// only (no execution) — fills a [`DeferredResult`] cursor's metadata before its first pull
    /// ([`try_deferred_query`]). Mirrors the planning prefix of `run_set_op` / `run_with` exactly so the
    /// names match the deferred run's. Bound params are not needed: column names never depend on bound
    /// values.
    pub(crate) fn deferred_column_names(
        &self,
        ast: &Statement,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let mut ptypes = ParamTypes::default();
        let plan = match ast {
            Statement::SetOp(so) => self.plan_query(
                &QueryExpr::SetOp(Box::new(so.clone())),
                None,
                &[],
                &mut ptypes,
            )?,
            Statement::With(wq) => {
                // The planning prefix of `run_with` (cte.md): plan the CTE bindings, then the body with
                // them visible. The body's column names/types are the WITH's output names/types.
                let bindings = self.plan_cte_bindings(&wq.ctes, wq.recursive, &mut ptypes)?;
                let body_q = wq
                    .body
                    .as_query()
                    .expect("a pure-query WITH (DML excluded by stmt_is_write)");
                self.plan_query(body_q, None, &bindings, &mut ptypes)?
            }
            _ => unreachable!("try_deferred_query only calls this for SetOp / With"),
        };
        Ok((
            plan.column_names().to_vec(),
            type_names(plan.column_types()),
        ))
    }

    /// Streaming secondary-index-order scan (spec/design/cost.md §3 "secondary-index order"): an
    /// `ORDER BY` the PK scan does NOT satisfy but a B-tree index does, with a `LIMIT` (the gate —
    /// `plan.phys.index_order` is `Some`). Walks the index store forward in key order (the indexed
    /// columns' order), peels the fixed-width PK suffix off the END of each entry key (the
    /// "key-suffix skip" — sound because `pk_storage_width` confirmed the suffix length), point-looks-
    /// up the row, applies the residual filter, and STOPS once the LIMIT/OFFSET window is filled — a
    /// top-N that elides the blocking sort (and, for a collated index, the `collate` units).
    ///
    /// Cost: the index tree's `page_read` is charged up front as the full block (like the streaming
    /// PK scan — only the per-row work short-circuits); each scanned entry then charges its table
    /// point-lookup's `page_read`/`value_decompress` + one `storage_row_read`, plus `row_produced`
    /// and projection `operator_eval`s per produced row. The rows match the eager sort exactly (the
    /// index order IS `ORDER BY <indexed columns> ASC NULLS LAST`, ties by PK — the stable tie-break).
    pub(crate) fn exec_index_order_scan(
        &self,
        plan: &SelectPlan,
        io: &IndexOrder,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<SelectResult> {
        let store = self.store_scoped(plan.rels[0].db.as_deref(), &plan.rels[0].table_name);
        let istore = self.index_store(&io.name_key);
        // Up-front index-tree page_read (the full block; the index store has no payload, so no slabs).
        meter.charge(COSTS.page_read * istore.node_count() as i64);

        let limit = plan.limit;
        let offset = plan.offset.unwrap_or(0);
        let mut out: Vec<Vec<Value>> = Vec::new();
        if limit != Some(0) {
            let mut passed: i64 = 0;
            let mut visit = |ekey: &[u8], _erow: &Row| -> Result<bool> {
                meter.guard()?; // enforce the cost ceiling per scanned entry (CLAUDE.md §13)
                // Peel the fixed-width PK suffix off the END of the index entry key (indexes.md §3):
                // the entry key is `<index columns> ‖ storage_key`, and `storage_key` is exactly
                // `io.pk_width` bytes — so the suffix is the row's storage key with no prefix parse.
                let row_key = &ekey[ekey.len() - io.pk_width..];
                let (row, pages, slabs) = store.get_with_units(row_key, &plan.rel_masks[0])?;
                let mut row = row.expect("an index entry references a stored row");
                meter.charge(
                    COSTS.page_read * pages as i64
                        + COSTS.value_decompress * slabs as i64
                        + COSTS.storage_row_read,
                );
                if TableStore::needs_resolution(&row, &plan.rel_masks[0]) {
                    store.resolve_columns(&mut row, &plan.rel_masks[0])?;
                }
                let keep = match &plan.filter {
                    Some(f) => f.eval(&row, env, meter)?.is_true(),
                    None => true,
                };
                if !keep {
                    return Ok(true);
                }
                passed += 1;
                if passed <= offset {
                    return Ok(true);
                }
                meter.charge(COSTS.row_produced);
                let mut projected = Vec::with_capacity(plan.projections.len());
                for p in &plan.projections {
                    projected.push(p.eval(&row, env, meter)?);
                }
                out.push(projected);
                // Stop once a LIMIT window is filled (a top-N over the index order).
                Ok(match limit {
                    Some(l) => (out.len() as i64) < l,
                    None => true,
                })
            };
            // An index store has no payload columns, so its rows carry nothing to mask — whole-row scan.
            istore.scan_range(&KeyBound::unbounded(), &mut visit)?;
        }
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows: out,
            cost: meter.accrued,
        })
    }

    /// Streaming external sort for a single-table `ORDER BY` (spec/design/spill.md §4/§5,
    /// streaming.md §4/§7). Streams scan→filter→[`Sorter`], so the input is never materialized in the
    /// executor heap; the sorter spills sorted runs to disk under `work_mem` (file-backed databases)
    /// and k-way-merges them at `finish`. Runs the **blocking part** (scan + sort + the `OFFSET` skip)
    /// and returns an [`Emitter::Sorted`] holding the [`SortedRows`] pull iterator positioned at the
    /// first output row — so the window's `row_produced` + projection is charged **lazily** by the
    /// caller's emitter drive, one row per pull (the §4/§7 output-laziness follow-on: the output `Vec`
    /// is never built and an early exit skips the rows it never pulls). Results + cost under full drain
    /// are byte-identical to the eager sort: the same `page_read` block, `storage_row_read` per scanned
    /// row, filter `operator_eval`, and `row_produced` per windowed row accrue — only the sort, which
    /// is unmetered (cost.md §3), now spills. Gated (by the caller) to a single table, no join,
    /// non-aggregate, non-DISTINCT, with an `ORDER BY` and no index bound.
    pub(crate) fn exec_streaming_sort(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
        params: &[Value],
    ) -> Result<Emitter> {
        let store = self.store_scoped(plan.rels[0].db.as_deref(), &plan.rels[0].table_name);

        // Resolve the scan bound (the PK pushdown, if any) and charge the page_read +
        // value_decompress block up front — identical to the eager scan (cost.md §3). An INDEX
        // bound never reaches here (the dispatch gate routes it to the eager path).
        let (bound, empty) = match &plan.phys.rel_bounds[0] {
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, params, env.outer, &[]) {
                Some(b) => (b, false),
                None => (KeyBound::unbounded(), true),
            },
            Some(ScanBound::Index(_))
            | Some(ScanBound::Gin(_))
            | Some(ScanBound::Gist(_))
            | Some(ScanBound::PkSet(_))
            | Some(ScanBound::IndexSet(_)) => {
                unreachable!("the streaming sort path is gated to PK/full scans")
            }
            None => (KeyBound::unbounded(), false),
        };
        let (overlap, slabs) = if empty {
            (0, 0)
        } else {
            store.overlap_scan_units(&bound, &plan.rel_masks[0])?
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);

        // Build the sorted source in `ORDER BY` order, deferring the window's row_produced +
        // projection to the lazy emitter drive (the caller). Two ways to sort, both yielding a
        // `SortedRows` pull iterator over the survivors:
        //
        // A collated ORDER BY cannot use the `C`-ordered Sorter / spill (collated keys are slice
        // 1e), and collation is in-memory only this slice — so materialize the survivors and sort
        // them with the collation-aware decorate sorter (spec/design/collation.md §8), then wrap the
        // sorted `Vec` as an in-memory `SortedRows`. The metered costs (storage_row_read per scanned
        // row, row_produced per windowed output) are identical to the Sorter path; the sort itself is
        // unmetered like every sort (cost.md §3).
        let (total, mut sorted) = if plan.order.iter().any(|(_, _, _, c)| c.is_some()) {
            let mut rows: Vec<Row> = Vec::new();
            if !empty {
                // Read-only SELECT feed: reconstruct only the touched columns (Track A1).
                store.scan_range(&bound, &mut |_key, row| {
                    meter.guard()?;
                    meter.charge(COSTS.storage_row_read);
                    let resolved = if TableStore::needs_resolution(row, &plan.rel_masks[0]) {
                        let mut r = row.clone();
                        store.resolve_columns(&mut r, &plan.rel_masks[0])?;
                        Some(r)
                    } else {
                        None
                    };
                    let row_ref = resolved.as_ref().unwrap_or(row);
                    let keep = match &plan.filter {
                        Some(f) => f.eval(row_ref, env, meter)?.is_true(),
                        None => true,
                    };
                    if keep {
                        rows.push(resolved.unwrap_or_else(|| row.clone()));
                    }
                    Ok(true)
                })?;
            }
            sort_rows(&mut rows, &plan.order)?;
            let total = rows.len() as i64;
            (total, crate::spill::SortedRows::InMemory(rows.into_iter()))
        } else {
            // Stream the scan → filter → sorter. ORDER BY is blocking, so the scan never
            // short-circuits: every in-range row is read (charging storage_row_read), its touched
            // columns resolved (large-values.md §14), the WHERE applied (charging operator_eval), and
            // a survivor pushed into the sorter, which spills when it exceeds the budget. Only
            // surviving rows are cloned.
            let mut sorter = self.new_sorter(&plan.order);
            if !empty {
                // Read-only SELECT feed: reconstruct only the touched columns (Track A1).
                store.scan_range(&bound, &mut |_key, row| {
                    meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
                    meter.charge(COSTS.storage_row_read);
                    let resolved = if TableStore::needs_resolution(row, &plan.rel_masks[0]) {
                        let mut r = row.clone();
                        store.resolve_columns(&mut r, &plan.rel_masks[0])?;
                        Some(r)
                    } else {
                        None
                    };
                    let row_ref = resolved.as_ref().unwrap_or(row);
                    let keep = match &plan.filter {
                        Some(f) => f.eval(row_ref, env, meter)?.is_true(),
                        None => true,
                    };
                    if keep {
                        sorter.push(resolved.unwrap_or_else(|| row.clone()))?;
                    }
                    Ok(true) // never stop early — the sort must see every row
                })?;
            }
            let total = sorter.total() as i64;
            (total, sorter.finish()?)
        };

        // LIMIT / OFFSET window over the sort's total row count (known without materializing the
        // output). Clamp in the i64 domain (CLAUDE.md §8). The OFFSET skip is part of the blocking
        // part (unwindowed — no row_produced), done now so `sorted` is positioned at the first output
        // row; the emitter drive then yields exactly `remaining` rows, charging row_produced +
        // projection per pull (streaming.md §4/§7).
        let start = plan.offset.unwrap_or(0).min(total);
        let end = match plan.limit {
            Some(lim) if lim < total - start => start + lim,
            _ => total,
        };
        for _ in 0..start {
            sorted.next()?; // skip the OFFSET rows (unwindowed — no row_produced)
        }
        Ok(Emitter::Sorted {
            sorted,
            remaining: (end - start) as usize,
        })
    }

    /// Streaming two-table INNER/CROSS join whose `ORDER BY` is satisfied by the OUTER (first)
    /// relation's primary-key scan order (cost.md §3 "secondary-index order" companion — the join
    /// top-N). A left-deep nested loop produces combined rows in `(outer PK, inner key)` order — which
    /// IS the requested order, since the outer drives the loop in PK order — so the blocking sort is
    /// elided, and with a `LIMIT` the loop STOPS once the window is filled. An ordinary inner is
    /// materialized once; an index-nested-loop inner is opened per outer row and later seeks are
    /// skipped when the window fills. Gated (by the caller / `plan.phys.join_pk_ordered`) to exactly
    /// two non-lateral base relations, an INNER or CROSS join, a `LIMIT`, and an `ORDER BY` the outer
    /// PK satisfies.
    pub(crate) fn exec_streaming_join(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
        params: &[Value],
        outer: &[&[Value]],
        stmt_rng: &std::cell::Cell<crate::seam::StmtRng>,
    ) -> Result<SelectResult> {
        // Materialize the outer once, in primary-key order. An ordinary inner is materialized once
        // too; an INL inner is opened below per outer row. Scan units accrue inside materialize_rel.
        let left_rows =
            self.materialize_rel(plan, 0, params, outer, &[], stmt_rng, env.ctes, meter)?;
        let right_inl = plan.phys.rel_inl_bounds[1].is_some();
        let right_rows = if right_inl {
            Vec::new()
        } else {
            self.materialize_rel(plan, 1, params, outer, &[], stmt_rng, env.ctes, meter)?
        };
        let on = &plan.joins[0].on;

        let limit = plan.limit;
        let offset = plan.offset.unwrap_or(0);
        let mut out: Vec<Vec<Value>> = Vec::new();
        if limit != Some(0) {
            let mut passed: i64 = 0;
            'outer: for left in &left_rows {
                let inner_rows;
                let current_right = if right_inl {
                    inner_rows = self
                        .materialize_rel(plan, 1, params, outer, left, stmt_rng, env.ctes, meter)?;
                    &inner_rows
                } else {
                    &right_rows
                };
                for right in current_right {
                    let mut combined = left.clone();
                    combined.extend_from_slice(right);
                    // INNER: keep the pair iff its ON is TRUE (3VL); CROSS: keep every pair (no ON).
                    let keep = match on {
                        Some(pred) => pred.eval(&combined, env, meter)?.is_true(),
                        None => true,
                    };
                    if !keep {
                        continue;
                    }
                    // The residual WHERE over the combined row (per surviving pair).
                    let pass = match &plan.filter {
                        Some(f) => f.eval(&combined, env, meter)?.is_true(),
                        None => true,
                    };
                    if !pass {
                        continue;
                    }
                    passed += 1;
                    if passed <= offset {
                        continue;
                    }
                    meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
                    meter.charge(COSTS.row_produced);
                    let mut projected = Vec::with_capacity(plan.projections.len());
                    for p in &plan.projections {
                        projected.push(p.eval(&combined, env, meter)?);
                    }
                    out.push(projected);
                    // Stop the whole nested loop once the LIMIT window is filled.
                    if let Some(l) = limit
                        && out.len() as i64 >= l
                    {
                        break 'outer;
                    }
                }
            }
        }
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows: out,
            cost: meter.accrued,
        })
    }

    /// Build an [`Sorter`](crate::spill::Sorter) for `order`, bounded by this handle's `work_mem`.
    /// Spilling is enabled only for a **file-backed** database (an in-memory one has nowhere to
    /// spill — spill.md §2); spill runs live next to the database file (same filesystem, guaranteed
    /// writable), falling back to the system temp dir.
    pub(crate) fn new_sorter(&self, order: &[crate::spill::SortKey]) -> crate::spill::Sorter {
        let spill_dir = if self.paging.is_some() {
            let dir = self
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(std::env::temp_dir);
            Some(dir)
        } else {
            None
        };
        crate::spill::Sorter::new(order.to_vec(), self.session.work_mem, spill_dir)
    }

    /// Materialize one FROM relation `ri` into its rows, given the current outer-row stack `outer`
    /// (spec/design/grammar.md §15/§44). A base table is scanned (a PK/index bound may seek via
    /// `outer`); an SRF is generated; a CTE / derived table is delivered / run in place. For a
    /// CORRELATED `LATERAL` relation (§44) the caller passes `outer` EXTENDED with the combined
    /// left-hand row, so the body / SRF args read that row as their immediate outer; a non-lateral
    /// relation is passed the query's own `outer` and its `parent = None` body simply ignores it
    /// (a `parent = None` plan holds no `OuterColumn`, so the two are observably identical).
    pub(crate) fn materialize_rel(
        &self,
        plan: &SelectPlan,
        ri: usize,
        params: &[Value],
        outer: &[&[Value]],
        left: &[Value],
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        ctes: CteCtx,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let rel = &plan.rels[ri];
        let env = EvalEnv {
            exec: self,
            params,
            outer,
            rng,
            ctes,
        };
        // A set-returning relation is generated, not scanned (functions.md §10): produce its rows,
        // charging generated_row per element (its args read `outer` — implicitly lateral, §44).
        if let Some(srf) = &rel.srf {
            return match srf.kind {
                SrfKind::GenerateSeries => self.generate_series_rows(srf, &env, meter),
                SrfKind::Unnest => self.unnest_rows(srf, &env, meter),
                SrfKind::JsonbArrayElements
                | SrfKind::JsonbArrayElementsText
                | SrfKind::JsonbObjectKeys
                | SrfKind::JsonObjectKeys
                | SrfKind::JsonbEach
                | SrfKind::JsonbEachText
                | SrfKind::JsonRecord { .. }
                | SrfKind::JsonbPathQuery => self.json_srf_rows(srf, &env, meter),
                SrfKind::JsonTable => self.json_table_rows(srf, &env, meter),
                SrfKind::JedTables => self.jed_tables_rows(srf, meter),
                SrfKind::JedColumns => self.jed_columns_rows(srf, meter),
                SrfKind::JedIndexes => self.jed_indexes_rows(srf, meter),
                SrfKind::JedConstraints => self.jed_constraints_rows(srf, meter),
            };
        }
        // A CTE reference delivers its rows from the per-statement context (cte.md §3/§5): a
        // MATERIALIZED CTE reads its buffer (charging cte_scan_row, guarded so a runaway scan aborts
        // 54P01); an INLINE CTE runs its body in place. (A CTE is never lateral.)
        if let Some(ci) = rel.cte {
            let rows = match env.ctes.modes[ci] {
                CteMode::Materialize => {
                    let buf = &env.ctes.buffers[ci];
                    for _ in buf {
                        meter.guard()?;
                        meter.charge(COSTS.cte_scan_row);
                    }
                    buf.clone()
                }
                CteMode::Inline => {
                    // Only a plain (query) CTE is ever inlined; a data-modifying CTE is always
                    // materialized (writable-cte.md §3), so its buffer was filled above.
                    let CteSource::Query(plan) = &env.ctes.bindings[ci].source else {
                        unreachable!("a data-modifying CTE is always materialized, never inlined")
                    };
                    let r = self.exec_query_plan(plan, outer, params, env.ctes)?;
                    meter.charge(r.cost);
                    r.rows
                }
            };
            return Ok(rows);
        }
        // A DERIVED TABLE runs its body in place (grammar.md §42), charging its intrinsic cost — no
        // cte_scan_row. Non-lateral it was planned `parent = None` and ignores `outer`; a LATERAL
        // body (§44) reads the left-hand row from `outer`.
        if let Some(dp) = &rel.derived {
            let r = self.exec_query_plan(dp, outer, params, env.ctes)?;
            meter.charge(r.cost);
            return Ok(r.rows);
        }
        // A base table: scan in primary-key order via a ScanSource (the page_read block + per-row
        // storage_row_read accrue inside next() — cost.md §3). A PK/index bound seeks/ranges instead
        // of a full walk; an empty bound reads nothing. An index-nested-loop bound (`rel_inl_bounds`)
        // takes precedence and resolves its `Sibling` source from the current left row (cost.md §3
        // "JOIN"); else the once-materialized `rel_bounds`.
        let store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
        let inl = plan.phys.rel_inl_bounds[ri].is_some();
        let bound = plan.phys.rel_inl_bounds[ri]
            .as_ref()
            .or(plan.phys.rel_bounds[ri].as_ref());
        let (mut rows, (node_count, slabs)) = match bound {
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, params, outer, left) {
                Some(b) => {
                    // Read-only SELECT feed: reconstruct only the touched columns (Track A1) — a Packed
                    // leaf skips decoding the untouched ones. Cost- and result-identical to the whole-row
                    // scan for a consumer that reads only the touched set (packed-leaf.md §11).
                    let (entries, pages, slabs) =
                        store.range_scan_with_units(&b, &plan.rel_masks[ri])?;
                    let rows = entries.into_iter().map(|(_, v)| v).collect();
                    (rows, (pages, slabs))
                }
                None => (Vec::new(), (0, 0)),
            },
            Some(ScanBound::Index(ib)) => self.index_bound_rows(
                &rel.table_name,
                ib,
                params,
                outer,
                &plan.rel_masks[ri],
                left,
            )?,
            Some(ScanBound::Gin(gb)) => {
                let query = if inl {
                    [plan.joins[ri - 1].on.as_ref(), plan.filter.as_ref()]
                        .into_iter()
                        .flatten()
                        .find_map(|f| {
                            gin_sibling_match(f, gb.col_global, rel.offset).map(|(_, q)| q)
                        })
                } else {
                    plan.filter
                        .as_ref()
                        .and_then(|f| gin_match(f, gb.col_global).map(|(_, q)| q))
                };
                let (pairs, units) = self.gin_bound_rows(
                    &rel.table_name,
                    gb,
                    query,
                    left,
                    &env,
                    meter,
                    &plan.rel_masks[ri],
                    false,
                )?;
                // SELECT discards the storage keys (UPDATE/DELETE keep them — gin.md §6).
                (pairs.into_iter().map(|(_, v)| v).collect(), units)
            }
            Some(ScanBound::Gist(gb)) => {
                let query = if inl {
                    [plan.joins[ri - 1].on.as_ref(), plan.filter.as_ref()]
                        .into_iter()
                        .flatten()
                        .find_map(|f| match gb.strategy {
                            crate::gist::GistStrategy::Equal => {
                                gist_scalar_sibling_match(f, gb.col_global, rel.offset)
                                    .map(|(_, q)| q)
                            }
                            _ => gist_sibling_match(f, gb.col_global, rel.offset).map(|(_, q)| q),
                        })
                } else {
                    plan.filter.as_ref().and_then(|f| gist_query_operand(f, gb))
                };
                let (pairs, units) = self.gist_bound_rows(
                    &rel.table_name,
                    gb,
                    query,
                    left,
                    &env,
                    meter,
                    &plan.rel_masks[ri],
                    false,
                )?;
                // SELECT discards the storage keys (UPDATE/DELETE keep them — gist.md §5).
                (pairs.into_iter().map(|(_, v)| v).collect(), units)
            }
            Some(ScanBound::PkSet(ks)) => {
                // Merged PK point-set (cost.md §3 "OR / IN-list"): a union of point probes over the
                // distinct sorted keys; the whole WHERE stays the residual filter downstream.
                let (entries, units) = self.pk_key_set_rows(
                    store,
                    ks,
                    params,
                    outer,
                    &plan.rel_masks[ri],
                    left,
                    true,
                )?;
                // SELECT discards the storage keys (UPDATE/DELETE keep them).
                (entries.into_iter().map(|(_, v)| v).collect(), units)
            }
            Some(ScanBound::IndexSet(ks)) => {
                // Merged secondary-index point-set (cost.md §3 "OR / IN-list").
                self.index_key_set_rows(
                    &rel.table_name,
                    ks,
                    params,
                    outer,
                    &plan.rel_masks[ri],
                    left,
                )?
            }
            None => {
                // Read-only full-scan SELECT feed: reconstruct only the touched columns (Track A1).
                let (entries, pages, slabs) = store.scan_with_units(&plan.rel_masks[ri])?;
                let rows = entries.into_iter().map(|(_, v)| v).collect();
                (rows, (pages, slabs))
            }
        };
        // Materialize this relation's touched columns where the lazy load left unfetched references
        // (large-values.md §14) — exactly the static set the cost block charges.
        for row in &mut rows {
            store.resolve_columns(row, &plan.rel_masks[ri])?;
        }
        meter.charge(COSTS.value_decompress * slabs as i64);
        let mut src = ScanSource::new(rows, node_count as i64);
        let mut table_rows: Vec<Row> = Vec::new();
        while let Some(row) = src.next(meter)? {
            table_rows.push(row);
        }
        Ok(table_rows)
    }

    /// Execute a resolved SELECT against an outer-row environment (`outer` = the enclosing
    /// rows, innermost last; empty at top level) and the bound parameters. The execute half of
    /// the old `run_select`: materialize, nested-loop join, WHERE, then aggregate / DISTINCT /
    /// window + project. The per-row evaluator gets an `EvalEnv` carrying the engine + outer
    /// rows, so a correlated subquery in any clause re-executes against them (grammar.md §26).
    pub(crate) fn exec_select_plan(
        &self,
        plan: &SelectPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        // Run the blocking part to an [`Emitter`], then drive the emission EAGERLY into a `Vec` (the
        // materialized `execute()` path the conformance corpus drives — byte-unchanged). The lazy
        // `query()` path drives the SAME `Emitter` row by row via `BufferedScan` (streaming.md §4);
        // both charge the identical units at the identical sites, so the totals agree (streaming.md §6).
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let mut meter = self.session.new_meter();
        let emitter = self.exec_select_emit(plan, outer, params, ctes, &stmt_rng, &mut meter)?;
        let out_rows = match emitter {
            // Already projected + charged (the special input-streaming paths) — hand the rows out.
            Emitter::Final { rows } => rows,
            // The streaming sort's lazy output: pull every windowed row from the `SortedRows`
            // iterator, charging `row_produced` + the projection per row — exactly the eager window
            // loop `exec_streaming_sort` ran before its output went lazy (streaming.md §4/§7).
            Emitter::Sorted {
                mut sorted,
                remaining,
            } => {
                let env = EvalEnv {
                    exec: self,
                    params,
                    outer,
                    rng: &stmt_rng,
                    ctes,
                };
                let mut out = Vec::with_capacity(remaining);
                for _ in 0..remaining {
                    let row = sorted
                        .next()?
                        .expect("the sorter yields exactly the windowed rows");
                    meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
                    meter.charge(COSTS.row_produced);
                    let mut o = Vec::with_capacity(plan.projections.len());
                    for p in &plan.projections {
                        o.push(p.eval(&row, &env, &mut meter)?);
                    }
                    out.push(o);
                }
                out
            }
            // The general blocking buffer: window it, charging `row_produced` per emitted row (and,
            // in `Project` mode, the projection list) — exactly the eager emission these branches ran
            // before the S4 split.
            Emitter::Buffer {
                mut rows,
                start,
                end,
                mode,
            } => match mode {
                EmitMode::Identity => {
                    let mut out = Vec::with_capacity(end - start);
                    for row in rows.drain(start..end) {
                        meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
                        meter.charge(COSTS.row_produced);
                        out.push(row);
                    }
                    out
                }
                EmitMode::Project => {
                    let env = EvalEnv {
                        exec: self,
                        params,
                        outer,
                        rng: &stmt_rng,
                        ctes,
                    };
                    let mut out = Vec::with_capacity(end - start);
                    for row in &rows[start..end] {
                        meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
                        meter.charge(COSTS.row_produced);
                        let mut o = Vec::with_capacity(plan.projections.len());
                        for p in &plan.projections {
                            o.push(p.eval(row, &env, &mut meter)?);
                        }
                        out.push(o);
                    }
                    out
                }
            },
            // Columnar projection (packed-leaf.md §11 Track A2/A3): gather each windowed output row from
            // the dense lanes — a bare-column projection with no full-width row — charging row_produced per
            // row, exactly the Project drive over a bare-column projection (whose eval is a zero-cost slot
            // read). A non-None `sel` (the A3 filter's survivors) maps output row j to lane position sel[j].
            Emitter::Columnar {
                cols,
                proj_cols,
                sel,
                start,
                end,
            } => {
                let mut out = Vec::with_capacity(end - start);
                for j in start..end {
                    meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
                    meter.charge(COSTS.row_produced);
                    let l = match &sel {
                        Some(s) => s[j] as usize,
                        None => j,
                    };
                    let mut o = Vec::with_capacity(proj_cols.len());
                    for &c in &proj_cols {
                        o.push(cols[c][l].clone());
                    }
                    out.push(o);
                }
                out
            }
        };
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows: out_rows,
            cost: meter.accrued,
        })
    }

    /// Run the A2/A3 columnar gather for a [`vectorized_project_eligible`] plan (packed-leaf.md §11 Track
    /// A2/A3): scan only the touched columns of the single base relation into dense per-column lanes
    /// (never a full-width [`Row`]), charge the identical scan cost block, apply any `WHERE` predicate over
    /// the lanes into a selection vector (A3), and return an [`Emitter::Columnar`] that gathers each
    /// surviving output row from the lanes on emission. Returns `None` (declining to the caller's row path)
    /// for an in-memory store (its Decoded leaves share rows zero-copy, so a lane gather would only add
    /// allocation), a spillable touched column (the columnar feed has no value-resolution step — this also
    /// covers a filter over a spillable column), or a projection column out of range / unmasked (a safety
    /// net, never expected — a projected column is touched, hence masked). Cost-neutral by construction:
    /// the same `page_read` (same node visits) / `value_decompress` (0) / `storage_row_read` (× row_count)
    /// / `operator_eval` (the filter over each scanned row) as the row path, then `row_produced` per emitted
    /// (surviving) row charged by the emitter drive — exactly the `Project` drive over a bare-column
    /// projection.
    pub(crate) fn project_columnar(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        params: &[Value],
        outer: &[&[Value]],
        meter: &mut Meter,
    ) -> Result<Option<Emitter>> {
        let rel = &plan.rels[0];
        let store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
        // File-backed only: an in-memory store's row path is already zero-copy.
        if !store.is_file_backed() {
            return Ok(None);
        }
        let mask = &plan.rel_masks[0];
        // No touched column may spill — so the feed's value_decompress slab count is 0 and no unfetched
        // value is left unresolved. The mask includes the filter's columns (collect_touched), so this also
        // declines a filter over a spillable column to the row path.
        if crate::format::any_spillable_masked(store.col_types(), mask) {
            return Ok(None);
        }
        // Each projected column must be a valid, masked table ordinal — else its gathered lane would be
        // empty. A projected column is always touched (hence masked), so this holds; the check also
        // declines a (never-expected) synthetic slot or non-column projection.
        let mut proj_cols = Vec::with_capacity(plan.projections.len());
        for p in &plan.projections {
            let RExpr::Column(idx) = p else {
                return Ok(None);
            };
            let idx = *idx;
            if idx >= mask.len() || !mask[idx] {
                return Ok(None);
            }
            proj_cols.push(idx);
        }

        // Determine the scan bound exactly as materialize_rel does: a PK-range bound, or the full scan. An
        // empty bound (a contradictory PK predicate) admits no rows — skip the scan entirely (0 pages/rows).
        let mut cols: Vec<Vec<Value>> = vec![Vec::new(); mask.len()];
        let mut row_count = 0usize;
        let mut pages = 0usize;
        let slabs = 0usize;
        let mut do_scan = true;
        let mut b = KeyBound::unbounded();
        if let Some(ScanBound::Pk(bp)) = &plan.phys.rel_bounds[0] {
            match build_key_bound(bp, params, outer, &[]) {
                Some(bb) => b = bb,
                None => do_scan = false,
            }
        }
        if do_scan {
            let (c, rc, p, _s) = store.columnar_scan_masked(&b, mask)?;
            cols = c;
            row_count = rc;
            pages = p;
        }
        // Charge the scan cost block identically to materialize_rel + ScanSource: page_read × nodes,
        // value_decompress × slabs (0 here), storage_row_read × row_count. On the unmetered lane (the caller
        // gates) this bulk charge reproduces the per-row accrual (guard is a no-op).
        meter.charge(
            COSTS.page_read * pages as i64
                + COSTS.value_decompress * slabs as i64
                + COSTS.storage_row_read * row_count as i64,
        );

        // A3: apply the WHERE predicate over the lanes into a selection vector (None ⇒ all rows survive).
        let (sel, n_emit) = match &plan.filter {
            Some(filter) => {
                let s = filter_columnar(filter, &cols, mask, row_count, env, meter)?;
                let n = s.len();
                (Some(s), n)
            }
            None => (None, row_count),
        };

        Ok(Some(Emitter::Columnar {
            cols,
            proj_cols,
            sel,
            start: 0,
            end: n_emit,
        }))
    }

    /// Whether `plan` is a shape [`exec_vectorized_agg`](Engine::exec_vectorized_agg) specializes: a
    /// single-base-table `SUM`/`COUNT`/`MIN`/`MAX`/`AVG` with no `DISTINCT` / `FILTER` / `HAVING` /
    /// window / `ORDER BY`, over a full or primary-key-bounded scan, that is EITHER whole-table (no
    /// `GROUP BY`) OR grouped by a single bare integer column. Mostly pure plan inspection — it charges
    /// nothing, so a bail is free and the general path runs with identical results + cost; the
    /// single-key case additionally reads the group-key column's static type from the table store (a
    /// one-time lookup, not per row) to confirm it is a scalar integer (so the int64-keyed bucket is a
    /// bijection of the value-canonical group key — see [`group_by_int_key`]).
    pub(crate) fn vectorized_agg_eligible(&self, plan: &SelectPlan) -> bool {
        if !plan.is_agg {
            return false;
        }
        // One base table, no join.
        if plan.rels.len() != 1 || !plan.joins.is_empty() {
            return false;
        }
        let rel = &plan.rels[0];
        if rel.srf.is_some() || rel.cte.is_some() || rel.derived.is_some() || rel.lateral {
            return false;
        }
        // Full scan or a primary-key bound only — an index / GIN / GiST bound changes the scan
        // mechanics and residual filter, so it keeps the scalar path.
        if matches!(
            plan.phys.rel_bounds[0],
            Some(ScanBound::Index(_))
                | Some(ScanBound::Gin(_))
                | Some(ScanBound::Gist(_))
                | Some(ScanBound::PkSet(_))
                | Some(ScanBound::IndexSet(_))
        ) {
            return false;
        }
        // Exactly one grouping set (ROLLUP/CUBE/GROUPING SETS produce several — deferred), no
        // materialized expression keys (`GROUP BY a + b`), and no GROUPING() calls.
        if plan.group_sets.len() != 1
            || !plan.group_exprs.is_empty()
            || !plan.grouping_specs.is_empty()
        {
            return false;
        }
        let gset = &plan.group_sets[0];
        match gset.key_cols.len() {
            // Whole-table aggregation: the () grand-total group, no master grouping columns.
            0 => {
                if !plan.group_keys.is_empty() {
                    return false;
                }
            }
            // Single-key GROUP BY: the sole master grouping column is this key, its synthetic slot is
            // 0, and the key is a bare scalar-INTEGER column of the base table (so the int64 bucket key
            // is a bijection of the scalar path's value-canonical group key).
            1 => {
                if plan.group_keys.len() != 1 || plan.group_keys[0] != gset.key_cols[0] {
                    return false;
                }
                if gset.slot_src.len() != 1 || gset.slot_src[0] != Some(0) {
                    return false;
                }
                let store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
                let ord = gset.key_cols[0].wrapping_sub(rel.offset);
                match store.col_types().get(ord) {
                    Some(ColType::Scalar(s)) if s.is_integer() => {}
                    _ => return false,
                }
            }
            _ => return false,
        }
        // No blocking / re-shaping operator beyond the fold. LIMIT/OFFSET is honored via the window
        // bounds below, so it need not bail.
        if plan.distinct || plan.having.is_some() || plan.has_window || !plan.order.is_empty() {
            return false;
        }
        if plan.agg_specs.is_empty() {
            return false;
        }
        plan.agg_specs.iter().all(vectorized_spec_eligible)
    }

    /// Run a [`vectorized_agg_eligible`](Engine::vectorized_agg_eligible) plan and return the already-
    /// grouped output as an [`Emitter::Buffer`] with [`EmitMode::Project`] under the query's
    /// LIMIT/OFFSET window — exactly the emitter the scalar aggregate branch returns, so emission +
    /// cost are identical either way. It reuses the scalar scan ([`materialize_rel`](Engine::materialize_rel))
    /// + `WHERE` for exact cost + survivor determination, then folds each aggregate over the survivors
    /// with the shared [`Acc`] (byte-identical acc state, hence finalize). A file-backed store gathers
    /// only its touched columns columnar ([`agg_columnar`](Engine::agg_columnar) — never a full-width
    /// row, the allocation dividend); an in-memory store (or a columnar decline) folds over the
    /// materialized rows. Only runs on the unmetered lane (the caller gates).
    pub(crate) fn exec_vectorized_agg(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Emitter> {
        let gset = &plan.group_sets[0];

        // A2/A3 columnar fast path (packed-leaf.md §11 Track A2/A3): a file-backed aggregate gathers
        // only its touched columns into dense lanes and folds columnar — never a full-width row. A
        // WHERE predicate (A3) is applied over the lanes into a selection vector rather than forcing
        // the row path. Declines (None) to the row path below for an in-memory store or a spillable
        // touched column. Cost-neutral by construction (agg_columnar charges the identical scan block).
        let srows = match self.agg_columnar(plan, gset, env, meter)? {
            Some(srows) => srows,
            None => {
                // Row path: scan the single base relation through the same path the eager executor
                // uses, so the page_read / value_decompress / storage_row_read block is charged
                // identically (materialize_rel), then apply the residual WHERE per scanned row through
                // the ordinary evaluator (its operator_eval charges + 3VL survivor test byte-identical
                // to the scalar WHERE loop).
                let rows = self.materialize_rel(
                    plan,
                    0,
                    env.params,
                    env.outer,
                    &[],
                    env.rng,
                    env.ctes,
                    meter,
                )?;
                let survivors: Vec<Row> = match &plan.filter {
                    None => rows,
                    Some(f) => {
                        let mut out: Vec<Row> = Vec::new();
                        for r in rows {
                            if f.eval(&r, env, meter)?.is_true() {
                                out.push(r);
                            }
                        }
                        out
                    }
                };
                let src = LaneSrc::Rows(&survivors);
                if gset.key_cols.is_empty() {
                    vec![fold_agg_whole(
                        &plan.agg_specs,
                        &src,
                        survivors.len(),
                        meter,
                    )?]
                } else {
                    group_by_int_key(
                        &plan.agg_specs,
                        gset.key_cols[0],
                        &src,
                        survivors.len(),
                        meter,
                    )?
                }
            }
        };

        // LIMIT/OFFSET window over the synthetic rows, mirroring the scalar branch's window_bounds
        // (clamped in the i64 domain before indexing). Emit as Buffer{Project} — the drive charges
        // row_produced + the projection over each windowed synthetic row exactly as for a scalar
        // aggregate result.
        let n = srows.len();
        let start = plan.offset.unwrap_or(0).min(n as i64) as usize;
        let end = match plan.limit {
            Some(lim) if lim < (n - start) as i64 => start + lim as usize,
            _ => n,
        };
        Ok(Emitter::Buffer {
            rows: srows,
            start,
            end,
            mode: EmitMode::Project,
        })
    }

    /// Run the A2/A3 columnar gather for a vectorized aggregate (packed-leaf.md §11 Track A2/A3): scan
    /// only the touched columns of the single base relation into dense per-column lanes (never a
    /// full-width row), charge the identical scan cost block, apply any `WHERE` predicate over the
    /// lanes into a selection vector (A3), and fold each aggregate columnar over the survivors —
    /// returning the finalized synthetic rows (the whole-table grand total or one per group). Returns
    /// `None` (declining to the caller's row path) when the store is in-memory (its Decoded leaves
    /// share their rows zero-copy, so a lane gather would only add allocation), when a touched column
    /// can spill (the columnar feed has no value-resolution step — this also covers a filter over a
    /// spillable column), or when a needed column ordinal is out of range / unmasked (a safety net,
    /// never expected for an eligible plan). Cost-neutral by construction: same page_read /
    /// value_decompress (0) / storage_row_read / operator_eval (the filter) / aggregate_accumulate /
    /// row_produced as the row path.
    pub(crate) fn agg_columnar(
        &self,
        plan: &SelectPlan,
        gset: &GroupSetPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let rel = &plan.rels[0];
        let store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
        // File-backed only: an in-memory store's row path is already zero-copy.
        if !store.is_file_backed() {
            return Ok(None);
        }
        let mask = &plan.rel_masks[0];
        // No touched column may spill — so the feed's value_decompress slab count is 0 and no unfetched
        // value is left unresolved. An eligible aggregate touches only integer operands + an integer
        // key (plus the filter's columns); this declines a filter or operand over a spillable column.
        if crate::format::any_spillable_masked(store.col_types(), mask) {
            return Ok(None);
        }
        // Every column the fold reads (each aggregate operand + the group key) must be a valid, masked
        // table ordinal — else its gathered lane would be empty. This also declines a (never-expected)
        // non-zero relation offset.
        let need = |idx: usize| idx < mask.len() && mask[idx];
        for spec in &plan.agg_specs {
            if let Some(RExpr::Column(idx)) = &spec.operand
                && !need(*idx)
            {
                return Ok(None);
            }
        }
        if let Some(&kc) = gset.key_cols.first()
            && !need(kc)
        {
            return Ok(None);
        }

        // Determine the scan bound exactly as materialize_rel does: a PK-range bound, or the full scan.
        // An empty bound (a contradictory PK predicate) admits no rows — skip the scan entirely.
        let mut do_scan = true;
        let mut b = KeyBound::unbounded();
        if let Some(ScanBound::Pk(bp)) = &plan.phys.rel_bounds[0] {
            match build_key_bound(bp, env.params, env.outer, &[]) {
                Some(bb) => b = bb,
                None => do_scan = false,
            }
        }

        let grouped = !gset.key_cols.is_empty();
        let key_col = gset.key_cols.first().copied().unwrap_or(0);

        // Fold each scanned row's touched columns straight into its accumulator during ONE tree walk —
        // no per-column lane is materialized, so a whole-table / single-int-key aggregate is O(1) memory
        // instead of the O(rows) whole-column gather the lane path paid (float.md §7, packed-leaf.md
        // §11). A WHERE predicate (A3) is evaluated over a single reusable masked scratch row read via
        // col_at (untouched columns NULL) — byte-identical input + operator_eval to the lane filter.
        let mut whole: Vec<Acc> = if grouped {
            Vec::new()
        } else {
            plan.agg_specs.iter().map(Acc::from_spec).collect()
        };
        let mut groups: Vec<(Value, Vec<Acc>)> = Vec::new();
        let mut index: HashMap<i64, usize> = HashMap::new();
        let mut null_gi: Option<usize> = None;
        let mut scratch: Vec<Value> = if plan.filter.is_some() {
            vec![Value::Null; mask.len()]
        } else {
            Vec::new()
        };
        let mut nsurv = 0usize;

        let (row_count, pages) = if do_scan {
            let mut visit = |node: &crate::pmap::Node, i: usize| -> Result<()> {
                if let Some(filter) = &plan.filter {
                    for (c, &m) in mask.iter().enumerate() {
                        if m {
                            scratch[c] = node.col_at(i, c)?;
                        }
                    }
                    if !filter.eval(&scratch, env, meter)?.is_true() {
                        return Ok(());
                    }
                }
                nsurv += 1;
                let accs: &mut Vec<Acc> = if grouped {
                    let gi = match node.col_at(i, key_col)? {
                        Value::Int(k) => match index.get(&k) {
                            Some(&g) => g,
                            None => {
                                let g = groups.len();
                                index.insert(k, g);
                                groups.push((
                                    Value::Int(k),
                                    plan.agg_specs.iter().map(Acc::from_spec).collect(),
                                ));
                                g
                            }
                        },
                        // A NULL integer key buckets into one sentinel group (the value-canonical key
                        // groups all NULLs together — matching the scalar/lane path).
                        _ => match null_gi {
                            Some(g) => g,
                            None => {
                                let g = groups.len();
                                null_gi = Some(g);
                                groups.push((
                                    Value::Null,
                                    plan.agg_specs.iter().map(Acc::from_spec).collect(),
                                ));
                                g
                            }
                        },
                    };
                    &mut groups[gi].1
                } else {
                    &mut whole
                };
                for (si, spec) in plan.agg_specs.iter().enumerate() {
                    let v = match operand_col(spec) {
                        Some(c) => node.col_at(i, c)?,
                        None => Value::Null, // COUNT(*) folds no value
                    };
                    accs[si].fold(v, meter)?;
                }
                Ok(())
            };
            store.fold_scan_masked(&b, &mut visit)?
        } else {
            (0, 0)
        };

        // Charge the identical cost totals (unmetered lane — charge order is invisible): the scan block
        // (page_read × nodes; value_decompress × 0 — no spillable touched column gated above;
        // storage_row_read × row_count) and aggregate_accumulate once per (survivor × spec). The
        // filter's operator_eval was charged per scanned row inside the walk.
        meter.charge(COSTS.page_read * pages as i64 + COSTS.storage_row_read * row_count as i64);
        meter.charge(COSTS.aggregate_accumulate * nsurv as i64 * plan.agg_specs.len() as i64);

        let srows = if grouped {
            groups
                .into_iter()
                .map(|(key, accs)| {
                    let mut srow = Vec::with_capacity(1 + accs.len());
                    srow.push(key);
                    for a in accs {
                        srow.push(a.finalize()?);
                    }
                    Ok(srow)
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![
                whole
                    .into_iter()
                    .map(Acc::finalize)
                    .collect::<Result<Vec<_>>>()?,
            ]
        };
        Ok(Some(srows))
    }
}
