//! Host-function batch prefetch (spec/design/extensibility.md §4.2.1) — speculative batch, scalar
//! replay. An evaluation site that already holds a chunk of rows asks [`HostBatch::prefetch`] to call
//! each eligible host function ONCE over the chunk's argument columns, then replays its ordinary
//! row-at-a-time evaluation with the batch installed on the [`EvalEnv`]. The `HostFunc` eval arm still
//! charges, guards, and type-checks per row exactly as before, and evaluates its arguments and
//! short-circuits NULL for any row the batch cannot answer; for a prefetched row it takes the kernel's
//! outcome from the cache, skipping the argument evaluation that would charge nothing, raise nothing,
//! and not short-circuit. So rows, cost, abort points, and errors are identical to batch-of-one by
//! construction.

use super::*;
use std::cell::{Cell, RefCell};

/// The largest chunk a site prefetches (extensibility.md §4.2.1).
pub(crate) const HOST_BATCH_ROWS: usize = 1024;

/// One eligible call node's prefetched outcomes over the site's current chunk.
struct Slot {
    /// The call node's identity (its address in the plan the site evaluates) — compared, never
    /// dereferenced. A node outside this site's expression list (a subquery's) never matches.
    node: usize,
    /// Indexes into `exprs` of the call node, so a prefetch can reach it.
    expr: usize,
    /// The site row index of `outcomes[0]`.
    base: usize,
    /// Per chunk row: the kernel's outcome, or `None` (not prefetched — a NULL / unevaluable argument,
    /// a row past a reported error, or a row outside the chunk). Taken (moved out) on replay.
    outcomes: RefCell<Vec<Option<Result<Value>>>>,
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

impl HostBatch {
    /// The batch for a site that evaluates every expression of `exprs` once per row, or `None` when
    /// none is eligible (§4.2.1): an item that is itself a call to a non-`volatile` host function
    /// whose arguments are all trivially evaluable.
    pub(crate) fn for_exprs(
        exprs: &[RExpr],
        ext: &crate::extension::ExtensionRegistry,
    ) -> Option<Self> {
        let slots: Vec<Slot> = exprs
            .iter()
            .enumerate()
            .filter(|(_, e)| match e {
                RExpr::HostFunc { id, args, .. } => {
                    ext.function(*id).batchable() && args.iter().all(trivially_evaluable)
                }
                _ => false,
            })
            .map(|(k, e)| Slot {
                node: e as *const RExpr as usize,
                expr: k,
                base: 0,
                outcomes: RefCell::new(Vec::new()),
            })
            .collect();
        (!slots.is_empty()).then(|| HostBatch {
            row: Cell::new(0),
            slots,
        })
    }

    /// Make row `i` of the site current. When a slot's chunk does not cover it, prefetch a new chunk
    /// starting at `i` over `rows[i..end]` (at most [`HOST_BATCH_ROWS`] rows, further capped by the
    /// meter's headroom).
    pub(crate) fn set_row(
        &mut self,
        i: usize,
        rows: &[Row],
        end: usize,
        exprs: &[RExpr],
        env: &EvalEnv,
        meter: &Meter,
    ) {
        self.row.set(i);
        let headroom = meter.headroom();
        for slot in &mut self.slots {
            let covered = i >= slot.base && i < slot.base + slot.outcomes.get_mut().len();
            if !covered {
                prefetch(slot, i, rows, end, &exprs[slot.expr], env, headroom);
            }
        }
    }

    /// The prefetched outcome of call node `node` for the current row, or `None` when the batch
    /// cannot answer it (the caller then calls the kernel for this row alone).
    pub(crate) fn take(&self, node: &RExpr) -> Option<Result<Value>> {
        let addr = node as *const RExpr as usize;
        let slot = self.slots.iter().find(|s| s.node == addr)?;
        let i = self.row.get().checked_sub(slot.base)?;
        slot.outcomes.borrow_mut().get_mut(i)?.take()
    }
}

/// Prefetch one slot's chunk starting at site row `start`.
fn prefetch(
    slot: &mut Slot,
    start: usize,
    rows: &[Row],
    end: usize,
    call: &RExpr,
    env: &EvalEnv,
    headroom: Option<i64>,
) {
    let RExpr::HostFunc { id, args, .. } = call else {
        unreachable!("a batch slot is a host call")
    };
    let hf = env.exec.session.extensions.function(*id);
    let mut n = (end - start).min(HOST_BATCH_ROWS);
    // Every replayed row charges at least the declared cost, so the kernel never runs on more rows
    // than the remaining budget could pay for, plus the one that trips it (§4.2.1).
    if let Some(h) = headroom
        && hf.cost > 0
    {
        n = n.min((h.max(0) / hf.cost) as usize + 1);
    }
    slot.base = start;
    let outcomes = slot.outcomes.get_mut();
    outcomes.clear();
    outcomes.resize_with(n, || None);
    // Gather the argument columns over the chunk rows whose arguments are all non-NULL; `members`
    // maps each batch row back to its chunk row.
    let mut cols: Vec<Vec<Value>> = (0..args.len()).map(|_| Vec::with_capacity(n)).collect();
    let mut members = Vec::with_capacity(n);
    let mut scratch = Meter::with_limit(0);
    'rows: for (k, row) in rows[start..start + n].iter().enumerate() {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            match a.eval(row, env, &mut scratch) {
                Ok(Value::Null) | Err(_) => continue 'rows,
                Ok(v) => vals.push(v),
            }
        }
        for (col, v) in cols.iter_mut().zip(vals) {
            col.push(v);
        }
        members.push(k);
    }
    if members.is_empty() {
        return;
    }
    for (k, outcome) in members.iter().zip(hf.call_batch(&cols, members.len())) {
        outcomes[*k] = outcome;
    }
}
