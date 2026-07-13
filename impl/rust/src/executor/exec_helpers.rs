//! Streaming cursor RowStream impls plus SELECT-execution helper free functions (mirrors parts of
//! impl/go exec_scan.go/exec_emit.go): the LaneSrc/StreamingScan/BufferedScan/DeferredResult cursor
//! adapters, the whole-relation aggregate fast path, outer-reference/touched-column analysis, and
//! GROUPING SETS/ROLLUP/CUBE expansion.

use super::*;

impl LaneSrc<'_> {
    #[inline]
    pub(crate) fn at(&self, j: usize, col: usize) -> &Value {
        match self {
            LaneSrc::Rows(rows) => &rows[j][col],
            LaneSrc::Cols { cols, sel } => {
                let i = match sel {
                    Some(s) => s[j] as usize,
                    None => j,
                };
                &cols[col][i]
            }
        }
    }
}

/// Fold one WHOLE-TABLE grand-total group over `nsurv` survivors from `src`, returning the finalized
/// aggregate results `[agg_0, …]` (the synthetic row for a `()` group — no key columns). It builds one
/// [`Acc`] per spec and folds each survivor's operand value through the shared [`Acc::fold`] (identical
/// acc state, hence [`Acc::finalize`], to the scalar path), charging `aggregate_accumulate` once per
/// (survivor × spec) in bulk — the identical total to the scalar loop (which charges per row × spec),
/// and cost-safe because the caller gates to the unmetered lane (no per-row guard to preserve).
pub(crate) fn fold_agg_whole(
    specs: &[AggSpec],
    src: &LaneSrc,
    nsurv: usize,
    meter: &mut Meter,
) -> Result<Vec<Value>> {
    let mut accs: Vec<Acc> = specs.iter().map(Acc::from_spec).collect();
    for (si, spec) in specs.iter().enumerate() {
        meter.charge(COSTS.aggregate_accumulate * nsurv as i64);
        let oc = operand_col(spec);
        for j in 0..nsurv {
            let v = match oc {
                Some(c) => src.at(j, c).clone(),
                None => Value::Null, // COUNT(*) folds no value
            };
            accs[si].fold(v, meter)?;
        }
    }
    accs.into_iter().map(Acc::finalize).collect()
}

/// Bucket `nsurv` survivors from `src` by their single INTEGER group-key column and fold each aggregate
/// per group, returning the finalized synthetic rows `[key, agg_0, …]` in scan-order-of-first-
/// appearance. The bucket is a `HashMap<i64, usize>` over the raw key (a bijection of the scalar path's
/// value-canonical group key for a fixed-width integer column) plus one sentinel group for NULL keys
/// (the value-canonical key groups all NULLs together). The fold reuses [`Acc::fold`] (byte-identical
/// acc state); `aggregate_accumulate` is charged once per (survivor × spec) in bulk — the identical
/// total to the scalar loop. The bucketing itself is unmetered (cost.md §3), so the `i64` map is a free
/// internal choice. The caller has verified the key lane (and each operand lane) is populated.
pub(crate) fn group_by_int_key(
    specs: &[AggSpec],
    key_col: usize,
    src: &LaneSrc,
    nsurv: usize,
    meter: &mut Meter,
) -> Result<Vec<Vec<Value>>> {
    let mut groups: Vec<(Value, Vec<Acc>)> = Vec::new();
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut null_gi: Option<usize> = None;

    meter.charge(COSTS.aggregate_accumulate * nsurv as i64 * specs.len() as i64);
    for j in 0..nsurv {
        let kv = src.at(j, key_col);
        let gi = match kv {
            Value::Int(k) => match index.get(k) {
                Some(&g) => g,
                None => {
                    let g = groups.len();
                    index.insert(*k, g);
                    groups.push((kv.clone(), specs.iter().map(Acc::from_spec).collect()));
                    g
                }
            },
            // A NULL integer key (the only other case for an integer column) buckets into one sentinel
            // group, exactly as the scalar path groups all NULLs together.
            _ => match null_gi {
                Some(g) => g,
                None => {
                    let g = groups.len();
                    null_gi = Some(g);
                    groups.push((Value::Null, specs.iter().map(Acc::from_spec).collect()));
                    g
                }
            },
        };
        let accs = &mut groups[gi].1;
        for (si, spec) in specs.iter().enumerate() {
            let v = match operand_col(spec) {
                Some(c) => src.at(j, c).clone(),
                None => Value::Null,
            };
            accs[si].fold(v, meter)?;
        }
    }

    groups
        .into_iter()
        .map(|(key, accs)| {
            let mut srow: Vec<Value> = Vec::with_capacity(1 + accs.len());
            srow.push(key);
            for a in accs {
                srow.push(a.finalize()?);
            }
            Ok(srow)
        })
        .collect()
}

/// A prepared statement's memoized scan plan (spec/design/api.md §2.4): the resolved [`SelectPlan`]
/// (shared `Rc`, so a cache hit rebuilds the cursor around the SAME plan allocation and re-plans
/// nothing) plus the finalized `$N` param types, stamped with the shared-core identity and each base
/// relation's ordered exact estimator-input tuple (database identity, catalog generation,
/// normalized name, revision). A hit compares every field and therefore also rejects temp shadows
/// and replaced attachments. Filled only for a reusable plan read from committed state
/// ([`Engine::try_scan_query`]). The plan
/// is `!Send` (it holds a regex `Cell`), so a `PreparedStatement` carrying one is `!Send` too — a
/// non-regression, the whole query/cursor path is already thread-affine.
pub(crate) struct CachedPlan {
    // Fields are private to the executor: api.rs / shared.rs only name the type (to hold the
    // `RefCell<Option<CachedPlan>>` cache and thread it), never touch the fields — which keeps the
    // more-private `SelectPlan` out of a pub(crate) field.
    //
    // `core` is a `Weak` so a statement outliving its `Database` does not keep the core's storage
    // alive — and the weak count keeps the allocation address from being reused, so the `ptr_eq`
    // identity check cannot alias a later database (no ABA).
    pub(crate) core: std::sync::Weak<crate::shared::Shared>,
    pub(crate) inputs: Vec<EstimatorInputSignature>,
    pub(crate) plan: std::rc::Rc<SelectPlan>,
    pub(crate) param_types: Vec<ScalarType>,
}

/// One exact relation-scoped estimator-input signature entry (estimator.md §6). The database and
/// revision fields are opaque `Arc` equality tokens, not hashes, so validation is collision-free.
pub(crate) struct EstimatorInputSignature {
    pub(crate) database: std::sync::Arc<EstimatorDatabaseIdentity>,
    pub(crate) cat_gen: u64,
    pub(crate) table: String,
    pub(crate) revision: std::sync::Arc<EstimatorRevision>,
}

/// The lazy pull pipeline behind a streaming [`Rows`](crate::Rows) cursor (spec/design/streaming.md
/// §3/§4, S3): [`exec_streaming_scan`](Engine::exec_streaming_scan)'s per-row loop turned inside out
/// so the CALLER pulls each row. It owns a frozen snapshot [`Engine`] (eval's `exec`, so the cursor
/// is self-contained and outlives the handle — streaming.md §5), a pull B-tree
/// [`StoreScan`](crate::storage::StoreScan) over that snapshot (the scan pin), the resolved + folded
/// plan, bound params, a per-statement entropy cell, and its own cost [`Meter`]. Each
/// [`next_row`](crate::cursor::RowStream::next_row) runs scan → resolve touched columns → `WHERE` →
/// project for ONE output row, accruing the identical cost units at the identical sites as the eager
/// path — so a fully-drained streaming query observes the same rows + total cost (streaming.md §6),
/// while a caller that stops early reads (and charges) less.
pub(crate) struct StreamingScan {
    pub(crate) engine: Engine,
    /// The resolved plan, shared (`Rc`) so a prepared statement's plan cache and this cursor hold the
    /// same allocation — a cache hit rebinds params + rebuilds the cursor but re-plans nothing
    /// (spec/design/api.md §2.4). Read-only during iteration (the fold ran before wrapping).
    pub(crate) plan: std::rc::Rc<SelectPlan>,
    pub(crate) params: Vec<Value>,
    pub(crate) rng: std::cell::Cell<crate::seam::StmtRng>,
    pub(crate) scan: crate::storage::StoreScan,
    pub(crate) meter: Meter,
    pub(crate) offset: i64,
    pub(crate) limit: Option<i64>,
    pub(crate) distinct: bool,
    pub(crate) seen: std::collections::HashSet<Vec<Value>>,
    /// Survivors past the filter+dedup so far (the `OFFSET` runs against this), like
    /// `exec_streaming_scan`'s `passed`.
    pub(crate) passed: i64,
    /// Output rows produced so far (the `LIMIT` short-circuit runs against this).
    pub(crate) produced: i64,
    /// Set once the scan is exhausted, the `LIMIT` window is filled, or the bound is empty —
    /// after which `next_row` short-circuits without faulting another leaf.
    pub(crate) done: bool,
}

impl crate::cursor::RowStream for StreamingScan {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        if self.done {
            return Ok(None);
        }
        // The LIMIT short-circuit: once the window is full, stop WITHOUT pulling another row — so no
        // further leaf is faulted (the streaming early-exit win; cost.md §3 "LIMIT short-circuit").
        if let Some(l) = self.limit
            && self.produced >= l
        {
            self.done = true;
            return Ok(None);
        }
        let env = EvalEnv {
            exec: &self.engine,
            params: &self.params,
            outer: &[],
            rng: &self.rng,
            ctes: CteCtx::empty(),
        };
        let mask = &self.plan.rel_masks[0];
        loop {
            let (_key, mut row) = match self.scan.next()? {
                Some(p) => p,
                None => {
                    self.done = true;
                    return Ok(None);
                }
            };
            self.meter.guard()?; // enforce the cost ceiling / cancellation per scanned row
            self.meter.charge(COSTS.storage_row_read);
            // Materialize the touched columns left unfetched by the lazy load (large-values.md §14);
            // the chain reads were already metered in the up-front block (cost.md §3).
            if TableStore::needs_resolution(&row, mask) {
                self.scan.resolve_columns(&mut row, mask)?;
            }
            let keep = match &self.plan.filter {
                Some(f) => f.eval(&row, &env, &mut self.meter)?.is_true(),
                None => true,
            };
            if !keep {
                continue;
            }
            if self.distinct {
                // DISTINCT (cost.md §3): project EVERY scanned filtered row (the dedup key, charged
                // even for a duplicate — the §3 asymmetry), drop a value already seen, then OFFSET/LIMIT
                // window the survivors — exactly `exec_streaming_scan`.
                let mut projected = Vec::with_capacity(self.plan.projections.len());
                for p in &self.plan.projections {
                    projected.push(p.eval(&row, &env, &mut self.meter)?);
                }
                if !self.seen.insert(projected.clone()) {
                    continue;
                }
                self.passed += 1;
                if self.passed <= self.offset {
                    continue;
                }
                self.meter.charge(COSTS.row_produced);
                self.produced += 1;
                return Ok(Some(projected));
            }
            self.passed += 1;
            if self.passed <= self.offset {
                continue;
            }
            self.meter.charge(COSTS.row_produced);
            let mut projected = Vec::with_capacity(self.plan.projections.len());
            for p in &self.plan.projections {
                projected.push(p.eval(&row, &env, &mut self.meter)?);
            }
            self.produced += 1;
            return Ok(Some(projected));
        }
    }

    fn cost(&self) -> i64 {
        self.meter.accrued
    }

    fn close(&mut self) {
        // The pinned snapshot is owned by `self.engine` / `self.scan` and released on `Drop`; mark
        // done so any further `next_row` is a no-op (streaming.md §5, idempotent).
        self.done = true;
    }
}

/// The lazy **buffered** pull pipeline behind a `query()` [`Rows`](crate::Rows) cursor for a plan with
/// a blocking operator (spec/design/streaming.md §4, S4) — the generalization of `SortedRows::next()`
/// to every blocking shape. It owns a frozen snapshot [`Engine`] (eval's `exec`, so the cursor is
/// self-contained and outlives the handle — streaming.md §5), the resolved + folded plan, bound
/// params, a per-statement entropy cell, its own cost [`Meter`], and the lazy emission `state`. On its
/// FIRST [`next_row`](crate::cursor::RowStream::next_row) it runs the blocking part
/// ([`exec_select_emit`](Engine::exec_select_emit)) to completion into an [`Emitter`] — buffering the
/// input (correctly: a sort/group/dedup/join must see it all) and charging the scan/sort/group/dedup
/// cost — then yields its buffer **one row at a time**: a `Project` row is projected (and charges
/// `row_produced` + projection) on emission, a `Sorted` row is pulled from the [`SortedRows`] iterator
/// and projected (the streaming-sort output, streaming.md §4/§7), an `Identity`/`Final` row is handed
/// out (already projected). So peak *output* memory is one row, a caller's early exit skips the
/// projection of the rows it never pulls, and a fully-drained query observes the same rows + total cost
/// as the eager path (streaming.md §6).
pub(crate) struct BufferedScan {
    pub(crate) engine: Engine,
    /// The resolved plan, shared (`Rc`) with a prepared statement's plan cache (see [`StreamingScan`]).
    pub(crate) plan: std::rc::Rc<SelectPlan>,
    pub(crate) params: Vec<Value>,
    pub(crate) rng: std::cell::Cell<crate::seam::StmtRng>,
    pub(crate) meter: Meter,
    pub(crate) state: BufState,
}

/// The lazy emission state of a [`BufferedScan`] (spec/design/streaming.md §4).
pub(crate) enum BufState {
    /// The blocking part has not run yet — the first `next_row` runs it (streaming.md §4).
    Pending,
    /// The general blocking buffer, windowed to `[idx, end)`. Each emission charges `row_produced`;
    /// `project` rows additionally evaluate the projection list (`Identity` rows are pre-projected).
    Buffer {
        rows: Vec<Vec<Value>>,
        idx: usize,
        end: usize,
        project: bool,
    },
    /// A fully-formed result from a special input-streaming path (already projected AND charged) —
    /// emission just hands the rows out.
    Final {
        iter: std::vec::IntoIter<Vec<Value>>,
    },
    /// The streaming sort's lazy output: the [`SortedRows`] pull iterator (positioned past the
    /// `OFFSET`) and `remaining` windowed rows still to emit. Each `next_row` pulls the next sorted
    /// row, charges `row_produced`, and projects it — so the output `Vec` is never built and an early
    /// exit skips the rows it never pulls (streaming.md §4/§7).
    Sorted {
        sorted: crate::spill::SortedRows,
        remaining: usize,
    },
    /// The columnar projection fast path's lazy state (packed-leaf.md §11 Track A2/A3): the pre-gathered
    /// dense lanes + the projection's column indices, windowed to `[idx, end)`, with the optional A3
    /// selection vector. Each emission gathers output row `j` from the lanes at lane position `sel[j]`
    /// (or `j`) and charges `row_produced` — an early exit skips the rows it never pulls.
    Columnar {
        cols: Vec<Vec<Value>>,
        proj_cols: Vec<usize>,
        sel: Option<Vec<i32>>,
        idx: usize,
        end: usize,
    },
    /// The buffer is exhausted (or the cursor was closed) — every further `next_row` is `None`.
    Done,
}

impl crate::cursor::RowStream for BufferedScan {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        // Run the blocking part on the FIRST pull (streaming.md §4 — `Buffered` runs the blocking part
        // then yields its buffer lazily). A mid-blocking cost abort / cancellation / trap surfaces HERE
        // (during iteration), not at `query()` time (streaming.md §6). Disjoint-field borrows: the
        // emit reads `self.engine`/`self.plan`/`self.params`/`self.rng` and writes `self.meter`, all
        // distinct from `self.state` it then assigns.
        if matches!(self.state, BufState::Pending) {
            let emitter = self.engine.exec_select_emit(
                self.plan.as_ref(),
                &[],
                &self.params,
                CteCtx::empty(),
                &self.rng,
                &mut self.meter,
            )?;
            self.state = match emitter {
                Emitter::Buffer {
                    rows,
                    start,
                    end,
                    mode,
                } => BufState::Buffer {
                    rows,
                    idx: start,
                    end,
                    project: matches!(mode, EmitMode::Project),
                },
                Emitter::Final { rows } => BufState::Final {
                    iter: rows.into_iter(),
                },
                Emitter::Sorted { sorted, remaining } => BufState::Sorted { sorted, remaining },
                Emitter::Columnar {
                    cols,
                    proj_cols,
                    sel,
                    start,
                    end,
                } => BufState::Columnar {
                    cols,
                    proj_cols,
                    sel,
                    idx: start,
                    end,
                },
            };
        }
        match &mut self.state {
            BufState::Done => Ok(None),
            BufState::Pending => unreachable!("the blocking part ran above"),
            // Already projected + charged — hand the next row out (no further cost).
            BufState::Final { iter } => Ok(iter.next()),
            // The streaming sort's lazy output: pull the next windowed row, charge `row_produced`,
            // and project it (streaming.md §4/§7). Disjoint-field borrows: `sorted`/`remaining` come
            // from `self.state`, distinct from `self.meter`/`self.engine`/`self.plan`/`self.rng`/
            // `self.params` the projection reads.
            BufState::Sorted { sorted, remaining } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                let row = sorted
                    .next()?
                    .expect("the sorter yields exactly the windowed rows");
                *remaining -= 1;
                self.meter.guard()?; // enforce the cost ceiling / cancellation per produced row
                self.meter.charge(COSTS.row_produced);
                let env = EvalEnv {
                    exec: &self.engine,
                    params: &self.params,
                    outer: &[],
                    rng: &self.rng,
                    ctes: CteCtx::empty(),
                };
                let mut out = Vec::with_capacity(self.plan.projections.len());
                for p in &self.plan.projections {
                    out.push(p.eval(&row, &env, &mut self.meter)?);
                }
                Ok(Some(out))
            }
            BufState::Buffer {
                rows,
                idx,
                end,
                project,
            } => {
                if *idx >= *end {
                    return Ok(None);
                }
                let i = *idx;
                *idx += 1;
                let project = *project;
                self.meter.guard()?; // enforce the cost ceiling / cancellation per produced row
                self.meter.charge(COSTS.row_produced);
                if project {
                    let env = EvalEnv {
                        exec: &self.engine,
                        params: &self.params,
                        outer: &[],
                        rng: &self.rng,
                        ctes: CteCtx::empty(),
                    };
                    let mut out = Vec::with_capacity(self.plan.projections.len());
                    for p in &self.plan.projections {
                        out.push(p.eval(&rows[i], &env, &mut self.meter)?);
                    }
                    Ok(Some(out))
                } else {
                    Ok(Some(std::mem::take(&mut rows[i])))
                }
            }
            // Columnar projection (packed-leaf.md §11 Track A2/A3): gather this row from the dense lanes —
            // a bare-column projection with no full-width row — charging only row_produced (a bare column
            // ref is a zero-cost slot read). A non-None `sel` (the A3 filter's survivors) maps output row
            // j to lane position sel[j].
            BufState::Columnar {
                cols,
                proj_cols,
                sel,
                idx,
                end,
            } => {
                if *idx >= *end {
                    return Ok(None);
                }
                let j = *idx;
                *idx += 1;
                self.meter.guard()?; // enforce the cost ceiling / cancellation per produced row
                self.meter.charge(COSTS.row_produced);
                let l = match sel {
                    Some(s) => s[j] as usize,
                    None => j,
                };
                let mut out = Vec::with_capacity(proj_cols.len());
                for &c in proj_cols.iter() {
                    out.push(cols[c][l].clone());
                }
                Ok(Some(out))
            }
        }
    }

    fn cost(&self) -> i64 {
        self.meter.accrued
    }

    fn close(&mut self) {
        // The pinned snapshot is owned by `self.engine` and released on `Drop`; mark done so any
        // further `next_row` is a no-op (streaming.md §5, idempotent).
        self.state = BufState::Done;
    }
}

/// A top-level set operation / pure-query `WITH` deferred to a lazy cursor (spec/design/streaming.md
/// §4/§7). Its output is already projected + charged, so there is no per-row projection to defer — the
/// cursor's only job is to run the whole query on the FIRST pull and yield the result one row at a
/// time. Owned by a [`DeferredResult`]; run via the eager `run_set_op` / `run_with` verbatim so the
/// rows + cost match `execute()` exactly (§6).
pub(crate) enum DeferredQuery {
    SetOp(SetOp),
    With(WithQuery),
}

/// The lazy **deferred** pull pipeline behind a `query()` [`Rows`](crate::Rows) cursor for a top-level
/// set operation / pure-query `WITH` (spec/design/streaming.md §7). It owns a frozen snapshot
/// [`Engine`] (§5), the owned query AST, and the bound params; on its FIRST
/// [`next_row`](crate::cursor::RowStream::next_row) it runs the whole `run_set_op` / `run_with` to
/// completion (so a cost abort / cancellation / trap surfaces *during iteration*, not at `query()` —
/// §6), records the accrued cost, and yields the materialized result **one row at a time**. The input
/// is still buffered (a set op dedups / a `WITH` materializes — it must), so the win here is only
/// lazy-yield: the work is deferred to the first pull and the result rows are handed out incrementally
/// rather than wrapped in an eager `Outcome`. Under full drain the rows + total cost are byte-identical
/// to the eager path (it drives the SAME `run_*`, §6).
pub(crate) struct DeferredResult {
    pub(crate) engine: Engine,
    /// The query to run, taken on the first pull (`None` afterwards).
    pub(crate) query: Option<DeferredQuery>,
    pub(crate) params: Vec<Value>,
    pub(crate) state: DeferredState,
    /// The accrued cost — 0 until the first pull runs the query, then `SelectResult::cost` (final).
    pub(crate) cost: i64,
}

/// The lazy emission state of a [`DeferredResult`] (spec/design/streaming.md §7).
pub(crate) enum DeferredState {
    /// The query has not run yet — the first `next_row` runs it (streaming.md §7).
    Pending,
    /// The materialized result, walked one row at a time.
    Yielding(std::vec::IntoIter<Vec<Value>>),
    /// Exhausted (or the cursor was closed) — every further `next_row` is `None`.
    Done,
}

impl crate::cursor::RowStream for DeferredResult {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        // Run the whole set op / WITH on the FIRST pull (streaming.md §7), reusing the eager
        // `run_set_op` / `run_with` verbatim so the rows + cost match `execute()` exactly. A mid-run
        // cost abort / cancellation / arithmetic trap surfaces HERE (during iteration), not at
        // `query()` (streaming.md §6). `query.take()` releases its borrow before the `&self.engine`
        // run, so the later `self.cost`/`self.state` writes do not alias.
        if let Some(query) = self.query.take() {
            let r = match query {
                DeferredQuery::SetOp(so) => self.engine.run_set_op(so, &self.params)?,
                DeferredQuery::With(wq) => self.engine.run_with(wq, &self.params)?,
            };
            self.cost = r.cost;
            self.state = DeferredState::Yielding(r.rows.into_iter());
        }
        match &mut self.state {
            DeferredState::Yielding(iter) => Ok(iter.next()),
            DeferredState::Pending | DeferredState::Done => Ok(None),
        }
    }

    fn cost(&self) -> i64 {
        self.cost
    }

    fn close(&mut self) {
        // The frozen snapshot is owned by `self.engine` and released on `Drop`; drop any pending query
        // + unread rows so a further `next_row` is a no-op (streaming.md §5, idempotent).
        self.query = None;
        self.state = DeferredState::Done;
    }
}

/// Build the constant `RExpr` for a folded uncorrelated-subquery value (§26). The static type
/// was settled at resolve, so a NULL value here is just `ConstNull`.
pub(crate) fn value_to_rexpr(v: &Value) -> RExpr {
    match v {
        Value::Null => RExpr::ConstNull,
        Value::Int(n) => RExpr::ConstInt(*n),
        Value::Bool(b) => RExpr::ConstBool(*b),
        Value::Text(s) => RExpr::ConstText(s.clone()),
        Value::Decimal(d) => RExpr::ConstDecimal(d.clone()),
        Value::Float32(f) => RExpr::ConstFloat32(*f),
        Value::Float64(f) => RExpr::ConstFloat64(*f),
        Value::Bytea(b) => RExpr::ConstBytea(b.clone()),
        Value::Uuid(u) => RExpr::ConstUuid(*u),
        Value::Timestamp(m) => RExpr::ConstTimestamp(*m),
        Value::Timestamptz(m) => RExpr::ConstTimestamptz(*m),
        Value::Date(d) => RExpr::ConstDate(*d),
        Value::Interval(iv) => RExpr::ConstInterval(*iv),
        // A folded composite constant: fold each field and wrap in a ROW node so eval rebuilds the
        // `Value::Composite` (spec/design/composite.md).
        Value::Composite(fields) => RExpr::Row(fields.iter().map(value_to_rexpr).collect()),
        // A folded array constant — preserve its full shape (dims/lbounds) in a const node.
        Value::Array(arr) => RExpr::ConstArray(Box::new(arr.clone())),
        Value::Range(r) => RExpr::ConstRange(Box::new(r.clone())),
        Value::Json(s) => RExpr::ConstJson(s.clone()),
        Value::JsonPath(s) => RExpr::ConstJsonPath(s.clone()),
        Value::Jsonb(n) => RExpr::ConstJsonb(Box::new(n.clone())),
        // Poisoned (large-values.md §14): a folded subquery's projections are resolved values.
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    }
}

/// Whether a resolved plan references any scope STRICTLY OUTSIDE itself — i.e. it is correlated
/// (spec/design/grammar.md §26). `depth` is how many nested-subquery frames we have descended
/// INTO this plan (0 = the plan's own clauses); an `OuterColumn { level }` points above this
/// plan iff `level > depth`. The `fold_uncorrelated` pass calls this with `depth = 0` on a
/// subquery's sub-plan to decide whether to fold it (uncorrelated) or leave it (correlated).
pub(crate) fn query_plan_references_outer(plan: &QueryPlan, depth: usize) -> bool {
    match plan {
        QueryPlan::Select(sp) => select_plan_references_outer(sp, depth),
        QueryPlan::SetOp(sop) => {
            query_plan_references_outer(&sop.lhs, depth)
                || query_plan_references_outer(&sop.rhs, depth)
        }
        // A VALUES body is planned `parent = None`, so its values hold no outer reference of their
        // own; a folded-in subquery, however, may correlate to the target scope.
        QueryPlan::Values(vp) => vp
            .rows
            .iter()
            .flatten()
            .any(|e| rexpr_references_outer(e, depth)),
        // A nested `WITH` adds no correlation frame: its body is at the same depth, and the CTE
        // bodies are planned `parent = None` (they hold no outer reference), so only the body can
        // correlate to an enclosing scope (spec/design/cte.md §7).
        QueryPlan::With(wp) => query_plan_references_outer(&wp.body, depth),
    }
}

pub(crate) fn select_plan_references_outer(sp: &SelectPlan, depth: usize) -> bool {
    sp.joins.iter().any(|j| {
        j.on.as_ref()
            .is_some_and(|on| rexpr_references_outer(on, depth))
    }) || sp
        .filter
        .as_ref()
        .is_some_and(|f| rexpr_references_outer(f, depth))
        || sp
            .having
            .as_ref()
            .is_some_and(|h| rexpr_references_outer(h, depth))
        || sp.agg_specs.iter().any(|s| {
            s.operand
                .as_ref()
                .is_some_and(|op| rexpr_references_outer(op, depth))
        })
        || sp
            .projections
            .iter()
            .any(|p| rexpr_references_outer(p, depth))
        // A materialized ORDER BY expression may itself carry a correlated reference
        // (query.order_by_correlated): a subquery whose ONLY outer reference is in its ORDER BY is
        // still correlated and must re-execute per outer row (else its OuterColumn reads an empty
        // outer-row environment).
        || sp
            .order_exprs
            .iter()
            .any(|oe| rexpr_references_outer(oe, depth))
        // A set-returning relation's arguments may carry a correlated reference (an implicitly-
        // lateral SRF arg sees params / outer / an earlier sibling — functions.md §10, grammar.md
        // §44), which makes the enclosing query correlated, so it must NOT be folded once.
        || sp.rels.iter().any(|r| {
            r.srf
                .as_ref()
                .is_some_and(|srf| srf.args.iter().any(|a| rexpr_references_outer(a, depth)))
        })
        // A LATERAL derived table's body is one frame deeper; a reference in it back into this
        // query's outer (e.g. a nested lateral reaching a grandparent relation) counts here so the
        // enclosing item is correctly flagged correlated (spec/design/grammar.md §44).
        || sp
            .rels
            .iter()
            .any(|r| r.derived.as_ref().is_some_and(|d| query_plan_references_outer(d, depth + 1)))
}

pub(crate) fn rexpr_references_outer(e: &RExpr, depth: usize) -> bool {
    match e {
        RExpr::OuterColumn { level, .. } => *level > depth,
        // A nested subquery's own clauses are one frame deeper; its IN lhs is at this frame.
        RExpr::Subquery { plan, lhs, .. } => {
            lhs.as_ref()
                .is_some_and(|l| rexpr_references_outer(l, depth))
                || query_plan_references_outer(plan, depth + 1)
        }
        RExpr::InValues { lhs, .. } => rexpr_references_outer(lhs, depth),
        RExpr::Quantified { lhs, array, .. } => {
            rexpr_references_outer(lhs, depth) || rexpr_references_outer(array, depth)
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => {
            rexpr_references_outer(inner, depth)
        }
        RExpr::Neg { operand, .. } => rexpr_references_outer(operand, depth),
        RExpr::Not(x) => rexpr_references_outer(x, depth),
        RExpr::Casing { arg, .. } => rexpr_references_outer(arg, depth),
        RExpr::AtTimeZone { zone, value, .. } => {
            rexpr_references_outer(zone, depth) || rexpr_references_outer(value, depth)
        }
        RExpr::DateTrunc { unit, value, zone } => {
            rexpr_references_outer(unit, depth)
                || rexpr_references_outer(value, depth)
                || zone
                    .as_ref()
                    .is_some_and(|z| rexpr_references_outer(z, depth))
        }
        RExpr::Extract { value, .. } => rexpr_references_outer(value, depth),
        RExpr::DateConvert { inner, .. } => rexpr_references_outer(inner, depth),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. } => {
            rexpr_references_outer(lhs, depth) || rexpr_references_outer(rhs, depth)
        }
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => {
            rexpr_references_outer(base, depth) || rexpr_references_outer(arg, depth)
        }
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            rexpr_references_outer(a, depth) || rexpr_references_outer(b, depth)
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            rexpr_references_outer(l, depth) || rexpr_references_outer(r, depth)
        }
        RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => rexpr_references_outer(operand, depth),
        RExpr::Case { arms, els, .. } => {
            arms.iter()
                .any(|(c, r)| rexpr_references_outer(c, depth) || rexpr_references_outer(r, depth))
                || rexpr_references_outer(els, depth)
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
        | RExpr::JsonPathFn { args, .. } => args.iter().any(|a| rexpr_references_outer(a, depth)),
        RExpr::JsonSqlFn { ctx, path, .. } => {
            rexpr_references_outer(ctx, depth) || rexpr_references_outer(path, depth)
        }
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            fields.iter().any(|f| rexpr_references_outer(f, depth))
        }
        RExpr::Field { base, .. } => rexpr_references_outer(base, depth),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            rexpr_references_outer(base, depth)
                || subscripts
                    .iter()
                    .flat_map(subscript_bounds)
                    .any(|e| rexpr_references_outer(e, depth))
        }
        RExpr::Column(_)
        | RExpr::Param(_)
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        // A DateClock leaf references no outer column.
        | RExpr::DateClock { .. }
        | RExpr::ConstNull => false,
    }
}

/// The bound expressions of one resolved subscript spec (an index, or a slice's present
/// lower/upper bounds) — for the RExpr tree walkers.
pub(crate) fn subscript_bounds(s: &RSubscript) -> Vec<&RExpr> {
    match s {
        RSubscript::Index(i) => vec![i],
        RSubscript::Slice { lower, upper } => lower
            .iter()
            .chain(upper.iter())
            .map(|b| b.as_ref())
            .collect(),
    }
}

/// Collect the combined-row columns an expression **statically references** — the touched set
/// (cost.md §3 "The touched set"; large-values.md §14). Depth bookkeeping mirrors
/// `rexpr_references_outer`: walking the target plan's own clauses is depth 0 (a `Column`
/// touches); inside a nested subquery a `Column` indexes the subquery's own row (ignored) and an
/// `OuterColumn { level == depth }` is a correlated reference back into the target scope
/// (touches). Purely syntactic — a never-taken CASE branch still touches — so the set is
/// deterministic and cross-core identical (a §8 contract).
pub(crate) fn collect_touched(e: &RExpr, depth: usize, touched: &mut [bool]) {
    match e {
        RExpr::Column(i) => {
            // A `Column` index beyond the real columns is a SYNTHETIC slot (an aggregate or window
            // result, spec/design/window.md §5.1), not a table column — it touches no stored data,
            // so the bound check skips it rather than panicking.
            if depth == 0 && *i < touched.len() {
                touched[*i] = true;
            }
        }
        RExpr::OuterColumn { level, index } => {
            // A correlated reference into the scope we are collecting for (its frame is `depth` levels
            // up). The index is a slot in that target scope's combined row; bounds-checked like the
            // Column case. Callers collect at the depth matching the reference's level — a correlated
            // subquery at its nesting depth, a LATERAL SRF arg at depth 1 (its sibling frame).
            if *level == depth && depth > 0 && *index < touched.len() {
                touched[*index] = true;
            }
        }
        RExpr::Subquery { plan, lhs, .. } => {
            if let Some(l) = lhs {
                collect_touched(l, depth, touched);
            }
            collect_touched_plan(plan, depth + 1, touched);
        }
        RExpr::InValues { lhs, .. } => collect_touched(lhs, depth, touched),
        RExpr::Quantified { lhs, array, .. } => {
            collect_touched(lhs, depth, touched);
            collect_touched(array, depth, touched);
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => {
            collect_touched(inner, depth, touched)
        }
        RExpr::Neg { operand, .. } => collect_touched(operand, depth, touched),
        RExpr::Not(x) => collect_touched(x, depth, touched),
        RExpr::Casing { arg, .. } => collect_touched(arg, depth, touched),
        RExpr::AtTimeZone { zone, value, .. } => {
            collect_touched(zone, depth, touched);
            collect_touched(value, depth, touched);
        }
        RExpr::DateTrunc { unit, value, zone } => {
            collect_touched(unit, depth, touched);
            collect_touched(value, depth, touched);
            if let Some(z) = zone {
                collect_touched(z, depth, touched);
            }
        }
        RExpr::Extract { value, .. } => collect_touched(value, depth, touched),
        RExpr::DateConvert { inner, .. } => collect_touched(inner, depth, touched),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. } => {
            collect_touched(lhs, depth, touched);
            collect_touched(rhs, depth, touched);
        }
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => {
            collect_touched(base, depth, touched);
            collect_touched(arg, depth, touched);
        }
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            collect_touched(a, depth, touched);
            collect_touched(b, depth, touched);
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            collect_touched(l, depth, touched);
            collect_touched(r, depth, touched);
        }
        RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => collect_touched(operand, depth, touched),
        RExpr::Case { arms, els, .. } => {
            for (c, r) in arms {
                collect_touched(c, depth, touched);
                collect_touched(r, depth, touched);
            }
            collect_touched(els, depth, touched);
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
                collect_touched(a, depth, touched);
            }
        }
        RExpr::JsonSqlFn { ctx, path, .. } => {
            collect_touched(ctx, depth, touched);
            collect_touched(path, depth, touched);
        }
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            for f in fields {
                collect_touched(f, depth, touched);
            }
        }
        RExpr::Field { base, .. } => collect_touched(base, depth, touched),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            collect_touched(base, depth, touched);
            for e in subscripts.iter().flat_map(subscript_bounds) {
                collect_touched(e, depth, touched);
            }
        }
        RExpr::Param(_)
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        // A DateClock leaf touches no stored column.
        | RExpr::DateClock { .. }
        | RExpr::ConstNull => {}
    }
}

/// The number of grouping sets a single GROUP BY term expands to, saturating well below `usize::MAX`
/// so a huge `CUBE` cannot overflow the product before the `MAX_GROUPING_SETS` limit check.
pub(crate) fn group_item_set_count(item: &GroupItem) -> usize {
    match item {
        GroupItem::Set(_) => 1,
        GroupItem::Rollup(groups) => groups.len() + 1,
        // CUBE of n column groups is 2ⁿ; clamp the exponent so the shift can't overflow.
        GroupItem::Cube(groups) => {
            if groups.len() >= 20 {
                usize::MAX >> 1
            } else {
                1usize << groups.len()
            }
        }
        GroupItem::GroupingSets(elems) => elems
            .iter()
            .map(group_item_set_count)
            .fold(0usize, |a, c| a.saturating_add(c)),
    }
}

/// Expand a single GROUP BY term into its list of grouping sets, each a list of column `Expr`s
/// (`ROLLUP`/`CUBE`/`GROUPING SETS` and nesting — spec/design/aggregates.md §12). The per-set column
/// order is the textual order; the set order is deterministic and identical across cores (tests
/// compare the row multiset with `rowsort`).
pub(crate) fn expand_group_item(item: &GroupItem) -> Vec<Vec<&Expr>> {
    match item {
        GroupItem::Set(cols) => vec![cols.iter().collect()],
        // ROLLUP(g1..gn): the prefixes longest-first down to the empty set — n+1 sets.
        GroupItem::Rollup(groups) => (0..=groups.len())
            .rev()
            .map(|k| groups[..k].iter().flatten().collect())
            .collect(),
        // CUBE(g1..gn): every subset of the column groups — 2ⁿ sets (bit i = include group i).
        GroupItem::Cube(groups) => (0..(1usize << groups.len()))
            .map(|mask| {
                let mut s: Vec<&Expr> = Vec::new();
                for (i, g) in groups.iter().enumerate() {
                    if mask & (1usize << i) != 0 {
                        s.extend(g.iter());
                    }
                }
                s
            })
            .collect(),
        // GROUPING SETS(e1..en): the concatenation of each element's expansion.
        GroupItem::GroupingSets(elems) => elems.iter().flat_map(expand_group_item).collect(),
    }
}

/// Expand a whole GROUP BY clause into its grouping sets: the cross-product of the top-level terms'
/// expansions (`GROUP BY a, ROLLUP(b,c)` → `{(a,b,c),(a,b),(a)}`). An empty clause yields one empty
/// set (the whole-table grand total). Aborts `54001` if the expansion exceeds `MAX_GROUPING_SETS`.
pub(crate) fn expand_group_by(items: &[GroupItem]) -> Result<Vec<Vec<&Expr>>> {
    let mut total: usize = 1;
    for it in items {
        total = total.saturating_mul(group_item_set_count(it));
    }
    if total > MAX_GROUPING_SETS {
        return Err(EngineError::new(
            SqlState::StatementTooComplex,
            format!("too many grouping sets (the limit is {MAX_GROUPING_SETS})"),
        ));
    }
    let mut acc: Vec<Vec<&Expr>> = vec![Vec::new()];
    for it in items {
        let exp = expand_group_item(it);
        let mut next: Vec<Vec<&Expr>> = Vec::with_capacity(acc.len() * exp.len().max(1));
        for a in &acc {
            for s in &exp {
                let mut combined = a.clone();
                combined.extend(s.iter().copied());
                next.push(combined);
            }
        }
        acc = next;
    }
    Ok(acc)
}

/// The resolution of one `GROUP BY` grouping term (aggregates.md §15): either an input COLUMN at a
/// flat row index, or a general EXPRESSION to materialize (its resolved node + type + canonical AST).
pub(crate) enum GroupKeyResolved {
    Column(usize),
    Expr(RExpr, ResolvedType, Expr),
}
