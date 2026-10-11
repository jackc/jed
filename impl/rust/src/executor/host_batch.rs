//! Host-function batch prefetch (spec/design/extensibility.md §4.2.1) — speculative batch, scalar
//! replay. An evaluation site that holds a chunk of rows asks [`HostBatch::set_row`] to call each
//! eligible host function ONCE over the chunk's argument columns, then replays its ordinary
//! row-at-a-time evaluation with the batch installed on the [`EvalEnv`]. The `HostFunc` eval arm still
//! charges, guards, and type-checks per row exactly as before, and evaluates its arguments and
//! short-circuits NULL for any row the batch cannot answer; for a prefetched row it takes the kernel's
//! outcome from the cache, skipping the argument evaluation that would charge nothing, raise nothing,
//! and not short-circuit. So rows, cost, abort points, and errors are identical to batch-of-one by
//! construction.
//!
//! A site that holds its rows (the `Buffer` emitter) prefetches from them directly. A site that PULLS
//! its rows (the streaming scan, the streaming sort / spool) first fills a [`ReadAhead`] window from
//! its source — pulling is not a metered event there, and a source error is deferred to the row where
//! the batch-of-one pull would have raised it (§4.2.1 "In-hand and pulled sites").

use super::*;
use std::cell::{Cell, RefCell};

/// The largest chunk a site prefetches (extensibility.md §4.2.1).
pub(crate) const HOST_BATCH_ROWS: usize = 1024;

/// The largest read-ahead window a pulled site fills (extensibility.md §4.2.1): every pulled row is a
/// fresh allocation the window keeps alive only to batch, so the window stays small — still 64× fewer
/// kernel crossings.
pub(crate) const READ_AHEAD_ROWS: usize = 64;

/// One eligible call node's prefetched outcomes over the site's current chunk.
struct Slot {
    /// The call node's identity (its address in the plan the site evaluates) — compared, never
    /// dereferenced. A node outside this site's expression list (a subquery's) never matches.
    node: usize,
    /// The function's registry id and a copy of its (trivially evaluable) argument nodes, so a
    /// prefetch can evaluate them without reaching back into the plan.
    id: usize,
    args: Vec<RExpr>,
    /// The site row index of `outcomes[0]`.
    base: Cell<usize>,
    /// How many chunk rows the prefetch settled: the whole chunk, or through the row the kernel
    /// reported failing. A row at or past `base + valid` starts a new chunk.
    valid: Cell<usize>,
    /// Per chunk row: the kernel's result, or `None` (not prefetched — a NULL / unfetched argument,
    /// the failing row, or a row past it). Taken (moved out) on replay.
    values: RefCell<Vec<Option<Value>>>,
    /// The chunk row the kernel reported failing, with its error. Taken on replay.
    error: RefCell<Option<(usize, EngineError)>>,
}

/// The prefetched host-function outcomes of one evaluation site (extensibility.md §4.2.1).
pub(crate) struct HostBatch {
    /// The site's current row index, set by [`HostBatch::set_row`] before each row's evaluation.
    row: Cell<usize>,
    slots: Vec<Slot>,
}

/// Whether `e` is free and cannot raise, so a prefetch may evaluate it without a meter: a column or
/// outer-column reference, a parameter, or a scalar constant.
fn trivially_evaluable(e: &RExpr) -> bool {
    matches!(
        e,
        RExpr::Column(_)
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
            | RExpr::ConstJsonb(_)
            | RExpr::ConstJsonPath(_)
            | RExpr::ConstTimestamp(_)
            | RExpr::ConstTimestamptz(_)
            | RExpr::ConstDate(_)
            | RExpr::ConstInterval(_)
            | RExpr::ConstNull
    )
}

/// A copy of a trivially evaluable node (`RExpr` is not `Clone`; these leaves are).
fn clone_trivial(e: &RExpr) -> RExpr {
    match e {
        RExpr::Column(i) => RExpr::Column(*i),
        RExpr::OuterColumn { level, index } => RExpr::OuterColumn {
            level: *level,
            index: *index,
        },
        RExpr::Param(i) => RExpr::Param(*i),
        RExpr::ConstInt(v) => RExpr::ConstInt(*v),
        RExpr::ConstBool(v) => RExpr::ConstBool(*v),
        RExpr::ConstText(v) => RExpr::ConstText(v.clone()),
        RExpr::ConstDecimal(v) => RExpr::ConstDecimal(v.clone()),
        RExpr::ConstFloat32(v) => RExpr::ConstFloat32(*v),
        RExpr::ConstFloat64(v) => RExpr::ConstFloat64(*v),
        RExpr::ConstBytea(v) => RExpr::ConstBytea(v.clone()),
        RExpr::ConstUuid(v) => RExpr::ConstUuid(*v),
        RExpr::ConstJson(v) => RExpr::ConstJson(v.clone()),
        RExpr::ConstJsonb(v) => RExpr::ConstJsonb(v.clone()),
        RExpr::ConstJsonPath(v) => RExpr::ConstJsonPath(v.clone()),
        RExpr::ConstTimestamp(v) => RExpr::ConstTimestamp(*v),
        RExpr::ConstTimestamptz(v) => RExpr::ConstTimestamptz(*v),
        RExpr::ConstDate(v) => RExpr::ConstDate(*v),
        RExpr::ConstInterval(v) => RExpr::ConstInterval(*v),
        RExpr::ConstNull => RExpr::ConstNull,
        _ => unreachable!("only trivially evaluable arguments are copied"),
    }
}

/// Whether a column argument's stored value is still unfetched (a deferred large value,
/// large-values.md §14): resolving it is engine I/O that can raise, so the prefetch leaves that row
/// to replay (extensibility.md §4.2.1 rule 2).
fn unfetched(a: &RExpr, row: &Row, env: &EvalEnv) -> bool {
    match a {
        RExpr::Column(i) => matches!(row[*i], Value::Unfetched(_)),
        RExpr::OuterColumn { level, index } => {
            matches!(
                env.outer[env.outer.len() - level][*index],
                Value::Unfetched(_)
            )
        }
        _ => false,
    }
}

/// Collect the eligible call nodes reachable from `e` (extensibility.md §4.2.1): a batch-kernel,
/// non-`volatile` host call with at least one argument, all trivially evaluable, reached only through
/// nodes that evaluate every operand (a cast, unary minus, NOT, IS [NOT] NULL, arithmetic, comparison).
fn collect(e: &RExpr, ext: &crate::extension::ExtensionRegistry, out: &mut Vec<Slot>) {
    match e {
        RExpr::HostFunc { id, args, .. } => {
            // A zero-argument call is never prefetched: its kernel receives no columns, so it could
            // not learn the batch's row count.
            if ext.function(*id).batchable()
                && !args.is_empty()
                && args.iter().all(trivially_evaluable)
            {
                out.push(Slot {
                    node: e as *const RExpr as usize,
                    id: *id,
                    args: args.iter().map(clone_trivial).collect(),
                    base: Cell::new(0),
                    valid: Cell::new(0),
                    values: RefCell::new(Vec::new()),
                    error: RefCell::new(None),
                });
            }
        }
        RExpr::Cast { inner, .. } => collect(inner, ext, out),
        RExpr::Neg { operand, .. } | RExpr::IsNull { operand, .. } => collect(operand, ext, out),
        RExpr::Not(operand) => collect(operand, ext, out),
        RExpr::Arith { lhs, rhs, .. } | RExpr::Compare { lhs, rhs, .. } => {
            collect(lhs, ext, out);
            collect(rhs, ext, out);
        }
        _ => {}
    }
}

impl HostBatch {
    /// The batch for a site that evaluates every expression of `exprs` once per row, or `None` when
    /// no call in them is eligible (§4.2.1).
    pub(crate) fn for_exprs(
        exprs: &[RExpr],
        ext: &crate::extension::ExtensionRegistry,
    ) -> Option<Self> {
        let mut slots = Vec::new();
        for e in exprs {
            collect(e, ext, &mut slots);
        }
        (!slots.is_empty()).then(|| HostBatch {
            row: Cell::new(0),
            slots,
        })
    }

    /// Make row `i` of the site current. When a slot's chunk does not settle it, prefetch a new chunk
    /// starting at `i` over `rows[i..end]` (at most [`HOST_BATCH_ROWS`] rows, further capped by the
    /// meter's headroom).
    pub(crate) fn set_row(&self, i: usize, rows: &[Row], end: usize, env: &EvalEnv, meter: &Meter) {
        self.row.set(i);
        for slot in &self.slots {
            let base = slot.base.get();
            if i < base || i >= base + slot.valid.get() {
                prefetch(slot, i, rows, end, env, meter.headroom());
            }
        }
    }

    /// Forget every chunk — the site's row indexes restart (a new read-ahead window).
    fn reset(&self) {
        for slot in &self.slots {
            slot.valid.set(0);
            slot.values.borrow_mut().clear();
            slot.error.replace(None);
        }
    }

    /// The prefetched outcome of call node `node` for the current row, or `None` when the batch
    /// cannot answer it (the caller then calls the kernel for this row alone).
    pub(crate) fn take(&self, node: &RExpr) -> Option<Result<Value>> {
        let addr = node as *const RExpr as usize;
        let slot = self.slots.iter().find(|s| s.node == addr)?;
        let i = self.row.get().checked_sub(slot.base.get())?;
        if let Some(v) = slot.values.borrow_mut().get_mut(i)?.take() {
            return Some(Ok(v));
        }
        let mut error = slot.error.borrow_mut();
        match &*error {
            Some((row, _)) if *row == i => error.take().map(|(_, e)| Err(e)),
            _ => None,
        }
    }
}

/// Prefetch one slot's chunk starting at site row `start`.
fn prefetch(
    slot: &Slot,
    start: usize,
    rows: &[Row],
    end: usize,
    env: &EvalEnv,
    headroom: Option<i64>,
) {
    let hf = env.exec.session.extensions.function(slot.id);
    let mut n = (end - start).min(HOST_BATCH_ROWS);
    // Every replayed row charges at least the declared cost, so the kernel never runs on more rows
    // than the remaining budget could pay for, plus the one that trips it (§4.2.1).
    if let Some(h) = headroom
        && hf.cost > 0
    {
        n = n.min((h.max(0) / hf.cost) as usize + 1);
    }
    slot.base.set(start);
    slot.valid.set(n);
    slot.error.replace(None);
    let mut values = slot.values.borrow_mut();
    values.clear();
    values.resize_with(n, || None);
    // Gather the argument columns over the chunk rows whose arguments are all non-NULL and fetched,
    // evaluating straight into the columns (a rejected row's partial values are truncated away);
    // `members` maps each batch row back to its chunk row.
    let mut cols: Vec<Vec<Value>> = (0..slot.args.len())
        .map(|_| Vec::with_capacity(n))
        .collect();
    let mut members = Vec::with_capacity(n);
    let mut scratch = Meter::with_limit(0);
    'rows: for (k, row) in rows[start..start + n].iter().enumerate() {
        let len = members.len();
        for (j, a) in slot.args.iter().enumerate() {
            let v = if unfetched(a, row, env) {
                None
            } else {
                match a.eval(row, env, &mut scratch) {
                    Ok(Value::Null) | Err(_) => None,
                    Ok(v) => Some(v),
                }
            };
            match v {
                Some(v) => cols[j].push(v),
                None => {
                    for col in &mut cols[..j] {
                        col.truncate(len);
                    }
                    continue 'rows;
                }
            }
        }
        members.push(k);
    }
    if members.is_empty() {
        return;
    }
    let (results, error) = hf.call_batch_prefix(&cols, members.len());
    let answered = results.len();
    for (k, v) in members.iter().zip(results) {
        values[*k] = Some(v);
    }
    if let Some(e) = error {
        // The chunk settles through the failing row; a later row starts a new chunk.
        let failed = members[answered];
        slot.error.replace(Some((failed, e)));
        slot.valid.set(failed + 1);
    }
}

/// A pulled site's read-ahead window (extensibility.md §4.2.1 "In-hand and pulled sites"): rows read
/// ahead of replay from a source whose pull is not a metered event, plus the site's batches over
/// them. The site hands rows out one at a time with [`ReadAhead::next`] and evaluates each through its
/// unchanged per-row pipeline.
pub(crate) struct ReadAhead {
    /// The current window, in source order.
    pub(crate) rows: Vec<Row>,
    /// The window index of the next row to hand out.
    pos: usize,
    /// A source error met while filling the window — raised once every row read before it has been
    /// handed out (exactly where the batch-of-one pull would have raised it), dropped if replay
    /// stops first.
    deferred: Option<EngineError>,
    /// The source reported its end.
    ended: bool,
    /// The site's `WHERE` prefetch, if its predicate has an eligible call.
    pub(crate) filter: Option<HostBatch>,
    /// The site's projection prefetch, if a projection item has an eligible call.
    pub(crate) project: Option<HostBatch>,
}

impl ReadAhead {
    /// The window for a site evaluating `filter` (if any) and `project` per row, or `None` when
    /// neither has an eligible call — the site then pulls row by row as before.
    pub(crate) fn for_site(
        filter: Option<&RExpr>,
        project: &[RExpr],
        ext: &crate::extension::ExtensionRegistry,
    ) -> Option<Self> {
        let filter = filter.and_then(|f| HostBatch::for_exprs(std::slice::from_ref(f), ext));
        let project = HostBatch::for_exprs(project, ext);
        (filter.is_some() || project.is_some()).then(|| ReadAhead {
            rows: Vec::new(),
            pos: 0,
            deferred: None,
            ended: false,
            filter,
            project,
        })
    }

    /// Point the window at a new source (the next interval of an interval-set scan).
    pub(crate) fn restart(&mut self) {
        self.rows.clear();
        self.pos = 0;
        self.deferred = None;
        self.ended = false;
    }

    /// The window index of the next source row, refilling the window with up to `cap()` rows from
    /// `pull` once it is spent (`cap` is asked only then); `None` at the source's end; or the deferred source error once every
    /// row read before it has been handed out.
    pub(crate) fn next(
        &mut self,
        cap: impl FnOnce() -> usize,
        mut pull: impl FnMut() -> Result<Option<Row>>,
    ) -> Result<Option<usize>> {
        if self.pos == self.rows.len() {
            if let Some(e) = self.deferred.take() {
                return Err(e);
            }
            if self.ended {
                return Ok(None);
            }
            self.rows.clear();
            self.pos = 0;
            for b in [&self.filter, &self.project].into_iter().flatten() {
                b.reset();
            }
            let cap = cap().max(1);
            while self.rows.len() < cap {
                match pull() {
                    Ok(Some(row)) => self.rows.push(row),
                    Ok(None) => {
                        self.ended = true;
                        break;
                    }
                    Err(e) => {
                        self.deferred = Some(e);
                        break;
                    }
                }
            }
            if self.rows.is_empty() {
                return match self.deferred.take() {
                    Some(e) => Err(e),
                    None => Ok(None),
                };
            }
        }
        self.pos += 1;
        Ok(Some(self.pos - 1))
    }

    /// Make window row `i` current for `batch` (one of this window's), prefetching from it if needed.
    pub(crate) fn set_row(&self, batch: &HostBatch, i: usize, env: &EvalEnv, meter: &Meter) {
        batch.set_row(i, &self.rows, self.rows.len(), env, meter);
    }
}

/// The read-ahead window size (extensibility.md §4.2.1): at most [`READ_AHEAD_ROWS`]; on a metered
/// handle at most the rows the remaining budget could pay `unit` (the per-row charge replay makes
/// first) for, plus the one that trips it; and at most `remaining`, the rows the site can still emit.
pub(crate) fn read_ahead_cap(meter: &Meter, unit: i64, remaining: Option<i64>) -> usize {
    let mut n = READ_AHEAD_ROWS;
    if let Some(r) = remaining {
        n = n.min(r.max(1) as usize);
    }
    if let Some(h) = meter.headroom()
        && unit > 0
    {
        n = n.min((h.max(0) / unit) as usize + 1);
    }
    n
}
